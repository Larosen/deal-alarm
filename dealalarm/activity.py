"""Protokoll und Fortschritt für die Weboberfläche.

Alle Log-Meldungen des Deal-Alarms landen zusätzlich in einem Ringpuffer, den die
Oberfläche unter „Protokoll“ anzeigt (dieselben Zeilen stehen im Docker-Log).
Laufende Scans melden ihren Fortschritt hier, damit Karte und Statusleiste ihn zeigen.
"""

import collections
import datetime
import itertools
import logging
import threading
import time

LOG_LIMIT = 500

_lock = threading.Lock()
_lines = collections.deque(maxlen=LOG_LIMIT)
_seq = itertools.count(1)
_progress = {}


class _RingHandler(logging.Handler):
    def emit(self, record):
        try:
            message = record.getMessage()
            if record.exc_info:
                message += f" ({record.exc_info[1]})"
            entry = {
                "id": next(_seq),
                "time": datetime.datetime.fromtimestamp(record.created).astimezone().isoformat(timespec="seconds"),
                "level": record.levelname,
                "message": message,
            }
            with _lock:
                _lines.append(entry)
        except Exception:
            self.handleError(record)


_handler = _RingHandler(level=logging.INFO)


def install():
    """Hängt den Ringpuffer an den Logger des Deal-Alarms (einmalig)."""
    logger = logging.getLogger("dealalarm")
    if _handler not in logger.handlers:
        logger.addHandler(_handler)
    if logger.level == logging.NOTSET or logger.level > logging.INFO:
        logger.setLevel(logging.INFO)


def recent(after=0, limit=200):
    with _lock:
        lines = [line for line in _lines if line["id"] > after]
    return lines[-limit:]


def set_progress(shop, **values):
    with _lock:
        _progress[shop] = {**_progress.get(shop, {}), **values}


def clear_progress(shop):
    with _lock:
        _progress.pop(shop, None)


def progress():
    with _lock:
        return {shop: dict(values) for shop, values in _progress.items()}


class ScanReporter:
    """Fortschrittsmeldungen eines Shop-Scans für ein Gerät.

    Wird von scan.run nach jeder Filiale aufgerufen, protokolliert aber nur alle
    `every` Filialen bzw. spätestens alle `interval` Sekunden.
    """

    def __init__(self, logger, shop, shop_label, watch_name, on_save=None, every=25, interval=60):
        self.log = logger
        self.shop = shop
        self.shop_label = shop_label
        self.watch_name = watch_name
        self.on_save = on_save
        self.every = every
        self.interval = interval
        self._last_log = time.monotonic()
        self._last_count = 0

    def __call__(self, stats, store_state, best=None):
        done = stats["checked"] + stats["failed"]
        set_progress(self.shop, done=done, planned=stats["planned"], offers=stats.get("offers", 0),
                     best_price=best["total_price"] if best else None,
                     best_store=best["store_name"] if best else None,
                     throttled=stats.get("aborted", False))
        due = done - self._last_count >= self.every or time.monotonic() - self._last_log >= self.interval
        if not due and done < stats["planned"]:
            return
        self._last_log = time.monotonic()
        self._last_count = done
        best_text = f", bisher günstigster {best['total_price']:.2f} € ({best['store_name']})" if best else ""
        self.log.info("%s · %s: %d/%d Filialen geprüft, %d mit Angebot%s",
                      self.shop_label, self.watch_name, done, stats["planned"], stats.get("offers", 0), best_text)
        if self.on_save:
            self.on_save(store_state)
