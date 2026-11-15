//! Weboberfläche und REST-API des Deal-Alarms.

use crate::checker::{Engine, Scheduler};
use crate::model::Offer;
use crate::shops::{self, common::plz_coordinates, expert, ResolveError};
use crate::storage::{Settings, SourceConfig, Storage, Watch};
use crate::util::age_hours;
use crate::clientip::{self, ClientIp, LoginCheck, LoginGuard, TrustedProxies};
use crate::{activity, notifier, ratelimit, scan, warn};
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const TOKEN_MASK: &str = "••••";

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<Storage>,
    pub engine: Arc<Engine>,
    pub scheduler: Arc<Scheduler>,
    pub client: reqwest::Client,
    /// Basic Auth (Benutzer, Passwort), wenn `UI_PASSWORD` gesetzt ist
    pub auth: Option<(String, String)>,
    /// Reverse-Proxys, deren `X-Forwarded-For` ausgewertet wird (`TRUSTED_PROXIES`)
    pub trusted_proxies: Arc<TrustedProxies>,
    pub login_guard: Arc<LoginGuard>,
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, message.into())
    }
    fn not_found() -> Self {
        ApiError(StatusCode::NOT_FOUND, "Nicht gefunden".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Ermittelt die echte Besucher-IP (auch hinter Pangolin & Co.) und legt sie in die Request-Extensions.
async fn client_ip(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let extensions = request.extensions();
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
        // In Tests setzt MockConnectInfo die Verbindungsadresse
        .or_else(|| extensions.get::<MockConnectInfo<SocketAddr>>().map(|info| info.0.ip()))
        .unwrap_or_else(clientip::unspecified);
    let client = clientip::resolve(peer, request.headers(), &state.trusted_proxies);
    request.extensions_mut().insert(client);
    next.run(request).await
}

async fn basic_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some((user, password)) = &state.auth else { return next.run(request).await };
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let ip = request.extensions().get::<ClientIp>().map(|c| c.ip).unwrap_or_else(clientip::unspecified);
    if let LoginCheck::Locked(remaining) = state.login_guard.check(ip) {
        let minutes = remaining.as_secs().div_ceil(60);
        return (StatusCode::TOO_MANY_REQUESTS, format!("Zu viele fehlgeschlagene Anmeldungen – bitte in {minutes} Min. erneut versuchen"))
            .into_response();
    }
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v.trim()).ok())
        .and_then(|v| String::from_utf8(v).ok());
    if let Some((u, p)) = given.as_deref().and_then(|g| g.split_once(':')) {
        // Beide Vergleiche immer ausführen
        let (user_ok, password_ok) = (constant_time_eq(u, user), constant_time_eq(p, password));
        if user_ok && password_ok {
            state.login_guard.succeeded(ip);
            return next.run(request).await;
        }
        // Nur echte Fehlversuche zählen, nicht die erste Anfrage des Browsers ohne Zugangsdaten
        warn!("Anmeldung fehlgeschlagen (Benutzer „{u}“) von {ip}");
        if state.login_guard.failed(ip) {
            warn!("{ip} nach wiederholten Fehlversuchen für 15 Minuten gesperrt");
        }
    }
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"Deal-Alarm\"")], "Anmeldung erforderlich").into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML) }))
        .route("/static/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        .route("/static/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE_CSS) }))
        .route("/api/state", get(get_state))
        .route("/api/sources", get(|| async { Json(json!(shops::all().iter().map(|s| s.info()).collect::<Vec<_>>())) }))
        .route("/api/watches", post(create_watch))
        .route("/api/watches/{id}", put(edit_watch).delete(remove_watch))
        .route("/api/watches/{id}/stores", get(store_prices))
        .route("/api/settings", put(put_settings))
        .route("/api/check", post(check_now))
        .route("/api/logs", get(logs))
        .route("/api/test-notify", post(test_notify))
        .route("/api/euronics/stores/preview", post(euronics_preview))
        .route("/api/article/resolve", get(resolve_article))
        .route("/api/whoami", get(whoami))
        .layer(middleware::from_fn_with_state(state.clone(), basic_auth))
        // äußerste Schicht: Die Besucher-IP steht allen weiteren Schichten zur Verfügung
        .layer(middleware::from_fn_with_state(state.clone(), client_ip))
        .with_state(state)
}

