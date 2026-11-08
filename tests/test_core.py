import json

import pytest


@pytest.fixture(autouse=True)
def offline_directory(monkeypatch):
    """Tests nutzen immer die mitgelieferte Marktliste, nie das Netz."""
    def no_download():
        raise OSError("offline")
    monkeypatch.setattr(euronics, "_download_directory", no_download)
    monkeypatch.setattr(euronics, "_markets", {"time": 0, "data": None})

from dealalarm import checker, ratelimit, scan
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
    offer = expert.parse_offer(data)
    assert offer["price"] == 229
    assert offer["total_price"] == 233.99
    assert expert.parse_offer({"price": {}}) is None


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


def _stores(n):
    return [{"id": f"s{i}", "name": f"Filiale {i}"} for i in range(n)]


def _offer(price):
    return {"price": price, "shipping": 0, "total_price": price, "online_stock": 1, "store_stock": 0, "on_display": False}


def test_plan_prioritises_known_offers_then_unchecked_then_oldest():
    state = {
        "s0": {"name": "", "checked": "2026-10-01T00:00:00+00:00", "offer": None},
        "s1": {"name": "", "checked": "2026-10-30T00:00:00+00:00", "offer": _offer(300)},
        "s2": {"name": "", "checked": "2026-10-30T00:00:00+00:00", "offer": _offer(250)},
        "s3": {"name": "", "checked": "2026-10-20T00:00:00+00:00", "offer": None},
    }
    order = [s["id"] for s in scan.plan(_stores(6), state)]
    assert order == ["s2", "s1", "s4", "s5", "s0", "s3"]
    assert [s["id"] for s in scan.plan(_stores(6), state, budget=4)] == ["s2", "s1", "s4", "s5"]
    assert [s["id"] for s in scan.plan(_stores(6), state, budget=3)] == ["s2", "s4", "s5"]


def test_scan_rotates_through_all_stores_with_budget():
    prices = {f"s{i}": 100 + i for i in range(10)}
    state = {}
    for _ in range(4):
        state, stats = scan.run(_stores(10), state, lambda s: _offer(prices[s["id"]]), budget=4)
    assert len(state) == 10 and stats["known"] == 10
    assert scan.fresh_offers({**state, "_webcode": "123"}, 24)[0]["store_id"] == "s0"


def test_scan_keeps_partial_results_when_rate_limited():
    def fetch(store):
        if store["id"] in ("s3", "s4"):
            raise ratelimit.RateLimited("zu viele Anfragen")
        return _offer(100)
    state, stats = scan.run(_stores(5), {}, fetch)
    assert stats["aborted"] and stats["checked"] == 3 and set(state) == {"s0", "s1", "s2"}


def test_scan_drops_stores_that_no_longer_exist():
    state = {"gone": {"name": "x", "checked": scan.now_iso(), "offer": _offer(1)}}
    state, _ = scan.run(_stores(1), state, lambda s: None)
    assert set(state) == {"s0"} and state["s0"]["offer"] is None


def test_limiter_backs_off_and_recovers(monkeypatch):
    monkeypatch.setattr(ratelimit.time, "sleep", lambda s: None)
    limiter = ratelimit.AdaptiveLimiter("t", 60, min_per_minute=2, max_strikes=2)
    limiter.throttled()
    assert limiter.rate == 30
    limiter.throttled(retry_after=5)
    assert limiter.rate == 15
    with pytest.raises(ratelimit.RateLimited):
        limiter.wait()
    limiter.start_scan()
    limiter.wait()
    for _ in range(25):
        limiter.ok()
    assert limiter.rate > 15


class _Resp:
    def __init__(self, status, headers=None):
        self.status_code = status
        self.headers = headers or {}


def test_request_retries_after_throttle(monkeypatch):
    monkeypatch.setattr(ratelimit.time, "sleep", lambda s: None)
    answers = iter([_Resp(429, {"Retry-After": "1"}), _Resp(200)])

    class Session:
        def request(self, *a, **k):
            return next(answers)
    limiter = ratelimit.AdaptiveLimiter("t", 600)
    assert ratelimit.request(Session(), limiter, "GET", "u").status_code == 200
    assert limiter.throttled_count == 1


