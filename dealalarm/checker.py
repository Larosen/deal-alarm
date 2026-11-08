"""Prüft alle Watches, vergleicht mit dem Wunschpreis und löst Dealalarme aus."""

import datetime
import logging
import random
import threading
import time

from . import activity, notifier, storage
from .shops import euronics, expert

log = logging.getLogger(__name__)

SHOPS = {
    "expert": ("expert", expert.check),
    "euronics": ("Euronics", euronics.check),
}

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
    return notifier.send(
        settings,
        title=f"Dealalarm: {name} bei {shop_label}",
        message=message.strip(),
        click=result.get("url"),
        tags=["rotating_light", "moneybag"],
    )


def check_shop(watch, shop, settings):
    """Prüft ein Gerät bei einem Shop, speichert das Ergebnis und löst ggf. den Dealalarm aus."""
    shop_label, check_fn = SHOPS[shop]
    name = watch.get("name") or "Gerät"
    previous = (watch.get("results") or {}).get(shop)
    store_state = (watch.get("store_prices") or {}).get(shop) or {}
    reporter = activity.ScanReporter(log, shop, shop_label, name,
                                     on_save=lambda state: storage.save_store_state(watch["id"], shop, state))
    started = time.monotonic()
    log.info("%s · %s: Prüfung gestartet", shop_label, name)
    try:
        result, store_state = check_fn(watch, settings, store_state, progress=reporter)
    except Exception as exc:  # einzelne Shopfehler sollen den Lauf nicht abbrechen
        log.warning("%s · %s: Fehler – %s", shop_label, name, exc)
        result = {"error": str(exc), "price": None, "available": False}
    result["checked_at"] = _now()

    if result.get("ean") and not watch.get("ean"):
        watch["ean"] = result["ean"]
        storage.set_watch_field(watch["id"], "ean", result["ean"])

    alert = evaluate(watch, shop, result, previous)
    if alert:
        try:
            message_id = notify_deal(settings, watch, shop, result)
            result["notified_at"] = result["checked_at"]
            log.info("%s · %s: Dealalarm gesendet (%.2f €, ntfy-ID %s)", shop_label, name, result["price"], message_id)
        except Exception as exc:
            log.error("%s · %s: ntfy-Versand fehlgeschlagen – %s", shop_label, name, exc)
            result["notify_error"] = str(exc)
            # Beim nächsten Lauf erneut versuchen
            result["alerted"] = False
            result["alert_price"] = None
    history = None
    if result.get("price") is not None:
        history = {"time": result["checked_at"], "shop": shop, "price": result["price"]}
    storage.save_result(watch["id"], shop, result, history, store_state)

    minutes = (time.monotonic() - started) / 60
    if not result.get("error"):
        scan_info = result.get("scan") or {}
        price = f"{result['price']:.2f} €" if result.get("price") is not None else "kein Angebot"
        log.info("%s · %s: fertig nach %.1f Min. – %s (%s)%s", shop_label, name, minutes, price,
                 result.get("detail", ""), " · gedrosselt, Rest folgt im nächsten Lauf" if scan_info.get("aborted") else "")
    return result


class ShopWorker(threading.Thread):
    """Arbeitet die Warteschlange eines Shops ab – unabhängig von den anderen Shops.

    So muss expert nicht warten, bis der langsame Euronics-Scan fertig ist, und
    ein neu angelegtes Gerät wird sofort bei jedem Shop eingereiht.
    """

    def __init__(self, shop):
        super().__init__(daemon=True, name=f"worker-{shop}")
        self.shop = shop
        self._queue = []
        self._cond = threading.Condition()
        self.current = None

    def enqueue(self, watch_id, front=False):
        with self._cond:
            if watch_id == self.current or watch_id in self._queue:
                if front and watch_id in self._queue:
                    self._queue.remove(watch_id)
                    self._queue.insert(0, watch_id)
                return
            if front:
                self._queue.insert(0, watch_id)
            else:
                self._queue.append(watch_id)
            self._cond.notify()

    def snapshot(self):
        with self._cond:
            return {"current": self.current, "queue": list(self._queue)}

    @property
    def busy(self):
        with self._cond:
            return self.current is not None or bool(self._queue)

    def run(self):
        while True:
            with self._cond:
                while not self._queue:
                    self._cond.wait()
                self.current = self._queue.pop(0)
            try:
                watch = storage.get_watch(self.current)
                if watch and _shop_active(watch, self.shop):
                    activity.set_progress(self.shop, watch_id=watch["id"], watch_name=watch.get("name"),
                                          started=_now(), done=0, planned=None)
                    check_shop(watch, self.shop, storage.get_settings())
            except Exception:
                log.exception("%s: Prüfung abgebrochen", SHOPS[self.shop][0])
            finally:
                activity.clear_progress(self.shop)
                with self._cond:
                    self.current = None
                _finish_if_idle()


_workers = {}
_workers_lock = threading.Lock()


def workers():
    with _workers_lock:
        if not _workers:
            for shop in SHOPS:
                worker = _workers[shop] = ShopWorker(shop)
                worker.start()
        return dict(_workers)


def _finish_if_idle():
    if not is_running():
        storage.update_scheduler({"last_end": _now()})
        log.info("Alle Prüfungen abgeschlossen")


def run_all(watch_id=None):
    """Reiht alle aktiven Geräte (oder ein bestimmtes, dann vorne) bei jedem Shop ein."""
    state = storage.get_state()
    watches = [w for w in state["watches"] if (w["id"] == watch_id if watch_id else w.get("enabled"))]
    if not watches:
        return False
    storage.update_scheduler({"last_start": _now()})
    for worker in workers().values():
        for watch in watches:
            worker.enqueue(watch["id"], front=bool(watch_id))
    log.info("Prüfung eingereiht: %s", ", ".join(w.get("name") or w["id"] for w in watches))
    return True


def is_running():
    with _workers_lock:
        return any(worker.busy for worker in _workers.values())


def status():
    """Was gerade geprüft wird und was wartet – pro Shop."""
    progress = activity.progress()
    with _workers_lock:
        current = dict(_workers)
    out = {}
    for shop in SHOPS:
        worker = current.get(shop)
        snap = worker.snapshot() if worker else {"current": None, "queue": []}
        out[shop] = {"label": SHOPS[shop][0], "current": snap["current"], "queue": snap["queue"],
                     "progress": progress.get(shop)}
    return out


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
        if storage.get_settings().get("scheduler_enabled"):
            log.info("Nächster automatischer Lauf: %s", next_run.strftime("%d.%m. %H:%M"))
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
