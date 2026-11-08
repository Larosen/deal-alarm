"""Preisabfrage bei Euronics und media@home – Onlineshop und jeder Markt einzeln.

Jeder Markt hat einen eigenen Shop mit eigenem Pfad, der Produktpfad ist überall gleich:

    https://www.euronics.de/<produktpfad>                ← Onlineshop
    https://www.euronics.de/<markt>/<produktpfad>        ← Euronics-Markt
    https://www.mediaathome.de/<markt>/<produktpfad>     ← media@home-Markt

Der Preis wird aus den strukturierten Produktdaten (schema.org) der Seite gelesen.
"""

import html
import json
import logging
import os
import re
import threading
import time
from urllib.parse import quote_plus, urljoin, urlsplit

import requests

from .. import ratelimit, scan
from .common import haversine_km, http_session, parse_price, plz_coordinates

log = logging.getLogger(__name__)

BASE = "https://www.euronics.de"
ONLINE_STORE = "online"
HOSTS = {"www.euronics.de": "Euronics", "www.mediaathome.de": "media@home"}

# Händlerverzeichnis: Daten der Euronics-Händlersuche (Woosmap), gesammelt vom offenen
# Projekt All The Places (CC0). Mitgeliefert als Momentaufnahme, wöchentlich aktualisiert.
MARKETS_BACKUP = os.path.join(os.path.dirname(__file__), "euronics_markets.json")
ATP_LATEST = "https://data.alltheplaces.xyz/runs/latest.json"
ATP_SPIDER = "euronics_de"
MARKETS_REFRESH_SECONDS = 7 * 24 * 3600
_markets = {"time": 0, "data": None}
_markets_lock = threading.Lock()

_JSONLD_RE = re.compile(r'<script[^>]+type=["\']application/ld\+json["\'][^>]*>(.*?)</script>', re.S | re.I)
_META_RE = re.compile(r'<meta[^>]+(?:itemprop|property|name)=["\']([^"\']+)["\'][^>]*content=["\']([^"\']*)["\']', re.I)
_META_REV_RE = re.compile(r'<meta[^>]+content=["\']([^"\']*)["\'][^>]*(?:itemprop|property|name)=["\']([^"\']+)["\']', re.I)
_TITLE_RE = re.compile(r"<title[^>]*>(.*?)</title>", re.S | re.I)


# Euronics sitzt hinter einer AWS-WAF, die bei zu vielen Anfragen mit 403 antwortet
THROTTLE_STATUS = {403, 429, 503}


def _limiter(settings, url=BASE):
    """Ein Limiter pro Domain – euronics.de und mediaathome.de drosseln unabhängig voneinander."""
    host = urlsplit(url).hostname or "www.euronics.de"
    return ratelimit.get_limiter(host, settings.get("euronics_rate_per_min") or 6, min_per_minute=1)


def _session():
    session = http_session()
    session.headers["Accept"] = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
    return session


def _get(url, settings, session=None, allow_missing=False):
    try:
        response = ratelimit.request(session or _session(), _limiter(settings, url), "GET", url,
                                     throttle_status=THROTTLE_STATUS, attempts=2, timeout=30)
    except ratelimit.RateLimited:
        raise ratelimit.RateLimited(
            f"{urlsplit(url).hostname} blockiert oder drosselt die Anfragen (HTTP 403/429). "
            "Rechenzentrums-IPs werden oft komplett gesperrt – am besten zuhause laufen lassen."
        )
    if allow_missing and response.status_code in (404, 410):
        return None
    response.raise_for_status()
    return response