def test_expert_filters():
    offers = [
        {**_offer(100), "on_display": True, "store_id": "a"},
        {**_offer(120), "online_stock": 0, "store_stock": 1, "store_id": "b", "coordinates": (53.6, 9.95)},
        {**_offer(130), "store_id": "c"},
    ]
    assert [o["store_id"] for o in expert.filter_offers(offers, {}, {})] == ["c"]
    watch = {"expert_include_display": True}
    assert [o["store_id"] for o in expert.filter_offers(offers, watch, {})] == ["a", "c"]


PATH = "haus-und-haushalt/koerperpflege-und-gesundheit/rasierer-und-haarentfernung/herrenrasierer/9150cc-series-9-sport-trocken-nass-rasierer-4065465374635"
MARKETS = """
# Hamburg
https://www.euronics.de/hamburg-buddenhagen
Badje-Ott | www.mediaathome.de/hamburg-badje-ott/
https://www.euronics.de/hamburg-stahn
https://www.euronics.de/hamburg-buddenhagen/irgendwas
https://example.com/kein-markt
"""


def test_parse_store_list():
    stores = euronics.parse_store_list(MARKETS)
    assert [s["id"] for s in stores] == ["www.euronics.de/hamburg-buddenhagen", "www.mediaathome.de/hamburg-badje-ott",
                                         "www.euronics.de/hamburg-stahn"]
    assert stores[1]["name"] == "Badje-Ott" and stores[1]["base"] == "https://www.mediaathome.de/hamburg-badje-ott"
    assert stores[0]["name"] == "Euronics Hamburg Buddenhagen"


def test_product_path_strips_market_prefix():
    stores = euronics.parse_store_list(MARKETS)
    assert euronics.product_path(f"https://www.euronics.de/{PATH}", stores) == PATH
    assert euronics.product_path(f"https://www.euronics.de/hamburg-buddenhagen/{PATH}", stores) == PATH
    assert euronics.product_path(f"https://www.mediaathome.de/hamburg-badje-ott/{PATH}", stores) == PATH
    assert euronics.ean_from_path(PATH) == "4065465374635"


class _Page:
    def __init__(self, url, price=None, status=200):
        self.url = url
        self.status_code = status
        ld = {"@type": "Product", "name": "Braun Series 9", "offers": {"price": price, "availability": "InStock"}}
        self.text = f'<script type="application/ld+json">{json.dumps(ld)}</script>' if price else "<html></html>"


def test_euronics_scans_every_market(monkeypatch):
    pages = {
        f"https://www.euronics.de/{PATH}": _Page(f"https://www.euronics.de/{PATH}", 329.0),
        f"https://www.euronics.de/hamburg-buddenhagen/{PATH}": _Page(f"https://www.euronics.de/hamburg-buddenhagen/{PATH}", 299.0),
        # nicht im Sortiment: Weiterleitung auf den Onlineshop
        f"https://www.mediaathome.de/hamburg-badje-ott/{PATH}": _Page(f"https://www.mediaathome.de/{PATH}", 279.0),
        f"https://www.euronics.de/hamburg-stahn/{PATH}": None,  # 404
    }
    monkeypatch.setattr(euronics, "_get", lambda url, settings, session=None, allow_missing=False: pages[url])
    watch = {"euronics_query": f"https://www.euronics.de/hamburg-stahn/{PATH}", "euronics_all_stores": True}
    settings = {"euronics_stores": MARKETS, "euronics_use_directory": False, "price_max_age_hours": 24}
    result, state = euronics.check(watch, settings, {})
    assert result["price"] == 299.0 and result["detail"].startswith("Euronics Hamburg Buddenhagen")
    assert result["ean"] == "4065465374635" and result["title"] == "Braun Series 9"
    assert result["url"] == f"https://www.euronics.de/hamburg-buddenhagen/{PATH}"
    assert result["scan"]["checked"] == 4
    assert state["www.mediaathome.de/hamburg-badje-ott"]["offer"] is None
    assert state["_product_path"] == PATH


