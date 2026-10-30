import json

import pytest

from dealalarm import checker
from dealalarm.shops import euronics, expert
from dealalarm.shops.common import parse_price


@pytest.mark.parametrize("raw,expected", [
    ("1.299,00 €", 1299.0),
    ("199,99", 199.99),
    ("1299.00", 1299.0),
    (249, 249.0),
    ("", None),
    (None, None),
])
def test_parse_price(raw, expected):
    assert parse_price(raw) == expected


def test_extract_webcode():
    url = "https://www.expert.de/shop/unsere-produkte/tv-audio/kopfhorer/12940100537-airpods-pro-3.html?branch_id=e_1"
    assert expert.extract_webcode(url) == "12940100537"
    assert expert.extract_webcode("12940100537") == "12940100537"
    assert expert.extract_webcode("airpods pro") is None


def test_parse_offer_uses_checkout_price_and_shipping():
    data = {
        "price": {
            "bruttoPrice": 249, "onlineStock": 2, "storeStock": 0,
            "shipmentArray": [{"shipmentBruttoPrice": 4.99}],
            "itemOnDisplay": {"onDisplay": False},
        },
        "promotionPrice": {"checkoutPrice": 229},
    }
    offer = expert.parse_offer(data, "expert Test", "e_1", "https://x")
    assert offer["price"] == 229
    assert offer["total_price"] == 233.99
    assert expert.parse_offer({"price": {}}, "x", "e_1", "u") is None


def test_euronics_jsonld():
    ld = {"@context": "https://schema.org", "@type": "Product", "name": "Apple AirPods Pro 3", "gtin13": "0195950543742",
          "offers": {"@type": "Offer", "price": "219.00", "priceCurrency": "EUR", "availability": "https://schema.org/InStock"}}
    page = f'<html><script type="application/ld+json">{json.dumps(ld)}</script></html>'
    product = euronics.parse_product_page(page, "https://www.euronics.de/p")
    assert product["price"] == 219.0 and product["available"] and product["ean"] == "0195950543742"


def test_euronics_jsonld_graph_out_of_stock():
    ld = {"@graph": [{"@type": "WebPage"}, {"@type": "Product", "name": "TV",
          "offers": [{"@type": "Offer", "price": 999, "availability": "https://schema.org/OutOfStock"}]}]}
    page = f'<script type="application/ld+json">{json.dumps(ld)}</script>'
    product = euronics.parse_product_page(page, "u")
    assert product["price"] == 999 and not product["available"]


def test_euronics_meta_fallback():
    page = '<title>TV | EURONICS</title><meta itemprop="price" content="1.099,00">'
    product = euronics.parse_product_page(page, "u")
    assert product["price"] == 1099.0 and product["available"]
    assert euronics.parse_product_page("<html></html>", "u") is None


def test_alarm_only_once_until_price_drops_or_resets():
    watch = {"target_price": 200}
    r1 = {"price": 199}
    assert checker.evaluate(watch, "expert", r1, None) is True
    r2 = {"price": 199}
    assert checker.evaluate(watch, "expert", r2, r1) is False      # gleicher Preis: kein Spam
    r3 = {"price": 189}
    assert checker.evaluate(watch, "expert", r3, r2) is True       # weiter gefallen: neuer Alarm
    err = {"error": "timeout", "price": None}
    assert checker.evaluate(watch, "expert", err, r3) is False     # Fehler hält Status
    r4 = {"price": 189}
    assert checker.evaluate(watch, "expert", r4, err) is False
    r5 = {"price": 210}
    assert checker.evaluate(watch, "expert", r5, r4) is False      # über Wunschpreis: Reset
    r6 = {"price": 195}
    assert checker.evaluate(watch, "expert", r6, r5) is True


def test_unavailable_is_no_deal():
    assert checker.evaluate({"target_price": 200}, "euronics", {"price": None}, None) is False


def test_api_roundtrip(tmp_path, monkeypatch):
    from dealalarm import storage
    monkeypatch.setattr(storage, "DATA_DIR", str(tmp_path))
    monkeypatch.setattr(storage, "DATA_FILE", str(tmp_path / "state.json"))
    from dealalarm.app import create_app
    client = create_app(start_scheduler=False).test_client()

    assert client.post("/api/watches", json={"name": "x"}).status_code == 400
    res = client.post("/api/watches", json={"name": "TV", "target_price": "499,90", "expert_query": "123"})
    assert res.status_code == 201 and res.json["target_price"] == 499.9
    wid = res.json["id"]
    assert client.put(f"/api/watches/{wid}", json={"enabled": False}).json["enabled"] is False
    assert client.put("/api/settings", json={"interval_minutes": 5}).status_code == 400
    assert client.put("/api/settings", json={"ntfy_topic": "abc", "ntfy_token": "geheim"}).status_code == 200
    state = client.get("/api/state").json
    assert state["settings"]["ntfy_token"] == "••••" and len(state["watches"]) == 1
    client.put("/api/settings", json={"ntfy_token": "••••"})
    assert storage.get_settings()["ntfy_token"] == "geheim"
    assert client.delete(f"/api/watches/{wid}").json["ok"] is True
