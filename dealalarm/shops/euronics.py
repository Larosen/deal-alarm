"""Preisabfrage bei Euronics und media@home – Onlineshop und jeder Markt einzeln.

Jeder Markt hat einen eigenen Shop mit eigenem Pfad, der Produktpfad ist überall gleich:

    https://www.euronics.de/<produktpfad>                ← Onlineshop
    https://www.euronics.de/<markt>/<produktpfad>        ← Euronics-Markt
    https://www.mediaathome.de/<markt>/<produktpfad>     ← media@home-Markt

Der Preis wird aus den strukturierten Produktdaten (schema.org) der Seite gelesen.
"""

import html
import json
import re
from urllib.parse import quote_plus, urljoin, urlsplit

from .. import ratelimit, scan
from .common import http_session, parse_price

BASE = "https://www.euronics.de"
ONLINE_STORE = "online"
HOSTS = {"www.euronics.de": "Euronics", "www.mediaathome.de": "media@home"}

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


def _resolve_path(watch, settings, stores, store_state):
    """Produktpfad aus der hinterlegten URL oder per EAN-Suche im Onlineshop."""
    query = (watch.get("euronics_query") or "").strip()
    if "euronics.de" in query or "mediaathome.de" in query:
        return product_path(query.split("#")[0].split("?")[0], stores)
    ean = (watch.get("ean") or "").strip()
    term = query or ean
    if not term:
        raise ValueError("Keine Euronics-URL bzw. EAN hinterlegt")
    if store_state.get("_query") == term and store_state.get("_product_path"):
        return store_state["_product_path"]
    response = _get(f"{BASE}/search?sSearch={quote_plus(term)}", settings)
    if parse_product_page(response.text, response.url):
        return product_path(response.url.split("?")[0], stores)
    link = _first_product_link(response.text, response.url)
    if link:
        return product_path(link, stores)
    raise LookupError("Produkt bei Euronics nicht gefunden – bitte direkte Produkt-URL angeben")


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


def check(watch, settings, store_state):
    """Fragt Onlineshop und – falls gewünscht – jeden Markt einzeln ab."""
    markets = parse_store_list(settings.get("euronics_stores")) if watch.get("euronics_all_stores", True) else []
    path = _resolve_path(watch, settings, markets, store_state)
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
        offer = to_offer(parse_product_page(response.text, url))
        if offer:
            offer["url"] = url
        return offer

    budget = int(settings.get("euronics_stores_per_cycle") or 0) or None
    meta = {k: v for k, v in store_state.items() if k.startswith("_")}
    stores_state = {k: v for k, v in store_state.items() if not k.startswith("_")}
    stores_state, stats = scan.run(stores, stores_state, fetch, budget=budget, workers=len(HOSTS))

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