def test_euronics_partial_block_keeps_other_domain(monkeypatch):
    def fake_get(url, settings, session=None, allow_missing=False):
        if "mediaathome" in url:
            raise ratelimit.RateLimited("gesperrt")
        return _Page(url, 300.0)
    monkeypatch.setattr(euronics, "_get", fake_get)
    watch = {"euronics_query": f"https://www.euronics.de/{PATH}"}
    result, state = euronics.check(watch, {"euronics_stores": MARKETS, "euronics_use_directory": False}, {})
    assert result["scan"]["aborted"] and result["scan"]["checked"] == 3 and result["price"] == 300.0


def test_euronics_fully_blocked_raises(monkeypatch):
    def fake_get(*a, **k):
        raise ratelimit.RateLimited("gesperrt")
    monkeypatch.setattr(euronics, "_get", fake_get)
    with pytest.raises(ratelimit.RateLimited):
        euronics.check({"euronics_query": f"https://www.euronics.de/{PATH}"},
                       {"euronics_stores": MARKETS, "euronics_use_directory": False}, {})


def test_markets_from_geojson_groups_locations_by_shop():
    geo = {"features": [
        {"properties": {"name": "Buddenhagen Wentorf", "website": "https://www.euronics.de/hamburg-buddenhagen"},
         "geometry": {"coordinates": [10.25, 53.49]}},
        {"properties": {"name": "Buddenhagen Ahrensburg", "website": "https://www.euronics.de/hamburg-buddenhagen/"},
         "geometry": {"coordinates": [10.24, 53.67]}},
        {"properties": {"name": "ohne Website"}, "geometry": {"coordinates": [0, 0]}},
    ]}
    data = euronics.markets_from_geojson(geo, "test")
    assert len(data["shops"]) == 1 and len(data["shops"][0]["locations"]) == 2


def test_bundled_directory_contains_hamburg_markets():
    ids = {s["id"]: s for s in euronics.directory_stores()}
    assert len(ids) > 300
    assert "www.euronics.de/hamburg-buddenhagen" in ids and "www.mediaathome.de/hamburg-badje-ott" in ids
    assert "(+3 Filialen)" in ids["www.euronics.de/hamburg-buddenhagen"]["name"]


def test_select_markets_radius_and_manual_names(monkeypatch):
    monkeypatch.setattr(euronics, "plz_coordinates", lambda plz: (53.6226, 9.9513))  # Hamburg-Niendorf
    settings = {"plz": "22459", "euronics_use_directory": True,
                "euronics_stores": "Mein Lieblingsmarkt | https://www.euronics.de/hamburg-stahn"}
    near = euronics.select_markets({"euronics_max_distance": 20}, settings)
    ids = [s["id"] for s in near]
    assert "www.euronics.de/hamburg-stahn" in ids and "www.mediaathome.de/hamburg-badje-ott" in ids
    assert all(s["distance_km"] <= 20 for s in near) and len(near) < 30
    assert next(s for s in near if s["id"].endswith("hamburg-stahn"))["name"] == "Mein Lieblingsmarkt"
    assert len(euronics.select_markets({}, settings)) > 300
    assert euronics.select_markets({"euronics_all_stores": False}, settings) == []


def test_title_from_path():
    assert euronics.title_from_path(PATH) == "9150cc Series 9 Sport Trocken Nass Rasierer"


def test_expert_not_in_assortment_is_no_error(monkeypatch):
    monkeypatch.setattr(expert, "find_by_ean", lambda ean: None)
    result, state = expert.check({"expert_query": "", "ean": "4065465374635"}, {}, {"alt": 1})
    assert result["available"] is False and "Nicht im expert-Sortiment" in result["detail"] and state == {}


def test_shop_active_uses_ean_for_both_shops():
    watch = {"expert_enabled": True, "euronics_enabled": True, "ean": "4065465374635"}
    assert checker._shop_active(watch, "expert") and checker._shop_active(watch, "euronics")
    assert not checker._shop_active({"expert_enabled": True}, "expert")


def _client(tmp_path, monkeypatch):
    from dealalarm import storage
    monkeypatch.setattr(storage, "DATA_DIR", str(tmp_path))
    monkeypatch.setattr(storage, "DATA_FILE", str(tmp_path / "state.json"))
    from dealalarm.app import create_app
    return create_app(start_scheduler=False).test_client()


