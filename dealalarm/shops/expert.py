"""Preisabfrage bei expert – basierend auf expert-checker reborn (MyDealz @Barney & @Gorex)."""

import concurrent.futures
import json
import os
import re
import time

from .common import haversine_km, http_session, plz_coordinates

API = "https://production.brntgs.expert.de"
ONLINE_STORE = "e_2879130"
BRANCH_BACKUP = os.path.join(os.path.dirname(__file__), "expert_branches.json")
BRANCH_CACHE_SECONDS = 24 * 3600
MAX_WORKERS = 8

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


def get_branch_product_data(session, webcode, store_id):
    delay = 2
    for attempt in range(5):
        try:
            response = session.get(f"{API}/api/pricepds", params={"webcode": webcode, "storeId": store_id}, timeout=20)
            if response.status_code == 429:
                time.sleep(delay)
                delay *= 2
                continue
            response.raise_for_status()
            return response.json()
        except Exception:
            if attempt == 4:
                raise
            time.sleep(delay)
            delay *= 2
    return None


def parse_offer(data, store_name, store_id, url, coordinates=None):
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
    on_display = bool((price_info.get("itemOnDisplay") or {}).get("onDisplay"))
    return {
        "store_id": store_id,
        "store_name": store_name,
        "price": round(price, 2),
        "shipping": round(shipping, 2),
        "total_price": round(price + shipping, 2),
        "online_stock": online_stock,
        "store_stock": price_info.get("storeStock", 0) or 0,
        "on_display": on_display,
        "url": url,
        "coordinates": coordinates,
    }


def check(watch, settings):
    """Durchsucht expert nach dem günstigsten passenden Angebot."""
    product = resolve(watch["expert_query"])
    webcode = product["webcode"]
    session = _session()

    if watch.get("expert_all_branches"):
        stores = []
        for branch in get_branches():
            store = branch["store"]
            name = store.get("name", "")
            if store.get("city") and store["city"] not in name:
                name = f"{name} {store['city']}"
            stores.append((store["expId"], store["id"], name, (store.get("latitude"), store.get("longitude"))))
        stores.append((ONLINE_STORE.split("_")[1], ONLINE_STORE, "expert Onlineshop", None))
    else:
        stores = [(ONLINE_STORE.split("_")[1], ONLINE_STORE, "expert Onlineshop", None)]

    user_coords = None
    if watch.get("expert_include_local") and settings.get("plz"):
        user_coords = plz_coordinates(settings["plz"])
    max_distance = float(watch.get("expert_max_distance") or 0) or float("inf")

    def fetch(store):
        exp_id, branch_id, name, coords = store
        try:
            data = get_branch_product_data(session, webcode, exp_id)
        except Exception:
            return None
        url = product["url"] + (f"?branch_id={branch_id}" if branch_id != ONLINE_STORE else "")
        return parse_offer(data, name, branch_id, url, coords)

    with concurrent.futures.ThreadPoolExecutor(max_workers=MAX_WORKERS) as pool:
        offers = [o for o in pool.map(fetch, stores) if o]

    def acceptable(offer):
        if offer["on_display"] and not watch.get("expert_include_display"):
            return False
        if offer["online_stock"] > 0:
            return True
        # Nur lokal verfügbar: nur wenn gewünscht und in Reichweite
        if not user_coords or offer["store_stock"] <= 0 or not offer["coordinates"]:
            return False
        distance = haversine_km(user_coords, offer["coordinates"])
        offer["distance_km"] = round(distance)
        return distance <= max_distance

    offers = sorted((o for o in offers if acceptable(o)), key=lambda o: o["total_price"])
    best = offers[0] if offers else None
    return {
        "title": product["title"],
        "url": best["url"] if best else product["url"],
        "ean": product["ean"],
        "price": best["total_price"] if best else None,
        "available": best is not None,
        "detail": _describe(best, len(offers)),
        "offers": [{k: v for k, v in o.items() if k != "coordinates"} for o in offers[:10]],
    }


def _describe(best, count):
    if not best:
        return "Nicht verfügbar"
    parts = [best["store_name"]]
    if best["online_stock"] > 0:
        parts.append("versandfähig")
    else:
        parts.append(f"nur lokal ({best.get('distance_km', '?')} km)")
    if best["on_display"]:
        parts.append("Ausstellungsstück")
    if count > 1:
        parts.append(f"{count} Angebote")
    return ", ".join(parts)
