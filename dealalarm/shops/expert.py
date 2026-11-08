"""Preisabfrage bei expert – basierend auf expert-checker reborn (MyDealz @Barney & @Gorex)."""

import json
import logging
import os
import re
import time

from .. import ratelimit, scan
from .common import haversine_km, http_session, plz_coordinates

log = logging.getLogger(__name__)

API = "https://production.brntgs.expert.de"
ONLINE_STORE = "e_2879130"
BRANCH_BACKUP = os.path.join(os.path.dirname(__file__), "expert_branches.json")
BRANCH_CACHE_SECONDS = 24 * 3600

_branch_cache = {"time": 0, "branches": None}


def _session():
    s = http_session()
    s.headers["Accept"] = "application/json"
    return s


def search(term):
    """Liefert Suchvorschläge (Titel, Webcode, URL, EAN) für einen Suchbegriff."""
    response = _session().get(f"{API}/api/search/suggest", params={"q": term, "storeId": ONLINE_STORE}, timeout=20)
    response.raise_for_status()
    results = []
    for entry in response.json().get("articleSuggest", []):
        article = entry["article"]
        results.append(_article_info(article))
    return results


def _article_info(article):
    title = article.get("seoPageTitle") or article.get("title") or ""
    return {
        "title": title.split(" - bei expert kaufen")[0],
        "webcode": article.get("webcode"),
        "article_id": article.get("articleId"),
        "ean": article.get("internationalArticleNumber") or "",
        "url": "https://www.expert.de" + article["link"] if article.get("link") else "",
    }


def extract_webcode(query):
    """Webcode aus einer expert-URL oder direkter Eingabe ermitteln."""
    query = query.strip()
    if "expert.de" in query:
        slug = query.split("?")[0].split("#")[0].rstrip("/").split("/")[-1]
        match = re.match(r"(\d+)", slug)
        return match.group(1) if match else None
    if query.isdigit():
        return query
    return None


def find_by_ean(ean):
    """Artikel zur EAN oder None, wenn expert ihn nicht führt."""
    response = _session().get(f"{API}/api/search/article", params={"ean": ean}, timeout=20)
    if response.status_code == 204:
        return None
    response.raise_for_status()
    data = response.json()
    return _article_info(data) if data and data.get("webcode") else None


def is_ean(value):
    value = (value or "").strip()
    return value.isdigit() and len(value) in (8, 12, 13, 14)


def resolve(query):
    """Löst URL, Webcode, EAN oder Suchbegriff (erster Treffer) zu Produktinfos auf."""
    if is_ean(query):
        product = find_by_ean(query.strip())
        if not product:
            raise LookupError(f"EAN {query.strip()} gibt es bei expert nicht")
        return product
    webcode = extract_webcode(query)
    if webcode is None:
        hits = search(query)
        if not hits:
            raise LookupError(f"Kein expert-Produkt für '{query}' gefunden")
        return hits[0]
    response = _session().get(f"{API}/api/search/article", params={"webcode": webcode}, timeout=20)
    response.raise_for_status()
    data = response.json()
    if not data or not data.get("webcode"):
        raise LookupError(f"Webcode {webcode} bei expert unbekannt")
    return _article_info(data)


def find_listings(product):
    """Alle Webcodes, unter denen expert denselben Artikel (gleiche EAN) führt – {webcode: URL}.

    expert listet einen Artikel teils mehrfach mit unterschiedlichen Preisen.
    """
    listings = {product["webcode"]: product["url"]}
    if not product.get("ean"):
        return listings
    try:
        for hit in search(product["title"]):
            if hit["ean"] == product["ean"] and hit["webcode"] and hit["url"]:
                listings.setdefault(hit["webcode"], hit["url"])
    except Exception:
        pass  # Zusatzsuche ist optional
    return listings


def get_branches():
    """Alle expert-Filialen (24h gecacht, lokales Backup als Fallback)."""
    if _branch_cache["branches"] and time.time() - _branch_cache["time"] < BRANCH_CACHE_SECONDS:
        return _branch_cache["branches"]
    params = {
        "lat": 0,
        "lng": 0,
        "maxResults": 500,
        "device": "Mozilla/5.0",
        "source": "HTML5",
        "withWebsite": True,
        "conditions": {"storeFinderResultFilter": "ALL"},
    }
    try:
        response = http_session().post(
            f"{API}/_api/storeFinder/searchStoresByGeoLocation",
            json=params,
            cookies={"fmarktcookie": ONLINE_STORE},
            timeout=30,
        )
        response.raise_for_status()
        branches = response.json()
        if not isinstance(branches, list) or not branches:
            raise ValueError("Leere Filialliste")
    except Exception:
        with open(BRANCH_BACKUP, "r", encoding="utf-8") as f:
            branches = json.load(f)
    _branch_cache.update(time=time.time(), branches=branches)
    return branches


def list_stores(all_branches=True):
    """Alle zu prüfenden Filialen im einheitlichen Scanner-Format."""
    stores = []
    if all_branches:
        for branch in get_branches():
            store = branch["store"]
            name = store.get("name", "")
            if store.get("city") and store["city"] not in name:
                name = f"{name} {store['city']}"
            coords = (store["latitude"], store["longitude"]) if store.get("latitude") else None
            stores.append({"id": store["id"], "exp_id": store["expId"], "name": name, "coordinates": coords})
    stores.append({"id": ONLINE_STORE, "exp_id": ONLINE_STORE.split("_")[1], "name": "expert Onlineshop", "coordinates": None})
    return stores