def test_resolve_euronics_url_finds_expert_by_ean(tmp_path, monkeypatch):
    client = _client(tmp_path, monkeypatch)
    monkeypatch.setattr(expert, "find_by_ean", lambda ean: {"title": "BRAUN Series 9", "ean": ean,
                                                            "url": "https://www.expert.de/x.html", "webcode": "1"})
    art = client.get("/api/article/resolve", query_string={"q": f"https://www.mediaathome.de/hamburg-badje-ott/{PATH}"}).json["article"]
    assert art["ean"] == "4065465374635" and art["expert_query"] == "https://www.expert.de/x.html"
    assert art["euronics_query"] == f"https://www.euronics.de/{PATH}" and art["title"] == "BRAUN Series 9"

    monkeypatch.setattr(expert, "find_by_ean", lambda ean: None)
    art = client.get("/api/article/resolve", query_string={"q": f"https://www.euronics.de/{PATH}"}).json["article"]
    assert art["expert_found"] is False and art["title"].startswith("9150cc")


def test_resolve_unknown_ean_still_creates_article(tmp_path, monkeypatch):
    client = _client(tmp_path, monkeypatch)
    monkeypatch.setattr(expert, "find_by_ean", lambda ean: None)
    art = client.get("/api/article/resolve", query_string={"q": "4065465374635"}).json["article"]
    assert art["ean"] == "4065465374635" and art["expert_found"] is False
    res = client.post("/api/watches", json={"name": "Braun", "target_price": 250, "ean": "4065465374635"})
    assert res.status_code == 201


def test_expert_checks_all_listings_of_an_ean(monkeypatch):
    product = {"title": "AirPods Pro 3", "ean": "0195950543742", "webcode": "A", "url": "https://e/A.html"}
    monkeypatch.setattr(expert, "find_by_ean", lambda ean: product)
    monkeypatch.setattr(expert, "search", lambda term: [
        {**product, "webcode": "B", "url": "https://e/B.html"},
        {**product, "webcode": "C", "url": "https://e/C.html", "ean": "andere"},
    ])
    monkeypatch.setattr(expert, "list_stores", lambda all_branches: [
        {"id": "e_1", "exp_id": "1", "name": "Filiale 1", "coordinates": None}])
    prices = {"A": 249, "B": 229}

    class Resp:
        status_code = 200
        def __init__(self, webcode): self.webcode = webcode
        def raise_for_status(self): pass
        def json(self):
            return {"price": {"bruttoPrice": prices[self.webcode], "onlineStock": 1}, "promotionPrice": {}}
    monkeypatch.setattr(expert.ratelimit, "request", lambda s, l, m, u, params: Resp(params["webcode"]))
    result, state = expert.check({"expert_query": "", "ean": product["ean"]}, {}, {})
    assert state["_webcodes"] == ["A", "B"]
    assert result["price"] == 229 and result["url"] == "https://e/B.html?branch_id=e_1"


def test_healthz_is_reachable_with_password(tmp_path, monkeypatch):
    from dealalarm import app as app_module
    monkeypatch.setattr(app_module, "UI_PASSWORD", "geheim")
    client = _client(tmp_path, monkeypatch)
    assert client.get("/healthz").status_code == 200
    assert client.get("/api/state").status_code == 401


def _storage_tmp(tmp_path, monkeypatch):
    from dealalarm import storage
    monkeypatch.setattr(storage, "DATA_DIR", str(tmp_path))
    monkeypatch.setattr(storage, "DATA_FILE", str(tmp_path / "state.json"))
    return storage


def test_shops_run_independently(tmp_path, monkeypatch):
    """expert wartet nicht auf den langsamen Euronics-Scan."""
    import threading
    import time
    storage = _storage_tmp(tmp_path, monkeypatch)
    release = threading.Event()

    def fast(watch, settings, state, progress=None):
        return {"price": 100.0, "available": True, "detail": "x"}, {}

    def slow(watch, settings, state, progress=None):
        release.wait(5)
        return {"price": 90.0, "available": True, "detail": "y"}, {}

    monkeypatch.setattr(checker, "SHOPS", {"expert": ("expert", fast), "euronics": ("Euronics", slow)})
    monkeypatch.setattr(checker, "_workers", {})
    a = storage.add_watch({"name": "A", "target_price": 50, "ean": "1"})
    b = storage.add_watch({"name": "B", "target_price": 50, "ean": "2"})
    assert checker.run_all()
    deadline = time.time() + 5
    while time.time() < deadline and not all("expert" in (storage.get_watch(w["id"]).get("results") or {}) for w in (a, b)):
        time.sleep(0.05)
    assert all("expert" in storage.get_watch(w["id"])["results"] for w in (a, b))
    status = checker.status()
    assert status["euronics"]["current"] == a["id"] and status["euronics"]["queue"] == [b["id"]]
    assert checker.is_running()
    # Ein einzeln angestoßenes Gerät rückt nach vorne
    checker.run_all(b["id"])
    assert checker.status()["euronics"]["queue"] == [b["id"]]
    release.set()
    deadline = time.time() + 5
    while time.time() < deadline and checker.is_running():
        time.sleep(0.05)
    assert not checker.is_running()
    assert storage.get_watch(b["id"])["results"]["euronics"]["price"] == 90.0


