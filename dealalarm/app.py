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

FLOAT_FIELDS = {"target_price", "expert_max_distance"}
BOOL_FIELDS = {
    "enabled", "expert_enabled", "expert_all_branches", "expert_include_display",
    "expert_include_local", "euronics_enabled", "euronics_all_stores", "scheduler_enabled",
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
        if not UI_PASSWORD:
            return None
        auth = request.authorization
        if auth and hmac.compare_digest(auth.username or "", UI_USER) and hmac.compare_digest(auth.password or "", UI_PASSWORD):
            return None
        return Response("Anmeldung erforderlich", 401, {"WWW-Authenticate": 'Basic realm="Deal-Alarm"'})

    @app.errorhandler(ValueError)
    def bad_request(exc):
        return jsonify(error=str(exc)), 400

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
        if not values.get("expert_query") and not values.get("euronics_query"):
            raise ValueError("Bitte mindestens expert- oder Euronics-Produkt angeben")
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
        """Zeigt, welche Märkte aus dem eingegebenen Text erkannt werden."""
        stores = euronics.parse_store_list((request.get_json() or {}).get("text", ""))
        return jsonify([{"name": s["name"], "url": s["base"]} for s in stores])

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
