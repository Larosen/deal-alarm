"""Weboberfläche und REST-API des Deal-Alarms."""

import hmac
import logging
import os
import threading

from flask import Flask, Response, jsonify, render_template, request

from . import checker, notifier, ratelimit, scan, storage
from .shops import euronics, expert

SHOP_MODULES = {"expert": expert, "euronics": euronics}

log = logging.getLogger(__name__)

UI_USER = os.environ.get("UI_USER", "admin")
UI_PASSWORD = os.environ.get("UI_PASSWORD", "")

FLOAT_FIELDS = {"target_price", "expert_max_distance", "euronics_max_distance"}
BOOL_FIELDS = {
    "enabled", "expert_enabled", "expert_all_branches", "expert_include_display",
    "expert_include_local", "euronics_enabled", "euronics_all_stores", "euronics_use_directory", "scheduler_enabled",
}
INT_FIELDS = {
    "interval_minutes", "jitter_minutes", "ntfy_priority", "expert_rate_per_min", "expert_workers",
    "expert_stores_per_cycle", "euronics_rate_per_min", "euronics_stores_per_cycle", "price_max_age_hours",
}


def _clean(values):
    cleaned = {}
    for key, value in (values or {}).items():
        try:
            if key in FLOAT_FIELDS:
                value = float(str(value).replace(",", ".") or 0)
            elif key in INT_FIELDS:
                value = int(float(value or 0))
            elif key in BOOL_FIELDS:
                value = bool(value)
            elif isinstance(value, str):
                value = value.strip()
        except ValueError:
            raise ValueError(f"Ungültiger Wert für {key}")
        cleaned[key] = value
    return cleaned