def test_scan_reports_progress_and_logs(caplog):
    import logging
    from dealalarm import activity
    reporter = activity.ScanReporter(logging.getLogger("dealalarm.test"), "expert", "expert", "TV", every=2)
    with caplog.at_level(logging.INFO, logger="dealalarm"):
        scan.run(_stores(5), {}, lambda s: _offer(100 + int(s["id"][1:])), progress=reporter)
    lines = [r.getMessage() for r in caplog.records]
    assert any("5/5 Filialen geprüft, 5 mit Angebot, bisher günstigster 100.00 € (Filiale 0)" in l for l in lines)
    assert activity.progress()["expert"]["done"] == 5
    activity.clear_progress("expert")


class _NtfyResp:
    def __init__(self, status=200, data=None, ctype="application/json"):
        self.status_code = status
        self._data = data
        self.headers = {"Content-Type": ctype}
    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(self.status_code)
    def json(self):
        if self._data is None:
            raise ValueError("kein JSON")
        return self._data


def test_notifier_checks_for_real_ntfy_answer(monkeypatch):
    from dealalarm import notifier
    settings = {"ntfy_server": "https://ntfy.example", "ntfy_topic": "deal-alarm"}
    monkeypatch.setattr(notifier.requests, "post", lambda *a, **k: _NtfyResp(data={"id": "abc", "topic": "deal-alarm"}))
    assert notifier.send(settings, "t", "m") == "abc"
    monkeypatch.setattr(notifier.requests, "post", lambda *a, **k: _NtfyResp(data=None, ctype="text/html"))
    with pytest.raises(ValueError, match="keine ntfy-Antwort"):
        notifier.send(settings, "t", "m")
    monkeypatch.setattr(notifier.requests, "post", lambda *a, **k: _NtfyResp(status=403))
    with pytest.raises(PermissionError, match="Access-Token"):
        notifier.send(settings, "t", "m")


def test_logs_endpoint_and_new_watch_is_queued(tmp_path, monkeypatch):
    import logging
    client = _client(tmp_path, monkeypatch)
    queued = []
    monkeypatch.setattr(checker, "run_all", lambda watch_id=None: queued.append(watch_id) or True)
    res = client.post("/api/watches", json={"name": "TV", "target_price": 999, "ean": "1"})
    assert queued == [res.json["id"]]
    logging.getLogger("dealalarm.test").info("Hallo Protokoll")
    lines = client.get("/api/logs").json
    assert any(l["message"] == "Hallo Protokoll" for l in lines)
    assert client.get("/api/logs", query_string={"after": lines[-1]["id"]}).json == []


def test_expert_single_listing_error_does_not_drop_store(monkeypatch):
    product = {"title": "AirPods Pro 3", "ean": "1", "webcode": "A", "url": "https://e/A.html"}
    monkeypatch.setattr(expert, "find_by_ean", lambda ean: product)
    monkeypatch.setattr(expert, "search", lambda term: [{**product, "webcode": "B", "url": "https://e/B.html"}])
    monkeypatch.setattr(expert, "list_stores", lambda all_branches: [
        {"id": "e_1", "exp_id": "1", "name": "Filiale 1", "coordinates": None}])

    class Resp:
        def __init__(self, webcode): self.webcode = webcode; self.status_code = 500 if webcode == "A" else 200
        def raise_for_status(self):
            if self.status_code >= 400: raise RuntimeError("500 Server Error")
        def json(self): return {"price": {"bruttoPrice": 199, "onlineStock": 1}, "promotionPrice": {}}
    monkeypatch.setattr(expert.ratelimit, "request", lambda s, l, m, u, params: Resp(params["webcode"]))
    result, _ = expert.check({"expert_query": "", "ean": "1"}, {}, {})
    assert result["price"] == 199 and result["scan"]["failed"] == 0


