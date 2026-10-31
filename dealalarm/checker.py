"""Prüft alle Watches, vergleicht mit dem Wunschpreis und löst Dealalarme aus."""

import datetime
import logging
import random
import threading
import time

from . import notifier, storage
from .shops import euronics, expert

log = logging.getLogger(__name__)

SHOPS = {
    "expert": ("expert", expert.check),
    "euronics": ("Euronics", euronics.check),
}

_run_lock = threading.Lock()


def _now():
    return datetime.datetime.now().astimezone().isoformat(timespec="seconds")


def format_price(value):
    return f"{value:,.2f} €".replace(",", "X").replace(".", ",").replace("X", ".")


def _shop_active(watch, shop):
    if not watch.get(f"{shop}_enabled"):
        return False
    # Ohne eigene Angabe wird der Artikel im Shop über die EAN gesucht
    return bool(watch.get(f"{shop}_query") or watch.get("ean"))


def evaluate(watch, shop, result, previous):
    """Ermittelt, ob ein Alarm fällig ist, und ergänzt den Alarmstatus im Ergebnis."""
    previous = previous or {}
    if result.get("error"):
        # Fehler setzen den Alarmstatus nicht zurück, sonst käme nach jedem Aussetzer ein neuer Alarm
        result["alerted"] = previous.get("alerted", False)
        result["alert_price"] = previous.get("alert_price")
        return False
    price = result.get("price")
    target = float(watch.get("target_price") or 0)
    is_deal = price is not None and target > 0 and price <= target
    result["is_deal"] = is_deal
    if not is_deal:
        result["alerted"] = False
        result["alert_price"] = None
        return False
    last_alert = previous.get("alert_price") if previous.get("alerted") else None
    should_alert = last_alert is None or price < last_alert - 0.005
    result["alerted"] = True
    result["alert_price"] = price if should_alert else last_alert
    return should_alert


def notify_deal(settings, watch, shop, result):
    shop_label = SHOPS[shop][0]
    name = watch.get("name") or result.get("title") or "Gerät"
    message = (
        f"{result.get('title') or name}\n"
        f"Preis: {format_price(result['price'])} (Wunschpreis {format_price(float(watch['target_price']))})\n"
        f"{result.get('detail', '')}"
    )
    notifier.send(
        settings,
        title=f"Dealalarm: {name} bei {shop_label}",
        message=message.strip(),
        click=result.get("url"),
        tags=["rotating_light", "moneybag"],
    )


def check_watch(watch, settings):
    summary = {}
    for shop, (_, check_fn) in SHOPS.items():
        if not _shop_active(watch, shop):
            continue
        previous = (watch.get("results") or {}).get(shop)
        store_state = (watch.get("store_prices") or {}).get(shop) or {}
        try:
            result, store_state = check_fn(watch, settings, store_state)
        except Exception as exc:  # einzelne Shopfehler sollen den Lauf nicht abbrechen
            log.warning("%s / %s: %s", watch.get("name"), shop, exc)
            result = {"error": str(exc), "price": None, "available": False}
        result["checked_at"] = _now()

        if result.get("ean") and not watch.get("ean"):
            watch["ean"] = result["ean"]
            storage.set_watch_field(watch["id"], "ean", result["ean"])

        alert = evaluate(watch, shop, result, previous)
        if alert:
            try:
                notify_deal(settings, watch, shop, result)
                result["notified_at"] = result["checked_at"]
            except Exception as exc:
                log.error("ntfy-Versand fehlgeschlagen: %s", exc)
                result["notify_error"] = str(exc)
                # Beim nächsten Lauf erneut versuchen
                result["alerted"] = False
                result["alert_price"] = None
        history = None
        if result.get("price") is not None:
            history = {"time": result["checked_at"], "shop": shop, "price": result["price"]}
        storage.save_result(watch["id"], shop, result, history, store_state)
        summary[shop] = result
    return summary


def run_all(watch_id=None):
    """Prüft alle (oder eine) aktiven Watches. Liefert False, wenn schon ein Lauf aktiv ist."""
    if not _run_lock.acquire(blocking=False):
        return False
    try:
        storage.update_scheduler({"running": True, "last_start": _now()})
        state = storage.get_state()
        for watch in state["watches"]:
            if watch_id and watch["id"] != watch_id:
                continue
            if not watch_id and not watch.get("enabled"):
                continue
            check_watch(watch, state["settings"])
        storage.update_scheduler({"last_end": _now()})
        return True
    finally:
        storage.update_scheduler({"running": False})
        _run_lock.release()


def is_running():
    return _run_lock.locked()


class Scheduler(threading.Thread):
    """Startet die Prüfung zyklisch (Standard: alle 120 Minuten ± Zufallsanteil)."""

    def __init__(self):
        super().__init__(daemon=True, name="deal-scheduler")
        self._wake = threading.Event()

    def reschedule(self):
        """Nach Änderung der Einstellungen den nächsten Termin neu berechnen."""
        self._wake.set()

    def _next_delay(self, settings):
        interval = max(15, float(settings.get("interval_minutes") or 120))
        jitter = min(max(0, float(settings.get("jitter_minutes") or 0)), interval / 2)
        return (interval + random.uniform(-jitter, jitter)) * 60

    def _set_deadline(self, base):
        deadline = base + self._next_delay(storage.get_settings())
        next_run = datetime.datetime.fromtimestamp(deadline).astimezone()
        storage.update_scheduler({"next_run": next_run.isoformat(timespec="seconds")})
        return deadline

    def run(self):
        last_run = None
        deadline = time.time() + 30  # kurz nach dem Start einmal prüfen
        while True:
            remaining = deadline - time.time()
            if remaining > 0:
                if self._wake.wait(remaining):
                    self._wake.clear()
                    if last_run is not None:
                        deadline = self._set_deadline(last_run)
                continue
            last_run = time.time()
            if storage.get_settings().get("scheduler_enabled"):
                try:
                    run_all()
                except Exception:
                    log.exception("Prüflauf fehlgeschlagen")
            deadline = self._set_deadline(last_run)