/// Welche IP der Deal-Alarm für die aktuelle Anfrage sieht – zur Kontrolle der Proxy-Einstellung.
async fn whoami(State(state): State<AppState>, request: Request) -> Json<Value> {
    let client = request.extensions().get::<ClientIp>().copied();
    Json(json!({
        "ip": client.map(|c| c.ip.to_string()),
        "peer": client.map(|c| c.peer.to_string()),
        "via_proxy": client.is_some_and(|c| c.via_proxy),
        "header_ignored": client.is_some_and(|c| c.header_ignored),
        "trusted_proxies": state.trusted_proxies.spec(),
    }))
}

// ---------- Eingaben bereinigen ----------

fn number_of(value: &Value) -> Result<f64, ()> {
    match value {
        Value::Number(n) => n.as_f64().ok_or(()),
        Value::String(s) => {
            let s = s.trim().replace(',', ".");
            if s.is_empty() { Ok(0.0) } else { s.parse().map_err(|_| ()) }
        }
        Value::Bool(b) => Ok(*b as u8 as f64),
        Value::Null => Ok(0.0),
        _ => Err(()),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "true" | "1" | "on" | "ja"),
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Wert passend zum Typ des bisherigen Werts umwandeln (Zahl, Wahrheitswert, Text).
fn coerce_like(existing: &Value, input: &Value, key: &str) -> Result<Value, ApiError> {
    let invalid = || ApiError::bad(format!("Ungültiger Wert für {key}"));
    Ok(match existing {
        Value::Bool(_) => json!(truthy(input)),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!(number_of(input).map_err(|_| invalid())?.round() as i64),
        Value::Number(_) => json!(number_of(input).map_err(|_| invalid())?),
        _ => json!(text_of(input)),
    })
}

async fn json_body(request: Request) -> Result<Value, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20).await.map_err(|e| ApiError::bad(e.to_string()))?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Ungültiges JSON"))
}

fn merge_settings(current: &Settings, body: &Value) -> Result<Settings, ApiError> {
    let mut merged = serde_json::to_value(current).unwrap();
    for (key, value) in body.as_object().into_iter().flatten() {
        if key == "ntfy_token" && value == TOKEN_MASK {
            continue;
        }
        if let Some(existing) = merged.get(key).cloned() {
            merged[key] = coerce_like(&existing, value, key)?;
        }
    }
    serde_json::from_value(merged).map_err(|e| ApiError::bad(e.to_string()))
}

/// Übernimmt die Angaben aus einem Geräte-Formular (`sources` je Quelle: enabled, query, options).
fn apply_watch_values(watch: &mut Watch, body: &Value) -> Result<(), ApiError> {
    if let Some(v) = body.get("name") {
        watch.name = text_of(v);
    }
    if let Some(v) = body.get("ean") {
        watch.ean = text_of(v);
    }
    if let Some(v) = body.get("enabled") {
        watch.enabled = truthy(v);
    }
    if let Some(v) = body.get("target_price") {
        watch.target_price = number_of(v).map_err(|_| ApiError::bad("Ungültiger Wert für target_price"))?;
    }
    for (id, values) in body.get("sources").and_then(Value::as_object).into_iter().flatten() {
        let Some(source) = shops::get(id) else { continue };
        let cfg: &mut SourceConfig = watch.sources.entry(id.clone()).or_default();
        if let Some(v) = values.get("enabled") {
            cfg.enabled = truthy(v);
        }
        if let Some(v) = values.get("query") {
            cfg.query = text_of(v);
        }
        for def in source.options() {
            if let Some(v) = values.get("options").and_then(|o| o.get(def.key)) {
                let key = format!("{id}.{}", def.key);
                let value = match def.kind {
                    "bool" => json!(truthy(v)),
                    "number" => json!(number_of(v).map_err(|_| ApiError::bad(format!("Ungültiger Wert für {key}")))?),
                    _ => {
                        let text = text_of(v);
                        if def.choices.iter().any(|(c, _)| *c == text) { json!(text) } else { def.default.clone() }
                    }
                };
                cfg.options.insert(def.key.to_string(), value);
            }
        }
    }
    Ok(())
}

