"""Preisabfrage bei Euronics über die strukturierten Produktdaten (schema.org) der Shopseite."""

import html
import json
import re
from urllib.parse import quote_plus, urljoin

from .. import ratelimit
from .common import http_session, parse_price

BASE = "https://www.euronics.de"

_JSONLD_RE = re.compile(r'<script[^>]+type=["\']application/ld\+json["\'][^>]*>(.*?)</script>', re.S | re.I)
_META_RE = re.compile(r'<meta[^>]+(?:itemprop|property|name)=["\']([^"\']+)["\'][^>]*content=["\']([^"\']*)["\']', re.I)
_META_REV_RE = re.compile(r'<meta[^>]+content=["\']([^"\']*)["\'][^>]*(?:itemprop|property|name)=["\']([^"\']+)["\']', re.I)
_TITLE_RE = re.compile(r"<title[^>]*>(.*?)</title>", re.S | re.I)


# Euronics sitzt hinter einer AWS-WAF, die bei zu vielen Anfragen mit 403 antwortet
THROTTLE_STATUS = {403, 429, 503}


def _limiter(settings):
    return ratelimit.get_limiter("euronics", settings.get("euronics_rate_per_min") or 6, min_per_minute=1)


def _get(url, settings):
    session = http_session()
    session.headers["Accept"] = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
    try:
        response = ratelimit.request(session, _limiter(settings), "GET", url,
                                     throttle_status=THROTTLE_STATUS, attempts=2, timeout=30)
    except ratelimit.RateLimited:
        raise ratelimit.RateLimited(
            "Euronics blockiert oder drosselt die Anfragen (HTTP 403/429). "
            "Rechenzentrums-IPs werden oft komplett gesperrt – am besten zuhause laufen lassen."
        )
    response.raise_for_status()
    return response


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


def check(watch, settings, store_state):
    query = (watch.get("euronics_query") or "").strip() or (watch.get("ean") or "").strip()
    if not query:
        raise ValueError("Keine Euronics-URL bzw. EAN hinterlegt")

    if "euronics.de" in query:
        url = query.split("#")[0]
    else:
        url = f"{BASE}/search?sSearch={quote_plus(query)}"

    _limiter(settings).start_scan()
    response = _get(url, settings)
    product = parse_product_page(response.text, response.url)
    if product is None and "euronics.de" not in query:
        link = _first_product_link(response.text, response.url)
        if link:
            response = _get(link, settings)
            product = parse_product_page(response.text, response.url)
    if product is None:
        raise LookupError("Kein Preis auf der Euronics-Seite gefunden – bitte direkte Produkt-URL angeben")

    return {
        "title": product["title"],
        "url": product["url"],
        "ean": product["ean"],
        "price": product["price"] if product["available"] else None,
        "available": product["available"],
        "detail": "Euronics Onlineshop" if product["available"] else "Nicht verfügbar",
    }, store_state