def test_progress_best_respects_filter():
    seen = []
    offers = {"s0": {**_offer(90), "online_stock": 0, "store_stock": 1}, "s1": _offer(120), "s2": _offer(110)}
    scan.run(_stores(3), {}, lambda s: offers[s["id"]], progress=lambda stats, state, best: seen.append(best),
             accept=lambda o: o["online_stock"] > 0)
    assert seen[-1]["total_price"] == 110 and seen[-1]["store_name"] == "Filiale 2"


EURONICS_SEARCH = """<html><a href="/tv-und-audio/fernsehen/oled-tvs/gq65s93dat-163-cm-65-oled-tv-4065327906011">S93D</a>
<a href="/tv-und-audio/fernsehen/oled-tvs/gq65s94hae-163-cm-oled-tv-8806097123456?ref=x">S94H</a></html>"""


def test_euronics_search_picks_product_with_matching_ean(monkeypatch):
    class Page:
        url = "https://www.euronics.de/search?sSearch=8806097123456"
        text = EURONICS_SEARCH
        status_code = 200
    monkeypatch.setattr(euronics, "_get", lambda *a, **k: Page())
    watch = {"ean": "8806097123456", "euronics_query": ""}
    path = euronics._resolve_path(watch, {}, [], {})
    assert path.endswith("gq65s94hae-163-cm-oled-tv-8806097123456")


def test_euronics_rejects_other_product(monkeypatch):
    class Page:
        url = "https://www.euronics.de/search?sSearch=8806097999999"
        text = EURONICS_SEARCH
        status_code = 200
    monkeypatch.setattr(euronics, "_get", lambda *a, **k: Page())
    monkeypatch.setattr(euronics, "_first_product_link", lambda page, url: "https://www.euronics.de/tv/gq65s93dat-4065327906011")
    with pytest.raises(euronics.OtherProduct, match="führt EAN 8806097999999 nicht") as exc:
        euronics._resolve_path({"ean": "8806097999999", "euronics_query": ""}, {}, [], {})
    assert exc.value.suggestion["url"] == "https://www.euronics.de/tv/gq65s93dat-4065327906011"
    # auch ein zwischengespeicherter falscher Pfad wird verworfen
    cached = {"_query": "8806097999999", "_product_path": "tv/gq65s93dat-4065327906011"}
    with pytest.raises(euronics.OtherProduct):
        euronics._resolve_path({"ean": "8806097999999", "euronics_query": ""}, {}, [], cached)
    # Händler-Variante: eine übernommene URL wird trotz anderer EAN geprüft
    watch = {"ean": "8806097999999", "euronics_query": "https://www.euronics.de/tv/gq65s93dat-4065327906011"}
    assert euronics._resolve_path(watch, {}, [], {}) == "tv/gq65s93dat-4065327906011"


def test_lookup_error_clears_old_store_prices(tmp_path, monkeypatch):
    storage = _storage_tmp(tmp_path, monkeypatch)
    def not_found(watch, settings, state, progress=None):
        raise euronics.OtherProduct("Euronics führt EAN 1 nicht", "https://www.euronics.de/x-2", "X")
    monkeypatch.setattr(checker, "SHOPS", {"euronics": ("Euronics", not_found)})
    w = storage.add_watch({"name": "TV", "target_price": 1, "ean": "1"})
    storage.save_store_state(w["id"], "euronics", {"x": {"name": "alt", "checked": scan.now_iso(), "offer": _offer(1)}})
    checker.check_shop(storage.get_watch(w["id"]), "euronics", {})
    watch = storage.get_watch(w["id"])
    assert watch["store_prices"]["euronics"] == {} and "führt EAN 1 nicht" in watch["results"]["euronics"]["error"]
    assert watch["results"]["euronics"]["suggestion"]["url"] == "https://www.euronics.de/x-2"