def parse_store_list(text):
    """Marktliste aus den Einstellungen: eine Markt-URL pro Zeile, optional 'Name | URL'."""
    stores, seen = [], set()
    for line in (text or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, url = line.rpartition("|") if "|" in line else ("", "", line)
        url = url.strip()
        if "://" not in url:
            url = "https://" + url
        parts = urlsplit(url)
        host = parts.hostname or ""
        if host in ("euronics.de", "mediaathome.de"):
            host = "www." + host
        slug = parts.path.strip("/").split("/")[0] if parts.path.strip("/") else ""
        if host not in HOSTS or not slug:
            continue
        store_id = f"{host}/{slug}"
        if store_id in seen:
            continue
        seen.add(store_id)
        label = name.strip() or f"{HOSTS[host]} {slug.replace('-', ' ').title()}"
        stores.append({"id": store_id, "name": label, "base": f"https://{host}/{slug}", "slug": slug})
    return stores


def markets_from_geojson(geojson, source=""):
    """Wandelt die GeoJSON-Ausgabe von All The Places in die Shop-Liste um (ein Eintrag je Shop-URL)."""
    shops = {}
    for feature in geojson.get("features", []):
        props = feature.get("properties") or {}
        url = (props.get("website") or "").rstrip("/")
        coords = (feature.get("geometry") or {}).get("coordinates")
        if not url or not coords:
            continue
        shops.setdefault(url, []).append({
            "name": props.get("name", ""),
            "street": props.get("addr:street_address", ""),
            "postcode": props.get("addr:postcode", ""),
            "city": props.get("addr:city", ""),
            "lat": round(coords[1], 6),
            "lon": round(coords[0], 6),
        })
    return {"source": source, "shops": [{"url": u, "locations": sorted(l, key=lambda x: x["name"])}
                                         for u, l in sorted(shops.items())]}


def _download_directory():
    latest = requests.get(ATP_LATEST, timeout=30).json()
    run_base = latest["output_url"].rsplit("/", 1)[0]
    response = requests.get(f"{run_base}/output/{ATP_SPIDER}.geojson", timeout=60)
    response.raise_for_status()
    data = markets_from_geojson(response.json(), f"All The Places (CC0), Lauf {latest.get('run_id', '?')}")
    if len(data["shops"]) < 100:
        raise ValueError(f"Nur {len(data['shops'])} Shops geliefert")
    return data


def load_directory():
    """Händlerverzeichnis (wöchentlich aktualisiert, mitgelieferte Momentaufnahme als Fallback)."""
    with _markets_lock:
        if _markets["data"] and time.time() - _markets["time"] < MARKETS_REFRESH_SECONDS:
            return _markets["data"]
        try:
            data = _download_directory()
        except Exception as exc:
            log.info("Händlerverzeichnis nicht aktualisiert (%s) – nutze %s", exc,
                     "letzten Stand" if _markets["data"] else "mitgelieferte Liste")
            data = _markets["data"]
            if data is None:
                with open(MARKETS_BACKUP, "r", encoding="utf-8") as f:
                    data = json.load(f)
        _markets.update(time=time.time(), data=data)
        return data


def directory_stores():
    stores = []
    for shop in load_directory()["shops"]:
        parsed = parse_store_list(shop["url"])
        if not parsed:
            continue
        store = parsed[0]
        locations = shop["locations"]
        store["name"] = locations[0]["name"] + (f" (+{len(locations) - 1} Filialen)" if len(locations) > 1 else "")
        store["locations"] = locations
        stores.append(store)
    return stores


def select_markets(watch, settings):
    """Alle zu prüfenden Märkte: Verzeichnis + eigene Einträge, optional auf einen Umkreis begrenzt."""
    if not watch.get("euronics_all_stores", True):
        return []
    markets = {}
    if settings.get("euronics_use_directory", True):
        markets.update((s["id"], s) for s in directory_stores())
    for store in parse_store_list(settings.get("euronics_stores")):
        if store["id"] in markets:
            markets[store["id"]]["name"] = store["name"]  # eigener Name hat Vorrang
        else:
            markets[store["id"]] = store
    user_coords = plz_coordinates(settings["plz"]) if settings.get("plz") else None
    radius = float(watch.get("euronics_max_distance") or 0)
    selected = []
    for store in markets.values():
        locations = store.get("locations") or []
        if user_coords and locations:
            nearest = min(locations, key=lambda l: haversine_km(user_coords, (l["lat"], l["lon"])))
            store["coordinates"] = (nearest["lat"], nearest["lon"])
            store["distance_km"] = round(haversine_km(user_coords, store["coordinates"]))
        elif locations:
            store["coordinates"] = (locations[0]["lat"], locations[0]["lon"])
        if radius and user_coords and store.get("distance_km", float("inf")) > radius:
            continue
        selected.append(store)
    return sorted(selected, key=lambda s: s.get("distance_km", float("inf")))


def product_path(url, stores=()):
    """Produktpfad aus einer Euronics/media@home-URL – ein evtl. Markt-Präfix wird entfernt."""
    parts = urlsplit(url.strip())
    segments = [s for s in parts.path.split("/") if s]
    host = parts.hostname or ""
    slugs = {s["slug"] for s in stores}
    # Bei media@home steht immer ein Markt vorne, bei Euronics nur in Marktshops
    if segments and (segments[0] in slugs or host.endswith("mediaathome.de")) and len(segments) > 1:
        segments = segments[1:]
    return "/".join(segments)


def title_from_path(path):
    """Lesbarer Titel aus dem letzten Pfadteil, z. B. '9150cc Series 9 Sport Trocken Nass Rasierer'."""
    slug = path.rstrip("/").split("/")[-1]
    slug = re.sub(r"-\d{8,14}$", "", slug)
    return " ".join(w if any(c.isdigit() for c in w) else w.capitalize() for w in slug.split("-"))


def ean_from_path(path):
    match = re.search(r"-(\d{13})$", path)
    return match.group(1) if match else ""


def _iter_jsonld(page):
    for block in _JSONLD_RE.findall(page):
        try:
            data = json.loads(html.unescape(block.strip()))
        except ValueError:
            continue
        stack = data if isinstance(data, list) else [data]
        while stack:
            item = stack.pop(0)
            if isinstance(item, dict):
                yield item
                if "@graph" in item:
                    stack.extend(item["@graph"])
            elif isinstance(item, list):
                stack.extend(item)


def _is_type(item, name):
    kind = item.get("@type")
    return kind == name or (isinstance(kind, list) and name in kind)


def _meta(page):
    values = {}
    for key, value in _META_RE.findall(page):
        values.setdefault(key.lower(), html.unescape(value))
    for value, key in _META_REV_RE.findall(page):
        values.setdefault(key.lower(), html.unescape(value))
    return values


def parse_product_page(page, url):
    """Liest Titel, Preis, Verfügbarkeit und EAN aus einer Produktseite."""
    for item in _iter_jsonld(page):
        if not _is_type(item, "Product"):
            continue
        offers = item.get("offers") or {}
        if isinstance(offers, list):
            offers = offers[0] if offers else {}
        if _is_type(offers, "AggregateOffer"):
            price = parse_price(offers.get("lowPrice") or offers.get("price"))
        else:
            price = parse_price(offers.get("price"))
        if price is None and offers.get("priceSpecification"):
            spec = offers["priceSpecification"]
            spec = spec[0] if isinstance(spec, list) else spec
            price = parse_price(spec.get("price"))
        availability = str(offers.get("availability", ""))
        return {
            "title": html.unescape(item.get("name", "")),
            "price": price,
            "available": price is not None and not re.search(r"OutOfStock|Discontinued|SoldOut", availability, re.I),
            "ean": str(item.get("gtin13") or item.get("gtin") or item.get("ean") or ""),
            "url": offers.get("url") or url,
        }

    meta = _meta(page)
    price = parse_price(meta.get("product:price:amount") or meta.get("og:price:amount") or meta.get("price"))
    if price is None:
        return None
    availability = meta.get("product:availability") or meta.get("availability") or ""
    title_match = _TITLE_RE.search(page)
    return {
        "title": meta.get("og:title") or (html.unescape(title_match.group(1).strip()) if title_match else ""),
        "price": price,
        "available": not re.search(r"out ?of ?stock|oos|nicht", availability, re.I),
        "ean": meta.get("gtin13") or meta.get("product:ean") or "",
        "url": url,
    }


def _first_product_link(page, base_url):
    """Erster Produktlink aus einer Ergebnisliste (ItemList oder Product-Einträge)."""
    for item in _iter_jsonld(page):
        if _is_type(item, "ItemList"):
            for element in item.get("itemListElement", []):
                link = element.get("url") or (element.get("item") or {}).get("url") if isinstance(element, dict) else None
                if link:
                    return urljoin(base_url, link)
    return None


class OtherProduct(LookupError):
    """Die Suche lieferte ein Produkt mit anderer EAN – evtl. dasselbe Gerät als Händler-Variante."""

    def __init__(self, message, url, title):
        super().__init__(message)
        self.suggestion = {"url": url, "title": title}


def same_ean(a, b):
    """EANs gleich? (führende Nullen werden ignoriert, EAN-13 vs. GTIN-14)"""
    return bool(a and b) and a.lstrip("0") == b.lstrip("0")


def _link_for_ean(page, base_url, ean):
    """Produktlink in einer Ergebnisseite, dessen Pfad auf die gesuchte EAN endet."""
    pattern = re.compile(r'href=["\']([^"\'#?]*-0*' + re.escape(ean.lstrip("0")) + r')/?(?:[?#][^"\']*)?["\']', re.I)
    match = pattern.search(page)
    return urljoin(base_url, html.unescape(match.group(1))) if match else None


def _resolve_path(watch, settings, stores, store_state):
    """Produktpfad aus der hinterlegten URL oder per EAN-Suche im Onlineshop.

    Wird über die Suche zugeordnet, muss die EAN im gefundenen Produktpfad zur EAN des
    Geräts passen – sonst würde ein ähnliches, aber anderes Gerät geprüft.
    """
    query = (watch.get("euronics_query") or "").strip()
    ean = (watch.get("ean") or "").strip()
    if "euronics.de" in query or "mediaathome.de" in query:
        path = product_path(query.split("#")[0].split("?")[0], stores)
        found = ean_from_path(path)
        if ean and found and not same_ean(found, ean):
            log.info("Euronics-URL hat EAN %s, das Gerät EAN %s (Händler-Variante) – geprüft wird die eingetragene URL",
                     found, ean)
        return path
    term = query or ean
    if not term:
        raise ValueError("Keine Euronics-URL bzw. EAN hinterlegt")
    if store_state.get("_query") == term and store_state.get("_product_path"):
        path = store_state["_product_path"]
    else:
        response = _get(f"{BASE}/search?sSearch={quote_plus(term)}", settings)
        path = None
        if parse_product_page(response.text, response.url):
            path = product_path(response.url.split("?")[0], stores)
        elif ean:
            link = _link_for_ean(response.text, response.url, ean)
            path = product_path(link, stores) if link else None
        if path is None:
            link = _first_product_link(response.text, response.url)
            path = product_path(link, stores) if link else None
        if path is None:
            raise LookupError(f"Bei Euronics nicht gefunden (Suche nach „{term}“ ohne Treffer) – "
                              "falls vorhanden, Produkt-URL im Gerät eintragen")
    if ean and not same_ean(ean_from_path(path), ean):
        # Hersteller wie Samsung vergeben je Händler eigene Modellnummern und EANs (expert S94 = Euronics S93).
        # Ob es dasselbe Gerät ist, entscheidet der Nutzer – automatisch geprüft wird nur bei gleicher EAN.
        raise OtherProduct(
            f"Euronics führt EAN {ean} nicht. Gefunden wurde „{title_from_path(path)}“ (EAN {ean_from_path(path) or '?'}) "
            "– falls das dasselbe Gerät ist (Händler-Variante), als Euronics-Produkt übernehmen.",
            f"{BASE}/{path}", title_from_path(path))
    return path


def to_offer(product):
    if not product or not product["available"] or product["price"] is None:
        return None
    return {
        "price": product["price"],
        "shipping": None,  # Versandkosten stehen nicht in den Produktdaten
        "total_price": product["price"],
        "online_stock": 0,
        "store_stock": 0,
        "on_display": False,
        "availability": "verfügbar",
        "title": product["title"],
    }


def check(watch, settings, store_state, progress=None):
    """Fragt Onlineshop und – falls gewünscht – jeden Markt einzeln ab."""
    markets = select_markets(watch, settings)
    path = _resolve_path(watch, settings, markets, store_state)
    log.info("Euronics · %s: Produkt %s/%s, %d Märkte + Onlineshop, Tempo %s Anfragen/Min. je Domain",
             watch.get("name"), BASE, path, len(markets), settings.get("euronics_rate_per_min") or 6)
    if store_state.get("_product_path") != path:
        store_state = {}
    stores = [{"id": ONLINE_STORE, "name": "Euronics Onlineshop", "base": BASE}] + markets
    session = _session()
    for host in HOSTS:
        _limiter(settings, f"https://{host}/").start_scan()

    def fetch(store):
        url = f"{store['base']}/{path}"
        response = _get(url, settings, session=session, allow_missing=True)
        if response is None:
            return None  # Produkt nicht im Sortiment des Markts
        if store["id"] != ONLINE_STORE and f"/{store['slug']}/" not in urlsplit(response.url).path + "/":
            return None  # Weiterleitung weg vom Markt: nicht im Sortiment
        product = parse_product_page(response.text, url)
        if store["id"] == ONLINE_STORE:
            if product is None:
                log.warning("Euronics Onlineshop: Seite ohne lesbare Produktdaten (HTTP %s, %s) – "
                            "Preise können so nicht gelesen werden", response.status_code, response.url)
            else:
                log.info("Euronics Onlineshop: %s – %s", product["title"] or "Produkt",
                         f"{product['price']:.2f} €" if product["available"] and product["price"] else "nicht verfügbar")
        offer = to_offer(product)
        if offer:
            offer["url"] = url
            if store.get("distance_km") is not None:
                offer["distance_km"] = store["distance_km"]
        return offer

    budget = int(settings.get("euronics_stores_per_cycle") or 0) or None
    meta = {k: v for k, v in store_state.items() if k.startswith("_")}
    stores_state = {k: v for k, v in store_state.items() if not k.startswith("_")}
    stores_state, stats = scan.run(stores, stores_state, fetch, budget=budget, workers=len(HOSTS), progress=progress)

    offers = scan.fresh_offers(stores_state, float(settings.get("price_max_age_hours") or 24))
    best = offers[0] if offers else None
    title = next((o.get("title") for o in offers if o.get("title")), "") or meta.get("_title", "")
    detail = "In keinem Markt verfügbar"
    if best:
        detail = best["store_name"] + (f", {len(offers)} Märkte mit Angebot" if len(offers) > 1 else "")
    if stats["checked"] == 0 and stats["planned"]:
        if stats["aborted"]:
            raise ratelimit.RateLimited("Euronics blockiert oder drosselt die Anfragen (HTTP 403/429) – "
                                        "Rechenzentrums-IPs werden oft komplett gesperrt, am besten zuhause laufen lassen.")
    result = {
        "title": title,
        "url": best["url"] if best else f"{BASE}/{path}",
        "ean": ean_from_path(path),
        "price": best["total_price"] if best else None,
        "available": best is not None,
        "detail": detail,
        "scan": stats,
    }
    new_meta = {"_product_path": path, "_title": title,
                "_query": (watch.get("euronics_query") or "").strip() or (watch.get("ean") or "").strip()}
    return result, {**stores_state, **new_meta}


def filter_offers(offers, watch, settings):
    return offers