// ---------- Routen ----------

async fn get_state(State(app): State<AppState>) -> ApiResult {
    let state = app.storage.get_state();
    let mut data = serde_json::to_value(&state).unwrap();
    if !state.settings.ntfy_token.is_empty() {
        data["settings"]["ntfy_token"] = json!(TOKEN_MASK);
    }
    data["scheduler"]["running"] = json!(app.engine.is_running());
    data["activity"] = app.engine.status();
    data["limiters"] = json!(ratelimit::snapshots());
    for watch in data["watches"].as_array_mut().into_iter().flatten() {
        // Filialpreise gibt es gesammelt über /api/watches/<id>/stores
        watch.as_object_mut().map(|w| w.remove("store_prices"));
    }
    Ok(Json(data))
}

#[derive(serde::Deserialize)]
struct StoresQuery {
    shop: Option<String>,
}

async fn store_prices(State(app): State<AppState>, Path(id): Path<String>, Query(q): Query<StoresQuery>) -> ApiResult {
    let watch = app.storage.get_watch(&id).ok_or_else(ApiError::not_found)?;
    let settings = app.storage.get_settings();
    let shop = q.shop.unwrap_or_else(|| shops::all()[0].id().to_string());
    let source = shops::get(&shop).ok_or_else(|| ApiError::bad("Unbekannter Shop"))?;
    let state = watch.store_prices.get(&shop).cloned().unwrap_or_default();
    let user_coords = plz_coordinates(&settings.plz).await;
    let all_offers = scan::fresh_offers(&state, f64::INFINITY);
    let accepted: HashMap<String, Offer> = source
        .filter_offers(all_offers, &watch, &settings, user_coords)
        .into_iter()
        .filter_map(|o| Some((o.store_id.clone()?, o)))
        .collect();
    let max_age = settings.price_max_age_hours.max(1) as f64;
    let mut rows: Vec<(f64, Value)> = vec![];
    let mut unavailable = 0;
    for (store_id, entry) in &state.stores {
        let Some(offer) = &entry.offer else {
            unavailable += 1;
            continue;
        };
        let url = offer.url.clone().or_else(|| source.offer_url(&state, store_id));
        let distance = accepted.get(store_id).and_then(|o| o.distance_km).or(offer.distance_km);
        let mut row = serde_json::to_value(offer).unwrap();
        row["store_id"] = json!(store_id);
        row["store_name"] = json!(entry.name);
        row["checked"] = json!(entry.checked);
        row["stale"] = json!(age_hours(&entry.checked) > max_age);
        row["matches_filter"] = json!(accepted.contains_key(store_id));
        row["distance_km"] = json!(distance);
        row["url"] = json!(url);
        rows.push((offer.total_price, row));
    }
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Json(json!({
        "stores": rows.into_iter().map(|r| r.1).collect::<Vec<_>>(),
        "unavailable": unavailable,
        "target_price": watch.target_price,
        "label": source.label(),
    })))
}

async fn put_settings(State(app): State<AppState>, request: Request) -> ApiResult {
    let body = json_body(request).await?;
    let settings = merge_settings(&app.storage.get_settings(), &body)?;
    if settings.interval_minutes < 15 {
        return Err(ApiError::bad("Intervall muss mindestens 15 Minuten betragen"));
    }
    app.storage.set_settings(settings);
    app.scheduler.reschedule();
    Ok(Json(json!({"ok": true})))
}