def parse_offer(data):
    """Wandelt eine pricepds-Antwort in ein Angebot um (None = nicht verfügbar)."""
    price_info = (data or {}).get("price") or {}
    if not price_info.get("bruttoPrice"):
        return None
    promotion = data.get("promotionPrice") or {}
    price = float(promotion.get("checkoutPrice") or price_info["bruttoPrice"])
    online_stock = price_info.get("onlineStock", 0) or 0
    shipping = 0.0
    if online_stock > 0 and price_info.get("shipmentArray"):
        shipping = float(price_info["shipmentArray"][0].get("shipmentBruttoPrice") or 0)
    return {
        "price": round(price, 2),
        "shipping": round(shipping, 2),
        "total_price": round(price + shipping, 2),
        "online_stock": online_stock,
        "store_stock": price_info.get("storeStock", 0) or 0,
        "on_display": bool((price_info.get("itemOnDisplay") or {}).get("onDisplay")),
    }


def store_url(product_url, store_id):
    return product_url if store_id == ONLINE_STORE else f"{product_url}?branch_id={store_id}"


def filter_offers(offers, watch, settings):
    """Wendet die Filter des Geräts an (Ausstellungsstücke, nur lokal, Entfernung)."""
    user_coords = plz_coordinates(settings["plz"]) if watch.get("expert_include_local") and settings.get("plz") else None
    max_distance = float(watch.get("expert_max_distance") or 0) or float("inf")
    accepted = []
    for offer in offers:
        offer = dict(offer)
        if user_coords and offer.get("coordinates"):
            offer["distance_km"] = round(haversine_km(user_coords, offer["coordinates"]))
        if offer["on_display"] and not watch.get("expert_include_display"):
            continue
        if offer["online_stock"] <= 0:
            # nur lokal verfügbar: nur wenn gewünscht und in Reichweite
            if not user_coords or offer["store_stock"] <= 0 or offer.get("distance_km") is None:
                continue
            if offer["distance_km"] > max_distance:
                continue
        accepted.append(offer)
    return accepted


def check(watch, settings, store_state, progress=None):
    """Fragt jede Filiale einzeln ab und liefert (Ergebnis, neuer Filialstatus)."""
    query = (watch.get("expert_query") or "").strip()
    if query:
        product = resolve(query)
    else:
        # Artikel wurde über einen anderen Shop ausgewählt: per EAN zuordnen
        product = find_by_ean(watch["ean"])
        if product is None:
            return {
                "title": "", "url": "", "ean": watch["ean"], "price": None, "available": False,
                "detail": f"Nicht im expert-Sortiment (EAN {watch['ean']})",
            }, {}
    listings = find_listings(product)
    webcodes = sorted(listings)
    # Filialstatus gilt nur für diese Artikel
    if store_state.get("_webcodes") != webcodes:
        store_state = {}
    stores_state = {k: v for k, v in store_state.items() if not k.startswith("_")}

    limiter = ratelimit.get_limiter("expert", settings.get("expert_rate_per_min") or 300)
    limiter.start_scan()
    session = _session()

    def fetch(store):
        """Günstigstes Angebot der Filiale über alle Webcodes des Artikels."""
        best, errors = None, []
        for webcode in webcodes:
            response = ratelimit.request(session, limiter, "GET", f"{API}/api/pricepds",
                                         params={"webcode": webcode, "storeId": store["exp_id"]})
            if response.status_code == 404:
                continue
            try:
                response.raise_for_status()
                data = response.json()
            except Exception as exc:
                # expert antwortet für einzelne Artikelnummern teils mit 500 – die übrigen zählen trotzdem
                errors.append(exc)
                continue
            offer = parse_offer(data)
            if offer and (best is None or offer["total_price"] < best["total_price"]):
                offer["webcode"] = webcode
                offer["url"] = store_url(listings[webcode], store["id"])
                best = offer
        if errors and len(errors) == len(webcodes):
            raise errors[-1]
        return best

    stores = list_stores(watch.get("expert_all_branches", True))
    budget = int(settings.get("expert_stores_per_cycle") or 0) or None
    log.info("expert · %s: %s, %d Artikelnummer(n) %s, %d Filialen",
             watch.get("name"), product["title"], len(webcodes), ", ".join(webcodes), len(stores))
    stores_state, stats = scan.run(stores, stores_state, fetch, budget=budget,
                                   workers=int(settings.get("expert_workers") or 8), progress=progress,
                                   accept=lambda offer: bool(filter_offers([offer], watch, settings)))

    offers = filter_offers(scan.fresh_offers(stores_state, float(settings.get("price_max_age_hours") or 24)), watch, settings)
    best = offers[0] if offers else None
    result = {
        "title": product["title"],
        "url": best["url"] if best else product["url"],
        "ean": product["ean"],
        "price": best["total_price"] if best else None,
        "available": best is not None,
        "detail": _describe(best, len(offers)) + (f" · {len(webcodes)} expert-Artikelnummern" if len(webcodes) > 1 else ""),
        "scan": stats,
    }
    return result, {**stores_state, "_webcodes": webcodes, "_product_url": product["url"]}


def _describe(best, count):
    if not best:
        return "In keiner Filiale verfügbar"
    parts = [best["store_name"]]
    if best["online_stock"] > 0:
        parts.append("versandfähig")
    else:
        parts.append(f"nur lokal ({best.get('distance_km', '?')} km)")
    if best["on_display"]:
        parts.append("Ausstellungsstück")
    if count > 1:
        parts.append(f"{count} passende Angebote")
    return ", ".join(parts)
