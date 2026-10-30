"""Gemeinsame Hilfsfunktionen für die Shop-Abfragen."""

import functools
import math

import requests

USER_AGENT = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0"


def http_session():
    session = requests.Session()
    session.headers.update({
        "User-Agent": USER_AGENT,
        "Accept-Language": "de-DE,de;q=0.9,en;q=0.5",
    })
    return session


@functools.lru_cache(maxsize=64)
def plz_coordinates(plz):
    """Geo-Koordinaten für eine deutsche PLZ (None bei Fehler)."""
    plz = str(plz).strip()
    if not plz.isdigit() or len(plz) != 5:
        return None
    try:
        response = requests.get(f"https://api.zippopotam.us/de/{plz}", timeout=15)
        if response.status_code != 200:
            return None
        place = response.json()["places"][0]
        return float(place["latitude"]), float(place["longitude"])
    except Exception:
        return None


def haversine_km(a, b):
    lat1, lon1, lat2, lon2 = map(math.radians, (a[0], a[1], b[0], b[1]))
    h = math.sin((lat2 - lat1) / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin((lon2 - lon1) / 2) ** 2
    return 2 * 6371.0 * math.asin(math.sqrt(h))


def parse_price(value):
    """'1.299,00 €', '1299.00' oder 1299 -> 1299.0"""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        return float(value)
    text = str(value).replace("€", "").replace("EUR", "").replace("\xa0", "").strip()
    if not text:
        return None
    if "," in text:
        text = text.replace(".", "").replace(",", ".")
    try:
        return float(text)
    except ValueError:
        return None