async fn create_watch(State(app): State<AppState>, request: Request) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = json_body(request).await?;
    let mut watch = Watch::default();
    apply_watch_values(&mut watch, &body)?;
    if watch.ean.is_empty() && watch.sources.values().all(|c| c.query.is_empty()) {
        return Err(ApiError::bad("Bitte einen Artikel auswählen"));
    }
    if watch.target_price <= 0.0 {
        return Err(ApiError::bad("Bitte einen Wunschpreis angeben"));
    }
    let watch = app.storage.add_watch(watch);
    if watch.enabled {
        app.engine.run_all(Some(&watch.id)); // neues Gerät sofort prüfen
    }
    Ok((StatusCode::CREATED, Json(serde_json::to_value(watch).unwrap())))
}

async fn edit_watch(State(app): State<AppState>, Path(id): Path<String>, request: Request) -> ApiResult {
    let body = json_body(request).await?;
    let mut error = None;
    let watch = app.storage.update_watch(&id, |watch| {
        let mut candidate = watch.clone();
        match apply_watch_values(&mut candidate, &body) {
            Ok(()) => *watch = candidate,
            Err(e) => error = Some(e),
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    Ok(Json(serde_json::to_value(watch.ok_or_else(ApiError::not_found)?).unwrap()))
}

async fn remove_watch(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!({"ok": app.storage.delete_watch(&id)})))
}

async fn check_now(State(app): State<AppState>, request: Request) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = json_body(request).await.unwrap_or(Value::Null);
    let watch_id = body.get("watch_id").and_then(Value::as_str);
    app.engine.run_all(watch_id);
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true}))))
}

async fn logs(Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let after = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
    Ok(Json(json!(activity::recent(after, 200))))
}

async fn test_notify(State(app): State<AppState>) -> Result<Json<Value>, Response> {
    let settings = app.storage.get_settings();
    let message = notifier::Message {
        title: "Deal-Alarm Test",
        message: "Wenn du das liest, funktionieren die Benachrichtigungen. 🎉",
        click: None,
        tags: &["white_check_mark"],
    };
    match notifier::send(&app.client, &settings, message).await {
        Ok(id) => {
            let server = if settings.ntfy_server.trim().is_empty() { "https://ntfy.sh" } else { settings.ntfy_server.trim() };
            Ok(Json(json!({"ok": true, "id": id, "target": format!("{}/{}", server.trim_end_matches('/'), settings.ntfy_topic)})))
        }
        Err(error) => {
            crate::warn!("ntfy-Test fehlgeschlagen: {error}");
            Err((StatusCode::BAD_GATEWAY, Json(json!({"error": error}))).into_response())
        }
    }
}

/// Zeigt, welche Märkte mit den eingegebenen Einstellungen geprüft würden.
async fn euronics_preview(State(app): State<AppState>, request: Request) -> ApiResult {
    let body = json_body(request).await?;
    let mut overrides = Map::new();
    for key in ["euronics_stores", "euronics_use_directory", "plz"] {
        if let Some(v) = body.get("settings").and_then(|s| s.get(key)) {
            overrides.insert(key.to_string(), v.clone());
        }
    }
    let settings = merge_settings(&app.storage.get_settings(), &Value::Object(overrides))?;
    let euronics = shops::euronics();
    let manual = shops::euronics::parse_store_list(&settings.euronics_stores);
    let directory = if settings.euronics_use_directory { Some(euronics.load_directory().await) } else { None };
    let markets = euronics.select_markets(true, 0.0, &settings).await;
    let mut per_host: HashMap<&str, usize> = HashMap::new();
    for store in &markets {
        *per_host.entry(store.id.split('/').next().unwrap_or("")).or_default() += 1;
    }
    Ok(Json(json!({
        "manual": manual.iter().map(|s| json!({"name": s.name, "url": s.base})).collect::<Vec<_>>(),
        "directory_source": directory.as_ref().map(|d| d.source.clone()),
        "directory_count": directory.as_ref().map_or(0, |d| d.shops.len()),
        "total": markets.len(),
        "per_host": per_host,
    })))
}