def create_app(start_scheduler=True):
    app = Flask(__name__)
    scheduler = checker.Scheduler()
    if start_scheduler:
        scheduler.start()

    @app.before_request
    def basic_auth():
        if not UI_PASSWORD or request.path == "/healthz":
            return None
        auth = request.authorization
        if auth and hmac.compare_digest(auth.username or "", UI_USER) and hmac.compare_digest(auth.password or "", UI_PASSWORD):
            return None
        return Response("Anmeldung erforderlich", 401, {"WWW-Authenticate": 'Basic realm="Deal-Alarm"'})

    @app.errorhandler(ValueError)
    def bad_request(exc):
        return jsonify(error=str(exc)), 400

    @app.get("/healthz")
    def healthz():
        return jsonify(ok=True)

    @app.get("/")
    def index():
        return render_template("index.html")

    @app.get("/api/state")
    def state():
        data = storage.get_state()
        data["settings"]["ntfy_token"] = "••••" if data["settings"].get("ntfy_token") else ""
        data["scheduler"]["running"] = checker.is_running()
        data["limiters"] = ratelimit.snapshots()
        for watch in data["watches"]:
            # Filialpreise gibt es gesammelt über /api/watches/<id>/stores
            watch.pop("store_prices", None)
        return jsonify(data)

    @app.get("/api/watches/<watch_id>/stores")
    def store_prices(watch_id):
        watch = storage.get_watch(watch_id)
        if not watch:
            return jsonify(error="Nicht gefunden"), 404
        settings = storage.get_settings()
        shop = request.args.get("shop", "expert")
        if shop not in SHOP_MODULES:
            return jsonify(error="Unbekannter Shop"), 400
        store_state = (watch.get("store_prices") or {}).get(shop) or {}
        product_url = store_state.get("_product_url", "")
        rows, unavailable = [], 0
        all_offers = scan.fresh_offers(store_state, float("inf"))
        accepted = {o["store_id"] for o in SHOP_MODULES[shop].filter_offers(all_offers, watch, settings)}
        max_age = float(settings.get("price_max_age_hours") or 24)
        for store_id, entry in store_state.items():
            if store_id.startswith("_"):
                continue
            offer = entry.get("offer")
            if not offer:
                unavailable += 1
                continue
            rows.append({
                **offer,
                "store_id": store_id,
                "store_name": entry["name"],
                "checked": entry["checked"],
                "stale": scan.age_hours(entry["checked"]) > max_age,
                "matches_filter": store_id in accepted,
                "distance_km": offer.get("distance_km"),
                "url": offer.get("url") or (expert.store_url(product_url, store_id) if shop == "expert" and product_url else None),
            })
        rows.sort(key=lambda r: r["total_price"])
        return jsonify(stores=rows, unavailable=unavailable, target_price=watch["target_price"])

    @app.put("/api/settings")
    def settings():
        values = _clean(request.get_json())
        if values.get("ntfy_token") == "••••":
            values.pop("ntfy_token")
        if "interval_minutes" in values and values["interval_minutes"] < 15:
            raise ValueError("Intervall muss mindestens 15 Minuten betragen")
        storage.update_settings(values)
        scheduler.reschedule()
        return jsonify(ok=True)

    @app.post("/api/watches")
    def create_watch():
        values = _clean(request.get_json())
        if not any(values.get(k) for k in ("expert_query", "euronics_query", "ean")):
            raise ValueError("Bitte einen Artikel auswählen")
        if not values.get("target_price"):
            raise ValueError("Bitte einen Wunschpreis angeben")
        return jsonify(storage.add_watch(values)), 201

    @app.put("/api/watches/<watch_id>")
    def edit_watch(watch_id):
        watch = storage.update_watch(watch_id, _clean(request.get_json()))
        if not watch:
            return jsonify(error="Nicht gefunden"), 404
        return jsonify(watch)

    @app.delete("/api/watches/<watch_id>")
    def remove_watch(watch_id):
        return jsonify(ok=storage.delete_watch(watch_id))

    @app.post("/api/check")
    def check_now():
        watch_id = (request.get_json(silent=True) or {}).get("watch_id")
        if checker.is_running():
            return jsonify(error="Es läuft bereits eine Prüfung"), 409
        threading.Thread(target=checker.run_all, args=(watch_id,), daemon=True).start()
        return jsonify(ok=True), 202

    @app.post("/api/test-notify")
    def test_notify():
        notifier.send(
            storage.get_settings(),
            title="Deal-Alarm Test",
            message="Wenn du das liest, funktionieren die Benachrichtigungen. 🎉",
            tags=["white_check_mark"],
        )
        return jsonify(ok=True)

    @app.post("/api/euronics/stores/preview")
    def euronics_store_preview():
        """Zeigt, welche Märkte mit den eingegebenen Einstellungen geprüft würden."""
        body = request.get_json() or {}
        settings = {**storage.get_settings(), **_clean(body.get("settings") or {})}
        manual = euronics.parse_store_list(settings.get("euronics_stores"))
        directory = euronics.load_directory() if settings.get("euronics_use_directory") else None
        markets = euronics.select_markets({"euronics_all_stores": True}, settings)
        per_host = {}
        for store in markets:
            host = store["id"].split("/")[0]
            per_host[host] = per_host.get(host, 0) + 1
        return jsonify(
            manual=[{"name": s["name"], "url": s["base"]} for s in manual],
            directory_source=directory["source"] if directory else None,
            directory_count=len(directory["shops"]) if directory else 0,
            total=len(markets),
            per_host=per_host,
        )

    @app.get("/api/article/resolve")
    def resolve_article():
        """Ordnet eine Eingabe (URL eines Shops, EAN oder Suchbegriff) einem Artikel in beiden Shops zu."""
        query = request.args.get("q", "").strip()
        if not query:
            return jsonify(error="Bitte Suchbegriff, URL oder EAN eingeben"), 400
        try:
            if "euronics.de" in query or "mediaathome.de" in query:
                path = euronics.product_path(query.split("#")[0].split("?")[0], euronics.parse_store_list(query))
                ean = euronics.ean_from_path(path)
                if not ean:
                    return jsonify(error="In der Euronics-URL steht keine EAN – bitte die Produktseite verwenden"), 400
                match = expert.find_by_ean(ean)
                title = match["title"] if match else euronics.title_from_path(path)
                return jsonify(article={"title": title, "ean": ean,
                                        "expert_query": match["url"] if match else "",
                                        "euronics_query": f"{euronics.BASE}/{path}",
                                        "expert_found": bool(match)})
            if "expert.de" in query or expert.is_ean(query) or expert.extract_webcode(query):
                product = expert.resolve(query)
                return jsonify(article={"title": product["title"], "ean": product["ean"],
                                        "expert_query": product["url"], "euronics_query": "",
                                        "expert_found": True})
            return jsonify(results=[{**hit, "expert_query": hit["url"]} for hit in expert.search(query)[:10]])
        except LookupError as exc:
            if expert.is_ean(query):
                # Nicht bei expert – Euronics sucht trotzdem über die EAN
                return jsonify(article={"title": "", "ean": query, "expert_query": "", "euronics_query": "",
                                        "expert_found": False})
            return jsonify(error=str(exc)), 404
        except Exception as exc:
            return jsonify(error=f"Suche fehlgeschlagen: {exc}"), 502

    @app.get("/api/expert/search")
    def expert_search():
        term = request.args.get("q", "").strip()
        if not term:
            return jsonify([])
        try:
            return jsonify(expert.search(term)[:10])
        except Exception as exc:
            return jsonify(error=f"expert-Suche fehlgeschlagen: {exc}"), 502

    return app


def main():
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"), format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    from waitress import serve

    host = os.environ.get("HOST", "0.0.0.0")
    port = int(os.environ.get("PORT", "8080"))
    log.info("Deal-Alarm läuft auf http://%s:%s", host, port)
    serve(create_app(), host=host, port=port)


if __name__ == "__main__":
    main()
