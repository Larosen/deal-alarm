"""Filial-Scanner: fragt den Preis eines Produkts in jeder Filiale einzeln ab.

Damit auch Shops mit strengem Rate-Limit vollständig abgedeckt werden, merkt sich der
Scanner pro Gerät und Filiale den letzten Preis. Pro Zyklus gilt ein Budget an Anfragen:

1. Die günstigsten bekannten Angebote (bis zur Hälfte des Budgets) werden jedes Mal neu
   geprüft, damit ein Deal nicht auf veralteten Daten beruht.
2. Danach kommen nie geprüfte Filialen,
3. danach die am längsten nicht geprüften.

So wandert der Scan über mehrere Zyklen durch alle Filialen, während die interessanten
jedes Mal aktuell sind. Bricht ein Scan wegen Drosselung ab, bleibt alles bisher
Gesammelte erhalten und der nächste Zyklus macht weiter.
"""

import concurrent.futures
import datetime
import logging

from .ratelimit import RateLimited

log = logging.getLogger(__name__)


def now_iso():
    return datetime.datetime.now().astimezone().isoformat(timespec="seconds")


def age_hours(iso):
    if not iso:
        return float("inf")
    then = datetime.datetime.fromisoformat(iso)
    return (datetime.datetime.now().astimezone() - then).total_seconds() / 3600


def plan(stores, store_state, budget=None):
    """Reihenfolge der zu prüfenden Filialen für diesen Zyklus."""
    if not budget or budget >= len(stores):
        budget = None
    known, never, rest = [], [], []
    for store in stores:
        entry = store_state.get(store["id"])
        if entry is None:
            never.append(store)
        elif entry.get("offer"):
            known.append((entry["offer"]["total_price"], entry.get("checked") or "", store))
        else:
            rest.append((entry.get("checked") or "", store))
    known.sort(key=lambda x: x[0])
    # Höchstens die Hälfte des Budgets für die günstigsten bekannten Angebote,
    # der Rest rotiert durch ungeprüfte und am längsten nicht geprüfte Filialen
    hot_count = len(known) if budget is None else min(len(known), max(1, budget // 2))
    hot = [s for _, _, s in known[:hot_count]]
    rest += [(checked, s) for _, checked, s in known[hot_count:]]
    order = hot + never + [s for _, s in sorted(rest, key=lambda x: x[0])]
    return order[:budget] if budget else order


def run(stores, store_state, fetch, budget=None, workers=1, progress=None):
    """Prüft die geplanten Filialen. `fetch(store)` liefert ein Angebot oder None (nicht verfügbar).

    `progress(stats, store_state, best)` wird nach jeder Filiale aufgerufen.
    Liefert (neuer store_state, Statistik).
    """
    store_state = {sid: entry for sid, entry in store_state.items() if sid in {s["id"] for s in stores}}
    queue = plan(stores, store_state, budget)
    stats = {"planned": len(queue), "checked": 0, "failed": 0, "aborted": False, "offers": 0}
    best = None

    def task(store):
        try:
            return store, fetch(store), None
        except RateLimited as exc:
            return store, None, exc
        except Exception as exc:
            return store, None, exc

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, workers)) as pool:
        for store, offer, error in pool.map(task, queue):
            if isinstance(error, RateLimited):
                if not stats["aborted"]:
                    log.warning("Scan wird gedrosselt und abgebrochen: %s – Fortsetzung im nächsten Lauf", error)
                stats["aborted"] = True
            elif error is not None:
                stats["failed"] += 1
                # Die ersten Fehler sichtbar machen, danach nur noch zählen
                if stats["failed"] <= 3:
                    log.warning("%s: Abfrage fehlgeschlagen (%s)", store["name"], error)
            else:
                stats["checked"] += 1
                store_state[store["id"]] = {
                    "name": store["name"],
                    "checked": now_iso(),
                    "coordinates": store.get("coordinates"),
                    "offer": offer,
                }
                if offer:
                    stats["offers"] += 1
                    if best is None or offer["total_price"] < best["total_price"]:
                        best = {**offer, "store_name": store["name"]}
            if progress:
                progress(stats, store_state, best)
    stats["total"] = len(stores)
    stats["known"] = len(store_state)
    return store_state, stats


def fresh_offers(store_state, max_age_hours):
    """Alle Angebote, die nicht älter als `max_age_hours` sind."""
    offers = []
    for store_id, entry in store_state.items():
        if store_id.startswith("_"):
            continue  # Metadaten wie _webcode
        if entry.get("offer") and age_hours(entry.get("checked")) <= max_age_hours:
            offers.append({**entry["offer"], "store_id": store_id, "store_name": entry["name"],
                           "checked": entry["checked"], "coordinates": entry.get("coordinates")})
    return sorted(offers, key=lambda o: o["total_price"])