/// Ordnet eine Eingabe (URL eines Shops, EAN oder Suchbegriff) einem Artikel in allen Quellen zu.
async fn resolve_article(Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let query = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    if query.is_empty() {
        return Err(ApiError::bad("Bitte Suchbegriff, URL oder EAN eingeben"));
    }
    let sources = shops::all();
    let origin = sources.iter().find(|s| s.matches_input(&query));
    let Some(origin) = origin else {
        // Freitext: erste Quelle mit Suche
        for source in sources {
            if let Some(result) = source.search(&query).await {
                let hits = result.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("Suche fehlgeschlagen: {e}")))?;
                return Ok(Json(json!({"results": hits.into_iter().take(10).collect::<Vec<_>>()})));
            }
        }
        return Ok(Json(json!({"results": []})));
    };
    let identity = match origin.identify(&query).await {
        Ok(identity) => identity,
        Err(ResolveError::NotFound(_)) if expert::is_ean(&query) => {
            // Nicht bei der Quelle – die anderen suchen trotzdem über die EAN
            return Ok(Json(json!({"article": {"title": "", "ean": query, "queries": {}, "found": {}}})));
        }
        Err(ResolveError::NotFound(m)) => return Err(ApiError(StatusCode::NOT_FOUND, m)),
        Err(ResolveError::Invalid(m)) => return Err(ApiError::bad(m)),
        Err(ResolveError::Failed(m)) => return Err(ApiError(StatusCode::BAD_GATEWAY, format!("Suche fehlgeschlagen: {m}"))),
    };
    let mut queries = Map::new();
    let mut found = Map::new();
    let mut title = identity.title.clone();
    queries.insert(origin.id().to_string(), json!(identity.query));
    found.insert(origin.id().to_string(), json!(true));
    if !identity.ean.is_empty() {
        for source in sources.iter().filter(|s| s.id() != origin.id()) {
            if let Ok(Some(hit)) = source.lookup_ean(&identity.ean).await {
                if !hit.title.is_empty() {
                    title = hit.title;
                }
                queries.insert(source.id().to_string(), json!(hit.query));
                found.insert(source.id().to_string(), json!(true));
            }
        }
    }
    Ok(Json(json!({"article": {"title": title, "ean": identity.ean, "queries": queries, "found": found}})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Offer, ShopResult, StoreEntry, StoreState};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct Harness {
        app: Router,
        storage: Arc<Storage>,
        _dir: tempfile::TempDir,
    }

    fn harness(auth: Option<(&str, &str)>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).unwrap());
        let state = AppState {
            storage: storage.clone(),
            // Ohne gestartete Arbeiter: Prüfungen werden nur eingereiht
            engine: Engine::new(storage.clone()),
            scheduler: Scheduler::new(),
            client: reqwest::Client::new(),
            auth: auth.map(|(u, p)| (u.to_string(), p.to_string())),
            trusted_proxies: Arc::new(TrustedProxies::default()),
            login_guard: Arc::new(LoginGuard::default()),
        };
        Harness { app: router(state), storage, _dir: dir }
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>, auth: Option<&str>) -> (StatusCode, Value) {
        let mut request = HttpRequest::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let request = request.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn new_watch_body() -> Value {
        json!({
            "name": "AirPods", "target_price": "199,00", "ean": "0195950543742",
            "sources": {
                "expert": {"enabled": true, "query": "", "options": {"all_branches": true, "max_distance": "30"}},
                "euronics": {"enabled": true, "query": ""},
                "amazon": {"enabled": true, "query": "https://www.amazon.de/dp/B0DEMO1234",
                           "options": {"warehouse_min_condition": "very_good", "include_marketplace": true}},
            }
        })
    }

    #[tokio::test]
    async fn api_roundtrip() {
        let h = harness(None);
        let (status, state) = call(&h.app, "GET", "/api/state", None, None).await;
        assert_eq!(status, 200);
        assert_eq!(state["watches"], json!([]));
        assert_eq!(state["settings"]["interval_minutes"], 120);
        assert_eq!(state["activity"].as_object().unwrap().keys().cloned().collect::<Vec<_>>(), ["amazon", "euronics", "expert"]);

        let (status, sources) = call(&h.app, "GET", "/api/sources", None, None).await;
        assert_eq!(status, 200);
        let ids: Vec<_> = sources.as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["expert", "euronics", "amazon"]);
        assert!(sources[2]["options"].as_array().unwrap().iter().any(|o| o["key"] == "warehouse_min_condition" && o["kind"] == "select"));

        // Validierung
        let (status, err) = call(&h.app, "POST", "/api/watches", Some(json!({"name": "x", "target_price": 5})), None).await;
        assert_eq!((status.as_u16(), err["error"].as_str().unwrap()), (400, "Bitte einen Artikel auswählen"));
        let (status, err) = call(&h.app, "POST", "/api/watches", Some(json!({"name": "x", "ean": "123"})), None).await;
        assert_eq!((status.as_u16(), err["error"].as_str().unwrap()), (400, "Bitte einen Wunschpreis angeben"));

        let (status, watch) = call(&h.app, "POST", "/api/watches", Some(new_watch_body()), None).await;
        assert_eq!(status, 201);
        assert_eq!(watch["target_price"], 199.0);
        assert_eq!(watch["sources"]["amazon"]["options"]["warehouse_min_condition"], "very_good");
        assert_eq!(watch["sources"]["expert"]["options"]["max_distance"], 30.0);
        let id = watch["id"].as_str().unwrap().to_string();

        // Neues Gerät wird bei allen Quellen eingereiht
        let (_, state) = call(&h.app, "GET", "/api/state", None, None).await;
        assert_eq!(state["watches"].as_array().unwrap().len(), 1);
        assert!(state["watches"][0].get("store_prices").is_none());
        for shop in ["expert", "euronics", "amazon"] {
            assert_eq!(state["activity"][shop]["queue"], json!([id]), "{shop}");
        }
        assert_eq!(state["scheduler"]["running"], true);

        // Bearbeiten: nur angegebene Felder, ungültige Auswahl fällt auf den Standard zurück
        let (status, edited) = call(&h.app, "PUT", &format!("/api/watches/{id}"),
            Some(json!({"enabled": false, "sources": {"amazon": {"options": {"warehouse_min_condition": "kaputt"}}}})), None).await;
        assert_eq!(status, 200);
        assert_eq!(edited["enabled"], false);
        assert_eq!(edited["name"], "AirPods");
        assert_eq!(edited["sources"]["amazon"]["options"]["warehouse_min_condition"], "good");
        assert_eq!(edited["sources"]["amazon"]["query"], "https://www.amazon.de/dp/B0DEMO1234");
        let (status, _) = call(&h.app, "PUT", "/api/watches/gibtsnicht", Some(json!({})), None).await;
        assert_eq!(status, 404);
        let (status, bad) = call(&h.app, "PUT", &format!("/api/watches/{id}"), Some(json!({"target_price": "abc"})), None).await;
        assert_eq!(status, 400);
        assert!(bad["error"].as_str().unwrap().contains("target_price"));

        let (_, deleted) = call(&h.app, "DELETE", &format!("/api/watches/{id}"), None, None).await;
        assert_eq!(deleted["ok"], true);
        assert!(h.storage.list_watches().is_empty());
    }

    #[tokio::test]
    async fn settings_are_coerced_validated_and_token_is_masked() {
        let h = harness(None);
        let (status, _) = call(&h.app, "PUT", "/api/settings", Some(json!({
            "ntfy_topic": "  geheim ", "ntfy_token": "tk_123", "interval_minutes": "90", "euronics_rate_per_min": 12.0,
            "scheduler_enabled": false, "unbekannt": 1,
        })), None).await;
        assert_eq!(status, 200);
        let settings = h.storage.get_settings();
        assert_eq!((settings.ntfy_topic.as_str(), settings.interval_minutes, settings.euronics_rate_per_min, settings.scheduler_enabled),
                   ("geheim", 90, 12, false));
        let (_, state) = call(&h.app, "GET", "/api/state", None, None).await;
        assert_eq!(state["settings"]["ntfy_token"], "••••");
        // Maske zurückschicken ändert das Token nicht
        call(&h.app, "PUT", "/api/settings", Some(json!({"ntfy_token": "••••", "plz": "20095"})), None).await;
        assert_eq!(h.storage.get_settings().ntfy_token, "tk_123");
        assert_eq!(h.storage.get_settings().plz, "20095");
        let (status, err) = call(&h.app, "PUT", "/api/settings", Some(json!({"interval_minutes": 5})), None).await;
        assert_eq!((status.as_u16(), err["error"].as_str().unwrap()), (400, "Intervall muss mindestens 15 Minuten betragen"));
        let (status, _) = call(&h.app, "PUT", "/api/settings", Some(json!({"interval_minutes": "x"})), None).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn healthz_is_reachable_with_password_but_ui_is_not() {
        let h = harness(Some(("admin", "geheim")));
        assert_eq!(call(&h.app, "GET", "/healthz", None, None).await.0, 200);
        assert_eq!(call(&h.app, "GET", "/api/state", None, None).await.0, 401);
        let good = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:geheim"));
        let bad = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:falsch"));
        assert_eq!(call(&h.app, "GET", "/api/state", None, Some(&good)).await.0, 200);
        assert_eq!(call(&h.app, "GET", "/api/state", None, Some(&bad)).await.0, 401);
        assert_eq!(call(&h.app, "GET", "/api/state", None, Some("Basic !!!")).await.0, 401);
    }

    #[tokio::test]
    async fn serves_frontend_files() {
        let h = harness(None);
        for (path, needle) in [("/", "Deal-Alarm"), ("/static/app.js", "use strict"), ("/static/style.css", "body")] {
            let response = h.app.clone().oneshot(HttpRequest::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), 200, "{path}");
            let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
            assert!(body.contains(needle), "{path}");
        }
    }

    #[tokio::test]
    async fn store_prices_table_marks_filter_matches_and_stale_entries() {
        let h = harness(None);
        let mut body = new_watch_body();
        body["target_price"] = json!(300);
        let (_, watch) = call(&h.app, "POST", "/api/watches", Some(body), None).await;
        let id = watch["id"].as_str().unwrap();
        let now = crate::util::now_iso();
        let entry = |name: &str, price: Option<f64>, condition: &str, checked: &str| StoreEntry {
            name: name.into(), checked: checked.into(), coordinates: None,
            offer: price.map(|p| Offer { price: p, total_price: p, condition: Some(condition.into()), ..Default::default() }),
        };
        let mut state = StoreState::default();
        state.stores.insert("new".into(), entry("Neuware", Some(399.0), "new", &now));
        state.stores.insert("warehouse_very_good".into(), entry("WH sehr gut", Some(280.0), "very_good", &now));
        state.stores.insert("warehouse_acceptable".into(), entry("WH akzeptabel", Some(200.0), "acceptable", "2020-01-01T00:00:00+00:00"));
        state.stores.insert("warehouse_like_new".into(), entry("WH wie neu", None, "like_new", &now));
        h.storage.save_result(id, "amazon", ShopResult::default(), None, Some(state));
        let (status, table) = call(&h.app, "GET", &format!("/api/watches/{id}/stores?shop=amazon"), None, None).await;
        assert_eq!(status, 200);
        assert_eq!(table["unavailable"], 1);
        let rows = table["stores"].as_array().unwrap();
        assert_eq!(rows.iter().map(|r| r["store_id"].as_str().unwrap()).collect::<Vec<_>>(), ["warehouse_acceptable", "warehouse_very_good", "new"]);
        let by_id = |id: &str| rows.iter().find(|r| r["store_id"] == id).unwrap().clone();
        assert_eq!(by_id("warehouse_very_good")["matches_filter"], true);
        assert_eq!(by_id("warehouse_acceptable")["matches_filter"], false); // unter „Sehr gut“
        assert_eq!(by_id("warehouse_acceptable")["stale"], true);
        assert_eq!(by_id("new")["matches_filter"], true);
        assert_eq!(call(&h.app, "GET", &format!("/api/watches/{id}/stores?shop=nope"), None, None).await.0, 400);
        assert_eq!(call(&h.app, "GET", "/api/watches/nope/stores", None, None).await.0, 404);
    }

    #[tokio::test]
    async fn resolve_validates_input_and_recognises_shop_urls() {
        let h = harness(None);
        assert_eq!(call(&h.app, "GET", "/api/article/resolve?q=", None, None).await.0, 400);
        // Euronics-URL ohne EAN
        let (status, err) = call(&h.app, "GET", "/api/article/resolve?q=https%3A%2F%2Fwww.euronics.de%2Fkategorie%2Fprodukt", None, None).await;
        assert_eq!(status, 400);
        assert!(err["error"].as_str().unwrap().contains("keine EAN"));
        // Amazon-URL ohne ASIN
        let (status, err) = call(&h.app, "GET", "/api/article/resolve?q=https%3A%2F%2Fwww.amazon.de%2Fgp%2Fhelp", None, None).await;
        assert_eq!(status, 400);
        assert!(err["error"].as_str().unwrap().contains("ASIN"));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd") && !constant_time_eq("abc", "ab") && !constant_time_eq("", "a"));
    }

    fn proxied_app(auth: Option<(&str, &str)>, peer: &str) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).unwrap());
        let state = AppState {
            storage: storage.clone(),
            engine: Engine::new(storage.clone()),
            scheduler: Scheduler::new(),
            client: reqwest::Client::new(),
            auth: auth.map(|(u, p)| (u.to_string(), p.to_string())),
            trusted_proxies: Arc::new(TrustedProxies::parse("172.18.0.0/16").unwrap()),
            login_guard: Arc::new(LoginGuard::new(3, std::time::Duration::from_secs(60), std::time::Duration::from_secs(60))),
        };
        let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
        (router(state).layer(MockConnectInfo(peer)), dir)
    }

    async fn whoami_with(app: &Router, forwarded: Option<&str>, auth: Option<&str>) -> (StatusCode, Value) {
        let mut request = HttpRequest::builder().uri("/api/whoami");
        if let Some(f) = forwarded {
            request = request.header("x-forwarded-for", f);
        }
        if let Some(a) = auth {
            request = request.header("authorization", a);
        }
        let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn whoami_shows_visitor_ip_behind_trusted_proxy() {
        let (app, _dir) = proxied_app(None, "172.18.0.4");
        let (_, body) = whoami_with(&app, Some("6.6.6.6, 198.51.100.7"), None).await;
        assert_eq!(body["ip"], "198.51.100.7");
        assert_eq!(body["peer"], "172.18.0.4");
        assert_eq!(body["via_proxy"], true);
        // Direkter Zugriff von außen: gefälschter Header wird ignoriert und gemeldet
        let (app, _dir) = proxied_app(None, "203.0.113.9");
        let (_, body) = whoami_with(&app, Some("198.51.100.7"), None).await;
        assert_eq!(body["ip"], "203.0.113.9");
        assert_eq!(body["header_ignored"], true);
    }

    #[tokio::test]
    async fn failed_logins_lock_only_the_visitor_not_the_proxy() {
        let (app, _dir) = proxied_app(Some(("admin", "geheim")), "172.18.0.4");
        let wrong = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:falsch"));
        let right = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("admin:geheim"));
        // Erste Anfrage des Browsers ohne Zugangsdaten zählt nicht als Fehlversuch
        assert_eq!(whoami_with(&app, Some("198.51.100.7"), None).await.0, StatusCode::UNAUTHORIZED);
        for _ in 0..3 {
            assert_eq!(whoami_with(&app, Some("198.51.100.7"), Some(&wrong)).await.0, StatusCode::UNAUTHORIZED);
        }
        // gesperrt – auch mit richtigem Passwort
        assert_eq!(whoami_with(&app, Some("198.51.100.7"), Some(&right)).await.0, StatusCode::TOO_MANY_REQUESTS);
        // andere Besucher hinter demselben Proxy sind nicht betroffen
        let (status, body) = whoami_with(&app, Some("198.51.100.8"), Some(&right)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ip"], "198.51.100.8");
    }
}
