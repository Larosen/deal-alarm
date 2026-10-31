"""Preisabfrage bei expert – basierend auf expert-checker reborn (MyDealz @Barney & @Gorex)."""

import json
import os
import re
import time

from .. import ratelimit, scan
from .common import haversine_km, http_session, plz_coordinates

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


def resolve(query):
    """Löst URL, Webcode oder Suchbegriff (erster Treffer) zu Produktinfos auf."""
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


def check(watch, settings, store_state):
    """Fragt jede Filiale einzeln ab und liefert (Ergebnis, neuer Filialstatus)."""
    product = resolve(watch["expert_query"])
    webcode = product["webcode"]
    # Filialstatus gilt nur für dieses Produkt
    if store_state.get("_webcode") != webcode:
        store_state = {}
    stores_state = {k: v for k, v in store_state.items() if not k.startswith("_")}

    limiter = ratelimit.get_limiter("expert", settings.get("expert_rate_per_min") or 300)
    limiter.start_scan()
    session = _session()

    def fetch(store):
        response = ratelimit.request(session, limiter, "GET", f"{API}/api/pricepds",
                                     params={"webcode": webcode, "storeId": store["exp_id"]})
        if response.status_code == 404:
            return None
        response.raise_for_status()
        return parse_offer(response.json())

    stores = list_stores(watch.get("expert_all_branches", True))
    budget = int(settings.get("expert_stores_per_cycle") or 0) or None
    stores_state, stats = scan.run(stores, stores_state, fetch, budget=budget,
                                   workers=int(settings.get("expert_workers") or 8))

    offers = filter_offers(scan.fresh_offers(stores_state, float(settings.get("price_max_age_hours") or 24)), watch, settings)
    for offer in offers:
        offer["url"] = store_url(product["url"], offer["store_id"])
    best = offers[0] if offers else None
    result = {
        "title": product["title"],
        "url": best["url"] if best else product["url"],
        "ean": product["ean"],
        "price": best["total_price"] if best else None,
        "available": best is not None,
        "detail": _describe(best, len(offers)),
        "scan": stats,
    }
    return result, {**stores_state, "_webcode": webcode, "_product_url": product["url"]}


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
