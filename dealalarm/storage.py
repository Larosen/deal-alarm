"""Persistente Ablage von Einstellungen und Watchlist als JSON-Datei."""

import copy
import json
import os
import threading
import uuid

DATA_DIR = os.environ.get("DATA_DIR", os.path.join(os.path.dirname(os.path.dirname(__file__)), "data"))
DATA_FILE = os.path.join(DATA_DIR, "state.json")
HISTORY_LIMIT = 100

DEFAULT_SETTINGS = {
    "ntfy_server": "https://ntfy.sh",
    "ntfy_topic": "",
    "ntfy_token": "",
    "ntfy_priority": 4,
    "interval_minutes": 120,
    "jitter_minutes": 10,
    "plz": "",
    "scheduler_enabled": True,
    # Abfragetempo pro Shop
    "expert_rate_per_min": 300,
    "expert_workers": 8,
    "expert_stores_per_cycle": 0,      # 0 = alle Filialen in jedem Zyklus
    "euronics_rate_per_min": 6,        # gilt je Domain (euronics.de, mediaathome.de)
    "euronics_stores_per_cycle": 0,    # 0 = alle Märkte in jedem Zyklus
    "euronics_stores": "",             # Markt-URLs, eine pro Zeile
    "price_max_age_hours": 24,         # ältere Filialpreise zählen nicht mehr für den Dealalarm
}

DEFAULT_WATCH = {
    "name": "",
    "target_price": 0.0,
    "enabled": True,
    # expert
    "expert_enabled": True,
    "expert_query": "",          # URL, Webcode oder Artikelnummer
    "expert_all_branches": True,  # alle Filialen statt nur Onlineshop
    "expert_include_display": False,
    "expert_include_local": False,
    "expert_max_distance": 50,
    # Euronics
    "euronics_enabled": True,
    "euronics_query": "",        # URL oder EAN/Suchbegriff
    "euronics_all_stores": True,  # alle Märkte aus der Marktliste
    "ean": "",
}

_lock = threading.RLock()


def _empty_state():
    return {"settings": dict(DEFAULT_SETTINGS), "watches": [], "scheduler": {}}


def _load():
    if not os.path.exists(DATA_FILE):
        return _empty_state()
    with open(DATA_FILE, "r", encoding="utf-8") as f:
        state = json.load(f)
    state.setdefault("watches", [])
    state.setdefault("scheduler", {})
    state["settings"] = {**DEFAULT_SETTINGS, **state.get("settings", {})}
    return state


def _save(state):
    os.makedirs(DATA_DIR, exist_ok=True)
    tmp = DATA_FILE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, ensure_ascii=False, indent=2)
    os.replace(tmp, DATA_FILE)


def get_state():
    with _lock:
        return copy.deepcopy(_load())


def get_settings():
    return get_state()["settings"]


def update_settings(values):
    with _lock:
        state = _load()
        for key in DEFAULT_SETTINGS:
            if key in values:
                state["settings"][key] = values[key]
        _save(state)
        return copy.deepcopy(state["settings"])


def list_watches():
    return get_state()["watches"]


def get_watch(watch_id):
    return next((w for w in list_watches() if w["id"] == watch_id), None)


def add_watch(values):
    with _lock:
        state = _load()
        watch = {**DEFAULT_WATCH, **{k: v for k, v in values.items() if k in DEFAULT_WATCH}}
        watch["id"] = uuid.uuid4().hex[:12]
        watch["results"] = {}
        watch["history"] = []
        state["watches"].append(watch)
        _save(state)
        return copy.deepcopy(watch)


def update_watch(watch_id, values):
    with _lock:
        state = _load()
        for watch in state["watches"]:
            if watch["id"] == watch_id:
                for key in DEFAULT_WATCH:
                    if key in values:
                        watch[key] = values[key]
                _save(state)
                return copy.deepcopy(watch)
        return None


def delete_watch(watch_id):
    with _lock:
        state = _load()
        before = len(state["watches"])
        state["watches"] = [w for w in state["watches"] if w["id"] != watch_id]
        _save(state)
        return len(state["watches"]) < before


def save_result(watch_id, shop, result, history_entry=None, store_state=None):
    """Speichert das Prüfergebnis eines Shops. Liefert das bisherige Ergebnis zurück."""
    with _lock:
        state = _load()
        for watch in state["watches"]:
            if watch["id"] == watch_id:
                previous = watch.setdefault("results", {}).get(shop)
                watch["results"][shop] = result
                if store_state is not None:
                    watch.setdefault("store_prices", {})[shop] = store_state
                if history_entry:
                    watch.setdefault("history", []).append(history_entry)
                    watch["history"] = watch["history"][-HISTORY_LIMIT:]
                _save(state)
                return previous
        return None


def set_watch_field(watch_id, key, value):
    with _lock:
        state = _load()
        for watch in state["watches"]:
            if watch["id"] == watch_id:
                watch[key] = value
                _save(state)
                return


def update_scheduler(values):
    with _lock:
        state = _load()
        state["scheduler"].update(values)
        _save(state)


def get_scheduler():
    return get_state()["scheduler"]
