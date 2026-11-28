//! Preisabfrage bei Euronics und media@home – Onlineshop und jeder Markt einzeln.
//!
//! Jeder Markt hat einen eigenen Shop mit eigenem Pfad, der Produktpfad ist überall gleich:
//!
//! ```text
//! https://www.euronics.de/<produktpfad>                ← Onlineshop
//! https://www.euronics.de/<markt>/<produktpfad>        ← Euronics-Markt
//! https://www.mediaathome.de/<markt>/<produktpfad>     ← media@home-Markt
//! ```
//!
//! Der Preis wird aus den strukturierten Produktdaten (schema.org) der Seite gelesen.

use super::common::{http_client, name_match, name_terms, plz_coordinates};
use super::{Confidence, Identity, Located, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Coords, Offer, ShopResult, StoreState, Suggestion};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::scan::{self, FetchError, Scannable};
use crate::storage::{Settings, Watch};
use crate::util::{age_hours, haversine_km, now_iso, parse_price, parse_price_value};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const BASE: &str = "https://www.euronics.de";
pub const ONLINE_STORE: &str = "online";
const HOSTS: &[(&str, &str)] = &[("www.euronics.de", "Euronics"), ("www.mediaathome.de", "media@home")];

// Händlerverzeichnis: Daten der Euronics-Händlersuche (Woosmap), gesammelt vom offenen
// Projekt All The Places (CC0). Mitgeliefert als Momentaufnahme, wöchentlich aktualisiert.
const MARKETS_BACKUP: &str = include_str!("../../assets/euronics_markets.json");
const ATP_LATEST: &str = "https://data.alltheplaces.xyz/runs/latest.json";
const ATP_SPIDER: &str = "euronics_de";
const MARKETS_REFRESH: Duration = Duration::from_secs(7 * 24 * 3600);

/// Euronics sitzt hinter einer AWS-WAF, die bei zu vielen Anfragen mit 403 antwortet
const THROTTLE_STATUS: &[u16] = &[403, 429, 503];

fn host_label(host: &str) -> Option<&'static str> {
    HOSTS.iter().find(|(h, _)| *h == host).map(|(_, l)| *l)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Location {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub postcode: String,
    #[serde(default)]
    pub city: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DirectoryShop {
    pub url: String,
    pub locations: Vec<Location>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Directory {
    pub source: String,
    pub shops: Vec<DirectoryShop>,
}

#[derive(Clone, Debug, Default)]
pub struct Market {
    pub id: String,
    pub name: String,
    pub base: String,
    pub slug: String,
    pub locations: Vec<Location>,
    pub coordinates: Option<Coords>,
    pub distance_km: Option<i64>,
}

impl Scannable for Market {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn coordinates(&self) -> Option<Coords> {
        self.coordinates
    }
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("Regex"))
}

fn jsonld_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"(?is)<script[^>]+type=["']application/ld\+json["'][^>]*>(.*?)</script>"#)
}
fn meta_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"(?i)<meta[^>]+(?:itemprop|property|name)=["']([^"']+)["'][^>]*content=["']([^"']*)["']"#)
}
fn meta_rev_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"(?i)<meta[^>]+content=["']([^"']*)["'][^>]*(?:itemprop|property|name)=["']([^"']+)["']"#)
}
fn title_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?is)<title[^>]*>(.*?)</title>")
}

fn unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Marktliste aus den Einstellungen: eine Markt-URL pro Zeile, optional 'Name | URL'.
pub fn parse_store_list(text: &str) -> Vec<Market> {
    let mut stores: Vec<Market> = vec![];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, url) = match line.rsplit_once('|') {
            Some((n, u)) => (n.trim(), u.trim()),
            None => ("", line),
        };
        let url = if url.contains("://") { url.to_string() } else { format!("https://{url}") };
        let Ok(parsed) = reqwest::Url::parse(&url) else { continue };
        let mut host = parsed.host_str().unwrap_or("").to_string();
        if host == "euronics.de" || host == "mediaathome.de" {
            host = format!("www.{host}");
        }
        let slug = parsed.path().trim_matches('/').split('/').next().unwrap_or("").to_string();
        let Some(label) = host_label(&host) else { continue };
        if slug.is_empty() {
            continue;
        }
        let id = format!("{host}/{slug}");
        if stores.iter().any(|s| s.id == id) {
            continue;
        }
        let title: String = slug
            .replace('-', " ")
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" ");
        stores.push(Market {
            name: if name.is_empty() { format!("{label} {title}") } else { name.to_string() },
            base: format!("https://{host}/{slug}"),
            id,
            slug,
            ..Default::default()
        });
    }
    stores
}

/// Wandelt die GeoJSON-Ausgabe von All The Places in die Shop-Liste um (ein Eintrag je Shop-URL).
pub fn markets_from_geojson(geojson: &Value, source: &str) -> Directory {
    let mut shops: BTreeMap<String, Vec<Location>> = BTreeMap::new();
    for feature in geojson["features"].as_array().into_iter().flatten() {
        let props = &feature["properties"];
        let url = props["website"].as_str().unwrap_or("").trim_end_matches('/');
        let (Some(lon), Some(lat)) = (feature["geometry"]["coordinates"][0].as_f64(), feature["geometry"]["coordinates"][1].as_f64())
        else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let text = |key: &str| props[key].as_str().unwrap_or("").to_string();
        let round6 = |v: f64| (v * 1e6).round() / 1e6;
        shops.entry(url.to_string()).or_default().push(Location {
            name: text("name"),
            street: text("addr:street_address"),
            postcode: text("addr:postcode"),
            city: text("addr:city"),
            lat: round6(lat),
            lon: round6(lon),
        });
    }
    Directory {
        source: source.to_string(),
        shops: shops
            .into_iter()
            .map(|(url, mut locations)| {
                locations.sort_by(|a, b| a.name.cmp(&b.name));
                DirectoryShop { url, locations }
            })
            .collect(),
    }
}

/// Produktpfad aus einer Euronics/media@home-URL – ein evtl. Markt-Präfix wird entfernt.
pub fn product_path(url: &str, stores: &[Market]) -> String {
    let url = url.trim();
    let (host, path) = match reqwest::Url::parse(url) {
        Ok(u) => (u.host_str().unwrap_or("").to_string(), u.path().to_string()),
        Err(_) => (String::new(), url.to_string()),
    };
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Bei media@home steht immer ein Markt vorne, bei Euronics nur in Marktshops
    if segments.len() > 1 && (stores.iter().any(|s| s.slug == segments[0]) || host.ends_with("mediaathome.de")) {
        segments.remove(0);
    }
    segments.join("/")
}

/// Lesbarer Titel aus dem letzten Pfadteil, z. B. '9150cc Series 9 Sport Trocken Nass Rasierer'.
pub fn title_from_path(path: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let slug = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let slug = re(&R, r"-\d{8,14}$").replace(slug, "");
    slug.split('-')
        .map(|w| {
            if w.chars().any(|c| c.is_ascii_digit()) {
                w.to_string()
            } else {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn ean_from_path(path: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"-(\d{13})$").captures(path).map(|c| c[1].to_string()).unwrap_or_default()
}

/// EANs gleich? (führende Nullen werden ignoriert, EAN-13 vs. GTIN-14)
pub fn same_ean(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && a.trim_start_matches('0') == b.trim_start_matches('0')
}

pub(crate) fn jsonld_items(page: &str) -> Vec<Value> {
    let mut items = vec![];
    for cap in jsonld_re().captures_iter(page) {
        let Ok(data) = serde_json::from_str::<Value>(unescape(cap[1].trim()).as_str()) else { continue };
        let mut stack: std::collections::VecDeque<Value> = match data {
            Value::Array(list) => list.into(),
            other => [other].into(),
        };
        while let Some(item) = stack.pop_front() {
            match item {
                Value::Object(ref obj) => {
                    if let Some(Value::Array(graph)) = obj.get("@graph") {
                        stack.extend(graph.iter().cloned());
                    }
                    items.push(item);
                }
                Value::Array(list) => stack.extend(list),
                _ => {}
            }
        }
    }
    items
}

fn is_type(item: &Value, name: &str) -> bool {
    match &item["@type"] {
        Value::String(s) => s == name,
        Value::Array(list) => list.iter().any(|t| t == name),
        _ => false,
    }
}

fn meta_values(page: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for cap in meta_re().captures_iter(page) {
        values.entry(cap[1].to_lowercase()).or_insert_with(|| unescape(&cap[2]));
    }
    for cap in meta_rev_re().captures_iter(page) {
        values.entry(cap[2].to_lowercase()).or_insert_with(|| unescape(&cap[1]));
    }
    values
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Product {
    pub title: String,
    pub price: Option<f64>,
    pub available: bool,
    pub ean: String,
    pub url: String,
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Liest Titel, Preis, Verfügbarkeit und EAN aus einer Produktseite.
pub fn parse_product_page(page: &str, url: &str) -> Option<Product> {
    static OOS: OnceLock<Regex> = OnceLock::new();
    static OOS_META: OnceLock<Regex> = OnceLock::new();
    for item in jsonld_items(page) {
        if !is_type(&item, "Product") {
            continue;
        }
        let mut offers = item["offers"].clone();
        if let Value::Array(list) = &offers {
            offers = list.first().cloned().unwrap_or(Value::Null);
        }
        let mut price = if is_type(&offers, "AggregateOffer") {
            parse_price_value(if offers["lowPrice"].is_null() || offers["lowPrice"] == "" { &offers["price"] } else { &offers["lowPrice"] })
        } else {
            parse_price_value(&offers["price"])
        };
        if price.is_none() && !offers["priceSpecification"].is_null() {
            let spec = match &offers["priceSpecification"] {
                Value::Array(list) => list.first().cloned().unwrap_or(Value::Null),
                other => other.clone(),
            };
            price = parse_price_value(&spec["price"]);
        }
        let availability = value_text(&offers["availability"]);
        let ean = [&item["gtin13"], &item["gtin"], &item["ean"]].iter().map(|v| value_text(v)).find(|s| !s.is_empty()).unwrap_or_default();
        return Some(Product {
            title: unescape(item["name"].as_str().unwrap_or("")),
            price,
            available: price.is_some() && !re(&OOS, r"(?i)OutOfStock|Discontinued|SoldOut").is_match(&availability),
            ean,
            url: offers["url"].as_str().filter(|u| !u.is_empty()).unwrap_or(url).to_string(),
        });
    }

    let meta = meta_values(page);
    let price = ["product:price:amount", "og:price:amount", "price"]
        .iter()
        .find_map(|k| meta.get(*k).filter(|v| !v.is_empty()))
        .and_then(|v| parse_price(v))?;
    let availability = meta.get("product:availability").or(meta.get("availability")).cloned().unwrap_or_default();
    let title = meta.get("og:title").cloned().filter(|t| !t.is_empty()).or_else(|| {
        title_re().captures(page).map(|c| unescape(c[1].trim()))
    });
    Some(Product {
        title: title.unwrap_or_default(),
        price: Some(price),
        available: !re(&OOS_META, r"(?i)out ?of ?stock|oos|nicht").is_match(&availability),
        ean: meta.get("gtin13").or(meta.get("product:ean")).cloned().unwrap_or_default(),
        url: url.to_string(),
    })
}

fn first_product_link(page: &str, base_url: &str) -> Option<String> {
    let base = reqwest::Url::parse(base_url).ok()?;
    for item in jsonld_items(page) {
        if !is_type(&item, "ItemList") {
            continue;
        }
        for element in item["itemListElement"].as_array().into_iter().flatten() {
            // Shopware liefert im ItemList teils `shopware.php?…`-Links statt des Produktpfads – die taugen nicht
            let link = element["url"].as_str().filter(|u| !u.is_empty()).or(element["item"]["url"].as_str()).filter(|u| !u.contains("shopware.php"));
            if let Some(joined) = link.and_then(|l| base.join(l).ok()) {
                return Some(joined.to_string());
            }
        }
    }
    None
}

/// Alle Produktlinks einer Ergebnisseite (Pfad endet auf eine EAN), in Reihenfolge, ohne Doppelte.
fn product_links(page: &str, base_url: &str) -> Vec<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let Ok(base) = reqwest::Url::parse(base_url) else { return vec![] };
    let mut links: Vec<String> = vec![];
    for cap in re(&R, r#"(?i)href=["']([^"'#?]*-\d{13})/?(?:[?#][^"']*)?["']"#).captures_iter(page) {
        if let Ok(url) = base.join(&unescape(&cap[1])) {
            let url = url.to_string();
            if !links.contains(&url) {
                links.push(url);
            }
        }
    }
    if let Some(first) = first_product_link(page, base_url) {
        if !links.contains(&first) {
            links.push(first);
        }
    }
    links
}

/// Ab diesem Namens-Abgleich wird ein Treffer trotz abweichender EAN automatisch geprüft
/// (Modellnummer stimmt überein, die meisten übrigen Wörter auch).
const NAME_AUTO_ACCEPT: f64 = 0.75;

/// HTTP-Client mit dem Cookie aus den Einstellungen (nur für die Suche).
fn search_client(cookie: &str) -> reqwest::Client {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, COOKIE, USER_AGENT};
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(super::common::USER_AGENT));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE,de;q=0.9,en;q=0.5"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"));
    if let Ok(v) = HeaderValue::from_str(cookie) {
        headers.insert(COOKIE, v);
    }
    reqwest::Client::builder().default_headers(headers).connect_timeout(Duration::from_secs(15)).build().expect("HTTP-Client")
}

/// Kennzeichen von Fehlermeldungen, die von der AWS-WAF-Sperre stammen.
const WAF_PREFIX: &str = "[WAF]";

/// Challenge-Seite der AWS WAF (Stand 28.11.2026: HTTP 405, ~2 KB, Skripte von *.awswaf.com).
pub fn is_waf_challenge(status: u16, body: &str) -> bool {
    (status == 405 || status == 202 || body.len() < 20_000) && (body.contains("awswaf") || body.contains("ob Sie ein Mensch sind"))
}

/// Ergebnis der Zuordnung eines Geräts zu einem Euronics-Produkt.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: String,
    /// Hersteller-EAN aus den Produktdaten (`gtin`) – die Zahl am Ende des Pfads ist die Euronics-Artikelnummer
    pub gtin: String,
    pub title: String,
    pub confidence: Confidence,
}

/// Bundles („… inkl. Soundbar“) sind nicht das gesuchte Gerät, auch wenn die Modellnummer passt.
fn is_bundle(title: &str, wanted: &str) -> bool {
    let t = title.to_lowercase();
    let w = wanted.to_lowercase();
    ["inkl", "bundle", " set ", "+ "].iter().any(|k| t.contains(k) && !w.contains(k))
}

pub struct Euronics {
    base: String,
    limiter_prefix: String,
    client: reqwest::Client,
    directory: Mutex<Option<(Instant, Arc<Directory>)>>,
    /// Euronics-Artikel über das idealo-Angebot suchen (aus in Tests mit eigener Basis)
    idealo_fallback: bool,
}

impl Default for Euronics {
    fn default() -> Self {
        Euronics::new(BASE, "")
    }
}

fn to_offer(product: Option<&Product>) -> Option<Offer> {
    let product = product?;
    let price = product.price.filter(|_| product.available)?;
    Some(Offer {
        price,
        shipping: None, // Versandkosten stehen nicht in den Produktdaten
        total_price: price,
        availability: Some("verfügbar".into()),
        title: Some(product.title.clone()),
        ..Default::default()
    })
}

impl Euronics {
    pub fn new(base: &str, limiter_prefix: &str) -> Self {
        Euronics {
            base: base.trim_end_matches('/').to_string(),
            limiter_prefix: limiter_prefix.to_string(),
            client: http_client("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
            directory: Mutex::new(None),
            idealo_fallback: base.trim_end_matches('/') == BASE,
        }
    }

    /// Ein Limiter pro Domain – euronics.de und mediaathome.de drosseln unabhängig voneinander.
    fn limiter(&self, settings: &Settings, url: &str) -> Arc<AdaptiveLimiter> {
        let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "www.euronics.de".into());
        ratelimit::get_limiter(&format!("{}{host}", self.limiter_prefix), settings.euronics_rate_per_min.max(1) as f64, 1.0)
    }

    async fn get(
        client: &reqwest::Client,
        limiter: &AdaptiveLimiter,
        url: &str,
        allow_missing: bool,
    ) -> Result<Option<(String, String)>, FetchError> {
        let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let response = ratelimit::get(client, limiter, url, &[], THROTTLE_STATUS, 2, Duration::from_secs(30))
            .await
            .map_err(|_| {
                FetchError::RateLimited(format!(
                    "{host} blockiert oder drosselt die Anfragen (HTTP 403/429). \
                     Rechenzentrums-IPs werden oft komplett gesperrt – am besten zuhause laufen lassen."
                ))
            })?;
        if allow_missing && matches!(response.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let ok = response.status().is_success();
        let body = response.text().await?;
        crate::debug::capture("euronics", &final_url, status, &body);
        if is_waf_challenge(status, &body) {
            return Err(FetchError::RateLimited(format!("{WAF_PREFIX} {host}: Bot-Sperre (AWS WAF, HTTP {status})")));
        }
        if !ok {
            return Err(FetchError::Other(format!("HTTP {status} von {host}")));
        }
        Ok(Some((body, final_url)))
    }

    async fn download_directory(&self) -> Result<Directory, String> {
        let client = reqwest::Client::new();
        let latest: Value = client.get(ATP_LATEST).timeout(Duration::from_secs(30)).send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        let output_url = latest["output_url"].as_str().ok_or("output_url fehlt")?;
        let run_base = output_url.rsplit_once('/').map(|x| x.0).unwrap_or(output_url);
        let geojson: Value = client.get(format!("{run_base}/output/{ATP_SPIDER}.geojson")).timeout(Duration::from_secs(60)).send().await
            .map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        let run = latest["run_id"].as_str().unwrap_or("?");
        let data = markets_from_geojson(&geojson, &format!("All The Places (CC0), Lauf {run}"));
        if data.shops.len() < 100 {
            return Err(format!("Nur {} Shops geliefert", data.shops.len()));
        }
        Ok(data)
    }

    /// Händlerverzeichnis (wöchentlich aktualisiert, mitgelieferte Momentaufnahme als Fallback).
    pub async fn load_directory(&self) -> Arc<Directory> {
        let mut cache = self.directory.lock().await;
        if let Some((time, data)) = cache.as_ref() {
            if time.elapsed() < MARKETS_REFRESH {
                return data.clone();
            }
        }
        let data = match self.download_directory().await {
            Ok(d) => Arc::new(d),
            Err(e) => {
                crate::info!("Händlerverzeichnis nicht aktualisiert ({e}) – nutze {}", if cache.is_some() { "letzten Stand" } else { "mitgelieferte Liste" });
                match cache.as_ref() {
                    Some((_, old)) => old.clone(),
                    None => Arc::new(serde_json::from_str(MARKETS_BACKUP).expect("euronics_markets.json")),
                }
            }
        };
        *cache = Some((Instant::now(), data.clone()));
        data
    }

    pub async fn directory_stores(&self) -> Vec<Market> {
        let directory = self.load_directory().await;
        directory
            .shops
            .iter()
            .filter_map(|shop| {
                let mut store = parse_store_list(&shop.url).into_iter().next()?;
                let first = shop.locations.first()?;
                store.name = format!(
                    "{}{}",
                    first.name,
                    if shop.locations.len() > 1 { format!(" (+{} Filialen)", shop.locations.len() - 1) } else { String::new() }
                );
                store.locations = shop.locations.clone();
                Some(store)
            })
            .collect()
    }

    /// Alle zu prüfenden Märkte: Verzeichnis + eigene Einträge, optional auf einen Umkreis begrenzt.
    pub async fn select_markets(&self, all_stores: bool, max_distance: f64, settings: &Settings) -> Vec<Market> {
        if !all_stores {
            return vec![];
        }
        let mut markets: BTreeMap<String, Market> = BTreeMap::new();
        if settings.euronics_use_directory {
            for store in self.directory_stores().await {
                markets.insert(store.id.clone(), store);
            }
        }
        for store in parse_store_list(&settings.euronics_stores) {
            match markets.get_mut(&store.id) {
                Some(existing) => existing.name = store.name, // eigener Name hat Vorrang
                None => {
                    markets.insert(store.id.clone(), store);
                }
            }
        }
        let user_coords = plz_coordinates(&settings.plz).await;
        let mut selected = vec![];
        for mut store in markets.into_values() {
            if let Some(first) = store.locations.first() {
                match user_coords {
                    Some(user) => {
                        let nearest = store
                            .locations
                            .iter()
                            .min_by(|a, b| haversine_km(user, (a.lat, a.lon)).total_cmp(&haversine_km(user, (b.lat, b.lon))))
                            .unwrap();
                        store.coordinates = Some((nearest.lat, nearest.lon));
                        store.distance_km = Some(haversine_km(user, (nearest.lat, nearest.lon)).round() as i64);
                    }
                    None => store.coordinates = Some((first.lat, first.lon)),
                }
            }
            if max_distance > 0.0 && user_coords.is_some() && store.distance_km.map_or(f64::INFINITY, |d| d as f64) > max_distance {
                continue;
            }
            selected.push(store);
        }
        selected.sort_by_key(|s| s.distance_km.unwrap_or(i64::MAX));
        selected
    }

    /// Suchseite im Onlineshop: (Seite, URL nach Weiterleitung), `None` bei Bot-Sperre (AWS WAF).
    ///
    /// Die Suche ist bei Euronics hinter der WAF; mit einem Cookie aus dem eigenen Browser
    /// (Einstellung „Euronics-Suche: Cookie“) geht sie trotzdem.
    async fn search_page(&self, settings: &Settings, term: &str) -> Result<Option<(String, String)>, CheckError> {
        let search_url = format!("{}/search?sSearch={}", self.base, urlencoding::encode(term));
        let limiter = self.limiter(settings, &search_url);
        let cookie = settings.euronics_cookie.trim();
        let client = if cookie.is_empty() { self.client.clone() } else { search_client(cookie) };
        match Self::get(&client, &limiter, &search_url, false).await {
            Ok(page) => Ok(page),
            Err(FetchError::RateLimited(m)) if m.starts_with(WAF_PREFIX) => {
                crate::info!("Euronics-Suche: Bot-Sperre (AWS WAF){}", if cookie.is_empty() { "" } else { " – Cookie abgelaufen?" });
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Produktdaten der Onlineshop-Seite (dort steht die Hersteller-EAN als `gtin`).
    async fn product(&self, settings: &Settings, path: &str) -> Result<Option<Product>, CheckError> {
        let url = format!("{}/{path}", self.base);
        let limiter = self.limiter(settings, &url);
        Ok(Self::get(&self.client, &limiter, &url, true).await?.and_then(|(page, final_url)| parse_product_page(&page, &final_url)))
    }

    /// Kandidaten einer Suchseite: (Pfad, Titel). Führte die Suche direkt aufs Produkt, ist das der einzige.
    fn candidates(page: &str, final_url: &str, stores: &[Market]) -> Vec<(String, String)> {
        if let Some(product) = parse_product_page(page, final_url) {
            let path = product_path(final_url.split('?').next().unwrap(), stores);
            let title = if product.title.is_empty() { title_from_path(&path) } else { product.title };
            return vec![(path, title)];
        }
        product_links(page, final_url)
            .into_iter()
            .map(|l| {
                let path = product_path(&l, stores);
                let title = title_from_path(&path);
                (path, title)
            })
            .collect()
    }

    /// Produktpfad aus der hinterlegten URL, per Suche (EAN, dann Name) oder über idealo.
    ///
    /// Ob es dasselbe Gerät ist, entscheidet die Hersteller-EAN der Produktseite (`gtin`). Ohne
    /// passende EAN wird ein Treffer nur übernommen, wenn das Gerät keine EAN hat und Name und
    /// Modellnummer eindeutig passen (keine Bundles). Sonst wird er als Vorschlag gemeldet.
    pub async fn resolve(&self, watch: &Watch, settings: &Settings, stores: &[Market], state: &StoreState) -> Result<Resolved, CheckError> {
        let query = watch.source("euronics").query.trim().to_string();
        let ean = watch.ean.trim().to_string();
        if query.contains("euronics.de") || query.contains("mediaathome.de") {
            let path = product_path(query.split('#').next().unwrap().split('?').next().unwrap(), stores);
            let cached_gtin = state.meta_str("gtin").filter(|_| state.meta_str("product_path") == Some(path.as_str())).map(str::to_string);
            let (gtin, title) = match cached_gtin {
                Some(g) => (g, state.meta_str("title").unwrap_or_default().to_string()),
                None => match self.product(settings, &path).await {
                    Ok(Some(p)) => (p.ean, p.title),
                    _ => (String::new(), title_from_path(&path)),
                },
            };
            if !ean.is_empty() && !gtin.is_empty() && !same_ean(&gtin, &ean) {
                crate::info!("Euronics-URL hat EAN {gtin}, das Gerät EAN {ean} (Händler-Variante) – geprüft wird die eingetragene URL");
            }
            return Ok(Resolved { path, gtin, title, confidence: Confidence::Ean });
        }
        let term = if query.is_empty() { ean.clone() } else { query.clone() };
        let names = if query.is_empty() { name_terms(watch, "euronics") } else { vec![] };
        if term.is_empty() && names.is_empty() {
            return Err(CheckError::Other("Keine Euronics-URL, EAN oder Gerätename hinterlegt".into()));
        }
        if Self::cached(watch, state) {
            if let Some(path) = state.meta_str("product_path") {
                return Ok(Resolved {
                    path: path.to_string(),
                    gtin: state.meta_str("gtin").unwrap_or_default().to_string(),
                    title: state.meta_str("title").unwrap_or_default().to_string(),
                    confidence: Confidence::Ean,
                });
            }
        }
        let mut suggestion: Option<(String, String, String)> = None; // (Pfad, Titel, EAN)
        let mut tried = vec![];
        let mut blocked = false;

        // Höchstens 3 Produktseiten je Suche prüfen (jede kostet eine Anfrage)
        const VERIFY: usize = 3;

        if !term.is_empty() {
            tried.push(term.clone());
            match self.search_page(settings, &term).await? {
                None => blocked = true,
                Some((page, final_url)) => {
                    let candidates = Self::candidates(&page, &final_url, stores);
                    if !query.is_empty() {
                        // eigener Suchbegriff: erster Treffer
                        if let Some((path, title)) = candidates.into_iter().next() {
                            return Ok(Resolved { path, gtin: String::new(), title, confidence: Confidence::Name });
                        }
                    } else {
                        for (path, title) in candidates.into_iter().take(VERIFY) {
                            let Some(product) = self.product(settings, &path).await? else { continue };
                            if same_ean(&product.ean, &ean) {
                                let title = if product.title.is_empty() { title } else { product.title };
                                return Ok(Resolved { path, gtin: product.ean, title, confidence: Confidence::Ean });
                            }
                            // Produktseite ohne EAN: Treffer der EAN-Suche gilt, wenn der Name passt
                            let shown = if product.title.is_empty() { title.clone() } else { product.title.clone() };
                            if product.ean.is_empty() && name_match(&watch.name, &shown) >= super::common::NAME_MATCH_MIN {
                                return Ok(Resolved { path, gtin: String::new(), title: shown, confidence: Confidence::Name });
                            }
                            if suggestion.is_none() && !is_bundle(&title, &watch.name) {
                                suggestion = Some((path, title, product.ean));
                            }
                        }
                    }
                }
            }
        }

        if !blocked {
            for name in &names {
                tried.push(name.clone());
                let Some((page, final_url)) = self.search_page(settings, name).await? else {
                    blocked = true;
                    break;
                };
                let mut ranked: Vec<((String, String), f64)> = Self::candidates(&page, &final_url, stores)
                    .into_iter()
                    .filter(|(_, t)| !is_bundle(t, name))
                    .map(|c| {
                        let score = name_match(name, &c.1);
                        (c, score)
                    })
                    .filter(|(_, score)| *score >= super::common::NAME_MATCH_MIN)
                    .collect();
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
                for ((path, title), score) in ranked.into_iter().take(VERIFY) {
                    let product = self.product(settings, &path).await?;
                    let gtin = product.as_ref().map(|p| p.ean.clone()).unwrap_or_default();
                    let title = product.map(|p| p.title).filter(|t| !t.is_empty()).unwrap_or(title);
                    if !ean.is_empty() && same_ean(&gtin, &ean) {
                        return Ok(Resolved { path, gtin, title, confidence: Confidence::Ean });
                    }
                    // EAN unbekannt (Gerät oder Produktseite ohne EAN) und Name/Modellnummer eindeutig
                    if (ean.is_empty() || gtin.is_empty()) && score >= NAME_AUTO_ACCEPT {
                        crate::info!("Euronics · {}: über Namenssuche „{name}“ zugeordnet: {title}", watch.name);
                        return Ok(Resolved { path, gtin, title, confidence: Confidence::Name });
                    }
                    if suggestion.is_none() {
                        suggestion = Some((path, title, gtin));
                    }
                }
            }
        }

        // Suche gesperrt oder erfolglos: Euronics-Angebot bei idealo
        if self.idealo_fallback && suggestion.is_none() && (blocked || !ean.is_empty()) && query.is_empty() {
            let idealo = super::idealo::Idealo::default();
            if let Some(url) = idealo.merchant_url(watch, settings, "euronics", "euronics.de").await {
                let path = product_path(&url, stores);
                if let Some(product) = self.product(settings, &path).await? {
                    if ean.is_empty() || same_ean(&product.ean, &ean) {
                        crate::info!("Euronics · {}: über das idealo-Angebot zugeordnet: {}", watch.name, product.title);
                        return Ok(Resolved { path, gtin: product.ean, title: product.title, confidence: Confidence::Ean });
                    }
                    suggestion = Some((path, product.title, product.ean));
                }
            }
        }

        let Some((path, title, found)) = suggestion else {
            if blocked {
                return Err(CheckError::lookup(
                    "Die Euronics-Suche ist durch eine Bot-Sperre (AWS WAF) geschützt und das Gerät war auch über idealo nicht \
                     zu finden. Bitte den Euronics-Link beim Gerät eintragen (Produkt- und Marktseiten funktionieren) oder in den \
                     Einstellungen ein Cookie für die Euronics-Suche hinterlegen.",
                ));
            }
            return Err(CheckError::lookup(format!(
                "Bei Euronics nicht gefunden (Suche nach {} ohne passenden Treffer) – falls vorhanden, Produkt-URL im Gerät eintragen",
                tried.iter().map(|t| format!("„{t}“")).collect::<Vec<_>>().join(", ")
            )));
        };
        // Hersteller wie Samsung vergeben je Händler eigene Modellnummern und EANs (expert S94 = Euronics S93).
        Err(CheckError::Lookup {
            message: format!(
                "Euronics führt {} nicht. Gefunden wurde „{title}“ (EAN {}) – falls das dasselbe Gerät ist \
                 (Händler-Variante), als Euronics-Produkt übernehmen.",
                if ean.is_empty() { "dieses Gerät eindeutig".to_string() } else { format!("EAN {ean}") },
                if found.is_empty() { "?" } else { &found }
            ),
            suggestion: Some(Suggestion { url: format!("{}/{path}", self.base), title }),
        })
    }

    /// Gilt die gemerkte Zuordnung aus der letzten Suche noch (gleiche Angaben, jünger als eine Woche)?
    fn cached(watch: &Watch, state: &StoreState) -> bool {
        state.meta_str("product_path").is_some()
            && state.meta_str("query") == Some(Self::cache_key(watch).as_str())
            && state.meta_str("resolved_at").is_none_or(|t| age_hours(t) < super::REDISCOVER_HOURS)
    }

    /// Wonach gesucht wurde – ändert sich das, wird neu gesucht.
    fn cache_key(watch: &Watch) -> String {
        let q = watch.source("euronics").query.trim().to_string();
        if !q.is_empty() {
            q
        } else if !watch.ean.trim().is_empty() {
            format!("{}|{}", watch.ean.trim(), watch.name.trim())
        } else {
            watch.name.trim().to_string()
        }
    }
}

#[async_trait]
impl Source for Euronics {
    fn id(&self) -> &'static str {
        "euronics"
    }
    fn label(&self) -> &'static str {
        "Euronics"
    }
    fn query_label(&self) -> &'static str {
        "Produkt-URL (Onlineshop- oder Markt-URL, leer = automatisch über EAN)"
    }
    fn query_placeholder(&self) -> &'static str {
        "https://www.euronics.de/…"
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![
            OptionDef::bool("all_stores", "Alle Märkte einzeln durchsuchen (nicht nur den Onlineshop)", true),
            OptionDef::number("max_distance", "Umkreis um deine PLZ (km, 0 = ganz Deutschland)", 0.0),
        ]
    }

    fn matches_input(&self, input: &str) -> bool {
        input.contains("euronics.de") || input.contains("mediaathome.de")
    }

    async fn identify(&self, input: &str) -> Result<Identity, ResolveError> {
        let path = product_path(input.split('#').next().unwrap().split('?').next().unwrap(), &parse_store_list(input));
        if ean_from_path(&path).is_empty() {
            return Err(ResolveError::Invalid("Das ist keine Euronics-Produktseite – bitte den Link zum Produkt verwenden".into()));
        }
        // Die Zahl am Ende des Pfads ist die Euronics-Artikelnummer; die Hersteller-EAN steht auf der Produktseite
        // (Produktseiten sind ohne Bot-Sperre abrufbar). Klappt das nicht, suchen die anderen Shops über den Namen.
        let product = self.product(&Settings::default(), &path).await.ok().flatten();
        let title = product.as_ref().map(|p| p.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| title_from_path(&path));
        Ok(Identity { title, ean: product.map(|p| p.ean).unwrap_or_default(), query: format!("{BASE}/{path}") })
    }

    fn offer_url(&self, _state: &StoreState, _store_id: &str) -> Option<String> {
        None
    }

    async fn locate(&self, watch: &Watch, settings: &Settings) -> Result<Option<Located>, String> {
        let stores = parse_store_list(&settings.euronics_stores);
        match self.resolve(watch, settings, &stores, &StoreState::default()).await {
            Ok(r) => Ok(Some(Located {
                query: format!("{BASE}/{}", r.path),
                title: if r.title.is_empty() { title_from_path(&r.path) } else { r.title },
                confidence: r.confidence,
            })),
            Err(CheckError::Lookup { suggestion: Some(s), .. }) => {
                let path = product_path(&s.url, &stores);
                Ok(Some(Located { query: format!("{BASE}/{path}"), title: s.title, confidence: Confidence::Variant }))
            }
            Err(CheckError::Lookup { .. }) => Ok(None),
            Err(e) => Err(e.message().to_string()),
        }
    }

    async fn check(
        &self,
        watch: &Watch,
        settings: &Settings,
        state: StoreState,
        progress: &mut crate::activity::ScanReporter,
    ) -> Result<(ShopResult, StoreState), CheckError> {
        let cfg = watch.source("euronics");
        let markets = self.select_markets(cfg.opt_bool("all_stores", true), cfg.opt_f64("max_distance", 0.0), settings).await;
        let from_cache = Self::cached(watch, &state);
        let resolved = self.resolve(watch, settings, &markets, &state).await?;
        let path = resolved.path.clone();
        crate::info!(
            "Euronics · {}: Produkt {}/{path}, {} Märkte + Onlineshop, Tempo {} Anfragen/Min. je Domain",
            watch.name, self.base, markets.len(), settings.euronics_rate_per_min
        );
        let state = if state.meta_str("product_path") != Some(path.as_str()) { StoreState::default() } else { state };
        let mut stores = vec![Market {
            id: ONLINE_STORE.into(),
            name: "Euronics Onlineshop".into(),
            base: self.base.clone(),
            ..Default::default()
        }];
        stores.extend(markets);
        for (host, _) in HOSTS {
            self.limiter(settings, &format!("https://{host}/")).start_scan();
        }
        self.limiter(settings, &self.base).start_scan();

        let client = self.client.clone();
        let path_arc = Arc::new(path.clone());
        let limiters: Arc<BTreeMap<String, Arc<AdaptiveLimiter>>> = Arc::new(
            stores
                .iter()
                .map(|s| (s.base.clone(), self.limiter(settings, &s.base)))
                .collect(),
        );
        let fetch = move |store: Market| {
            let (client, path, limiters) = (client.clone(), path_arc.clone(), limiters.clone());
            async move {
                let url = format!("{}/{path}", store.base);
                let limiter = &limiters[&store.base];
                let Some((page, final_url)) = Self::get(&client, limiter, &url, true).await? else {
                    return Ok(None); // Produkt nicht im Sortiment des Markts
                };
                let final_path = reqwest::Url::parse(&final_url).map(|u| u.path().to_string()).unwrap_or_default();
                if store.id != ONLINE_STORE && !format!("{final_path}/").contains(&format!("/{}/", store.slug)) {
                    return Ok(None); // Weiterleitung weg vom Markt: nicht im Sortiment
                }
                let product = parse_product_page(&page, &url);
                if store.id == ONLINE_STORE {
                    match &product {
                        None => crate::warn!("Euronics Onlineshop: Seite ohne lesbare Produktdaten ({final_url}) – Preise können so nicht gelesen werden"),
                        Some(p) => crate::info!(
                            "Euronics Onlineshop: {} – {}",
                            if p.title.is_empty() { "Produkt" } else { &p.title },
                            match p.price { Some(price) if p.available => format!("{price:.2} €"), _ => "nicht verfügbar".into() }
                        ),
                    }
                }
                Ok::<_, FetchError>(to_offer(product.as_ref()).map(|mut offer| {
                    offer.url = Some(url);
                    offer.distance_km = store.distance_km;
                    offer
                }))
            }
        };
        let budget = Some(settings.euronics_stores_per_cycle).filter(|b| *b > 0).map(|b| b as usize);
        let (mut state, stats) = scan::run(&stores, state, fetch, budget, HOSTS.len(), Some(progress), |_| true).await;

        let offers = scan::fresh_offers(&state, settings.price_max_age_hours.max(1) as f64);
        let best = offers.first();
        let title = offers.iter().find_map(|o| o.title.clone().filter(|t| !t.is_empty())).or_else(|| state.meta_str("title").map(str::to_string)).unwrap_or_default();
        let detail = match best {
            Some(b) => format!("{}{}", b.store_name.clone().unwrap_or_default(), if offers.len() > 1 { format!(", {} Märkte mit Angebot", offers.len()) } else { String::new() }),
            None => "In keinem Markt verfügbar".into(),
        };
        if stats.checked == 0 && stats.planned > 0 && stats.aborted {
            return Err(CheckError::RateLimited(
                "Euronics blockiert oder drosselt die Anfragen (HTTP 403/429) – \
                 Rechenzentrums-IPs werden oft komplett gesperrt, am besten zuhause laufen lassen."
                    .into(),
            ));
        }
        let result = ShopResult {
            title: title.clone(),
            url: best.and_then(|b| b.url.clone()).unwrap_or_else(|| format!("{}/{path}", self.base)),
            ean: resolved.gtin.clone(),
            price: best.map(|b| b.total_price),
            available: best.is_some(),
            detail,
            scan: Some(stats),
            ..Default::default()
        };
        let query = Self::cache_key(watch);
        let resolved_at = state.meta_str("resolved_at").filter(|_| from_cache).map(str::to_string).unwrap_or_else(now_iso);
        state.meta.insert("resolved_at".into(), json!(resolved_at));
        state.meta.insert("product_path".into(), json!(path));
        state.meta.insert("gtin".into(), json!(resolved.gtin));
        state.meta.insert("title".into(), json!(title));
        state.meta.insert("query".into(), json!(query));
        Ok((result, state))
    }
}

impl From<FetchError> for CheckError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::RateLimited(m) => CheckError::RateLimited(m),
            FetchError::Other(m) => CheckError::Other(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, response::Redirect, routing::get, Router};
    use std::collections::HashMap;

    fn jsonld_page(ld: Value) -> String {
        format!(r#"<html><script type="application/ld+json">{ld}</script></html>"#)
    }

    #[test]
    fn reads_jsonld_product() {
        let page = jsonld_page(json!({"@context": "https://schema.org", "@type": "Product", "name": "Apple AirPods Pro 3",
            "gtin13": "0195950543742", "offers": {"@type": "Offer", "price": "219.00", "priceCurrency": "EUR", "availability": "https://schema.org/InStock"}}));
        let product = parse_product_page(&page, "https://www.euronics.de/p").unwrap();
        assert_eq!(product.price, Some(219.0));
        assert!(product.available);
        assert_eq!(product.ean, "0195950543742");
    }

    #[test]
    fn reads_graph_and_out_of_stock() {
        let page = jsonld_page(json!({"@graph": [{"@type": "WebSite"}, {"@type": ["Product"], "name": "X",
            "offers": [{"@type": "Offer", "price": 99, "availability": "https://schema.org/OutOfStock"}]}]}));
        let product = parse_product_page(&page, "u").unwrap();
        assert_eq!(product.price, Some(99.0));
        assert!(!product.available);
        assert!(to_offer(Some(&product)).is_none());
    }

    #[test]
    fn falls_back_to_meta_tags() {
        let page = r#"<html><head><meta property="og:title" content="Fön &amp; Co"><meta property="product:price:amount" content="49,90">
            <meta content="in stock" property="product:availability"></head></html>"#;
        let product = parse_product_page(page, "u").unwrap();
        assert_eq!(product.price, Some(49.9));
        assert_eq!(product.title, "Fön & Co");
        assert!(product.available);
        assert!(parse_product_page("<html></html>", "u").is_none());
    }

    #[test]
    fn parses_store_list() {
        let stores = parse_store_list("# Kommentar\nBuddenhagen | https://www.euronics.de/hamburg-buddenhagen\neuronics.de/xanten-meier\nhttps://www.mediaathome.de/berlin-x/\nhttps://example.com/foo\nhttps://www.euronics.de/hamburg-buddenhagen");
        let ids: Vec<_> = stores.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["www.euronics.de/hamburg-buddenhagen", "www.euronics.de/xanten-meier", "www.mediaathome.de/berlin-x"]);
        assert_eq!(stores[0].name, "Buddenhagen");
        assert_eq!(stores[1].name, "Euronics Xanten Meier");
        assert_eq!(stores[1].base, "https://www.euronics.de/xanten-meier");
    }

    #[test]
    fn strips_market_prefix_from_product_path() {
        let stores = parse_store_list("https://www.euronics.de/hamburg-buddenhagen");
        let online = product_path("https://www.euronics.de/audio/kopfhoerer/airpods-pro-0195950543742", &stores);
        assert_eq!(online, "audio/kopfhoerer/airpods-pro-0195950543742");
        assert_eq!(product_path("https://www.euronics.de/hamburg-buddenhagen/audio/kopfhoerer/airpods-pro-0195950543742", &stores), online);
        assert_eq!(product_path("https://www.mediaathome.de/berlin-x/audio/airpods-0195950543742", &[]), "audio/airpods-0195950543742");
    }

    #[test]
    fn path_helpers() {
        let path = "rasierer/9150cc-series-9-sport-trocken-nass-rasierer-8710103123456";
        assert_eq!(title_from_path(path), "9150cc Series 9 Sport Trocken Nass Rasierer");
        assert_eq!(ean_from_path(path), "8710103123456");
        assert!(same_ean("0195950543742", "195950543742") && !same_ean("1", "2") && !same_ean("", ""));
    }

    #[test]
    fn groups_geojson_locations_by_shop() {
        let geojson = json!({"features": [
            {"properties": {"name": "B", "website": "https://www.euronics.de/a/", "addr:city": "X"}, "geometry": {"coordinates": [9.5, 53.5]}},
            {"properties": {"name": "A", "website": "https://www.euronics.de/a"}, "geometry": {"coordinates": [10.0, 54.0]}},
            {"properties": {"name": "C", "website": "https://www.euronics.de/c"}, "geometry": {"coordinates": [8.0, 50.0]}},
            {"properties": {"name": "ohne"}, "geometry": {"coordinates": [8.0, 50.0]}},
        ]});
        let dir = markets_from_geojson(&geojson, "test");
        assert_eq!(dir.shops.len(), 2);
        assert_eq!(dir.shops[0].url, "https://www.euronics.de/a");
        assert_eq!(dir.shops[0].locations.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
    }

    #[tokio::test]
    async fn bundled_directory_has_all_markets() {
        let euronics = Euronics::new("http://127.0.0.1:9", "t0-");
        let markets = euronics.directory_stores().await;
        assert!(markets.len() > 300, "{}", markets.len());
        assert!(markets.iter().any(|m| m.slug.contains("hamburg")));
        // PLZ-Umkreis und eigene Namen
        super::super::common::seed_plz("20095", (53.55, 10.0));
        let settings = Settings { plz: "20095".into(), euronics_stores: "Mein Markt | https://www.euronics.de/hamburg-buddenhagen".into(), ..Default::default() };
        let all = euronics.select_markets(true, 0.0, &settings).await;
        let near = euronics.select_markets(true, 30.0, &settings).await;
        assert!(near.len() < all.len() && !near.is_empty());
        assert!(near.iter().all(|m| m.distance_km.unwrap() <= 30));
        assert!(near.windows(2).all(|w| w[0].distance_km <= w[1].distance_km));
        if let Some(m) = all.iter().find(|m| m.id == "www.euronics.de/hamburg-buddenhagen") {
            assert_eq!(m.name, "Mein Markt");
        }
        assert!(euronics.select_markets(false, 0.0, &settings).await.is_empty());
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    /// Produktseite wie bei Euronics: `sku` = Artikelnummer (steht im Pfad), `gtin` = Hersteller-EAN.
    fn real_product_html(name: &str, price: &str, sku: &str, gtin: &str) -> String {
        jsonld_page(json!({"@type": "Product", "name": name, "sku": sku, "gtin": gtin, "brand": {"@type": "Brand", "name": "Samsung"},
            "offers": {"@type": "Offer", "price": price, "availability": "https://schema.org/InStock"}}))
    }

    /// Suchergebnis wie bei Euronics/media@home (Shop-Bericht 28.11.2026, gekürzt): `.product--box` mit SEO-Pfad,
    /// dazu ein ItemList, dessen `url` ein unbrauchbarer `shopware.php`-Link ist.
    fn real_search_html(boxes: &[(&str, &str)]) -> String {
        let items: String = boxes
            .iter()
            .map(|(path, title)| format!(r#"<div class="product--box" data-ordernumber="x"><a class="product--title" href="/{path}" title="{title}">{title}</a></div>"#))
            .collect();
        format!(
            r#"<html>{items}{}</html>"#,
            jsonld_page(json!({"@type": "ItemList", "itemListElement": [{"@type": "ListItem", "item": {"@type": "Product", "url": "shopware.php?sViewport=detail&sArticle=1"}}]}))
        )
    }

    const TV_PATH: &str = "tv-und-audio/fernsehen/oled-tvs/gq65s95hft-163-cm-65-oled-vision-ai-smart-tv-graphitschwarz-4065465352589";
    const BUNDLE_PATH: &str = "tv-und-audio/fernsehen/oled-tvs/gq65s95hft-inkl-hw-b760gf-4065465435435";
    const TV_EAN: &str = "8806099134839";
    const TV_NAME: &str = "Samsung GQ65S95HFTXZG S95H Vision AI Smart TV (2026) OLED, 65\"";

    fn tv_router(search_blocked: bool) -> Router {
        Router::new()
            .route("/search", get(move |Query(q): Query<HashMap<String, String>>| async move {
                if search_blocked {
                    // AWS-WAF-Challenge wie am 28.11.2026: HTTP 405, ~2 KB
                    return (axum::http::StatusCode::METHOD_NOT_ALLOWED,
                        axum::response::Html(r#"<html><title>Lassen Sie uns prüfen, ob Sie ein Mensch sind</title><script src="https://x.eu-central-1.token.awswaf.com/x/challenge.js"></script></html>"#.to_string()));
                }
                let page = match q["sSearch"].as_str() {
                    "8806099134839" => real_search_html(&[(TV_PATH, "Samsung GQ65S95HFT 163 cm (65\") OLED Vision AI Smart TV")]),
                    _ => real_search_html(&[
                        (BUNDLE_PATH, "Samsung GQ65S95HFT 163 cm (65\") OLED inkl. HW-B760GF Soundbar"),
                        (TV_PATH, "Samsung GQ65S95HFT 163 cm (65\") OLED Vision AI Smart TV"),
                    ]),
                };
                (axum::http::StatusCode::OK, axum::response::Html(page))
            }))
            .route(&format!("/{TV_PATH}"), get(|| async { axum::response::Html(real_product_html("GQ65S95HFT 163 cm (65) OLED Vision AI Smart TV", "1799.00", "4065465352589", TV_EAN)) }))
            .route(&format!("/{BUNDLE_PATH}"), get(|| async { axum::response::Html(real_product_html("GQ65S95HFT inkl. HW-B760GF", "2199.00", "4065465435435", "4065465435435")) }))
    }

    fn online_only_watch(query: &str, ean: &str) -> Watch {
        let mut watch = Watch { name: "Gerät".into(), ean: ean.into(), ..Default::default() };
        watch.sources.insert("euronics".into(), SourceConfig {
            enabled: true, query: query.into(),
            options: json!({"all_stores": false}).as_object().unwrap().clone(),
        });
        watch
    }

    fn settings() -> Settings {
        Settings { euronics_rate_per_min: 60000, ..Default::default() }
    }

    #[tokio::test]
    async fn ean_search_is_verified_by_gtin_not_by_path() {
        let euronics = Euronics::new(&serve(tv_router(false)).await, "t1-");
        let mut watch = online_only_watch("", TV_EAN);
        watch.name = TV_NAME.into();
        let mut reporter = crate::activity::ScanReporter::new("euronics-test", "Euronics", "Gerät", None);
        let (result, state) = euronics.check(&watch, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(1799.0));
        assert_eq!(result.ean, TV_EAN, "Hersteller-EAN aus gtin, nicht die Artikelnummer aus dem Pfad");
        assert_eq!(state.meta_str("product_path"), Some(TV_PATH));
        assert_eq!(state.meta_str("gtin"), Some(TV_EAN));
        let hit = euronics.locate(&watch, &settings()).await.unwrap().unwrap();
        assert_eq!(hit.confidence, Confidence::Ean);
    }

    #[tokio::test]
    async fn name_search_skips_bundles_and_checks_gtin() {
        let euronics = Euronics::new(&serve(tv_router(false)).await, "t2-");
        // EAN unbekannt bei Euronics-Suche -> Namenssuche liefert Bundle + Gerät; das Gerät hat die passende gtin
        let w = Watch { name: TV_NAME.into(), ean: "1111111111111".into(), ..Default::default() };
        match euronics.resolve(&w, &settings(), &[], &StoreState::default()).await {
            // andere EAN als das Gerät: nur Vorschlag, und zwar das Gerät, nicht das Bundle
            Err(CheckError::Lookup { suggestion: Some(s), .. }) => assert!(s.url.ends_with(TV_PATH), "{}", s.url),
            other => panic!("{other:?}"),
        }
        // ohne EAN: Name + Modellnummer eindeutig -> übernommen
        let w = Watch { name: TV_NAME.into(), ..Default::default() };
        let r = euronics.resolve(&w, &settings(), &[], &StoreState::default()).await.unwrap();
        assert_eq!((r.path.as_str(), r.gtin.as_str()), (TV_PATH, TV_EAN));
        // andere Modellnummer -> nichts
        let w = Watch { name: "Samsung GQ55S95HFTXZG".into(), ..Default::default() };
        assert!(matches!(euronics.resolve(&w, &settings(), &[], &StoreState::default()).await, Err(CheckError::Lookup { suggestion: None, .. })));
    }

    #[tokio::test]
    async fn blocked_search_is_reported_but_given_url_still_works() {
        let euronics = Euronics::new(&serve(tv_router(true)).await, "t3-");
        let mut reporter = crate::activity::ScanReporter::new("euronics-test", "Euronics", "Gerät", None);
        let mut watch = online_only_watch("", TV_EAN);
        watch.name = TV_NAME.into();
        let err = euronics.check(&watch, &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        match &err {
            CheckError::Lookup { message, .. } => assert!(message.contains("Bot-Sperre") && message.contains("Euronics-Link"), "{message}"),
            other => panic!("{other:?}"),
        }
        // Produktseiten sind nicht gesperrt: mit eingetragener URL klappt die Prüfung
        let watch = online_only_watch(&format!("https://www.euronics.de/{TV_PATH}"), TV_EAN);
        let (result, state) = euronics.check(&watch, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!((result.price, result.ean.as_str()), (Some(1799.0), TV_EAN));
        assert_eq!(state.meta_str("gtin"), Some(TV_EAN));
        assert!(is_waf_challenge(405, "<script src=\"https://a.token.awswaf.com/challenge.js\">"));
        assert!(!is_waf_challenge(200, &real_product_html("x", "1", "1", "1")));
    }

    #[tokio::test]
    async fn identify_reads_manufacturer_ean_from_product_page() {
        let base = serve(tv_router(true)).await;
        let euronics = Euronics::new(&base, "t5-");
        // Markt-URL: Marktteil fällt weg, die EAN kommt von der Produktseite (gtin), nicht aus dem Pfad
        let id = euronics.identify(&format!("https://www.euronics.de/achern-schindler/{TV_PATH}")).await.unwrap();
        assert!(id.query.ends_with(TV_PATH) && !id.query.contains("achern"), "{}", id.query);
        assert_eq!(id.ean, TV_EAN);
        assert!(matches!(euronics.identify("https://www.euronics.de/tv-und-audio").await, Err(ResolveError::Invalid(_))));
    }

    #[tokio::test]
    async fn locate_reports_confidence_and_rediscovers_after_a_week() {
        let euronics = Euronics::new(&serve(tv_router(false)).await, "t7-");
        let settings = settings();
        // Gemerkte Zuordnung gilt eine Woche, danach wird neu gesucht
        let mut w = online_only_watch("", TV_EAN);
        w.name = TV_NAME.into();
        let mut reporter = crate::activity::ScanReporter::new("euronics-test", "Euronics", "Gerät", None);
        let (_, mut state) = euronics.check(&w, &settings, StoreState::default(), &mut reporter).await.unwrap();
        assert!(Euronics::cached(&w, &state));
        state.meta.insert("resolved_at".into(), json!("2020-01-01T00:00:00+01:00"));
        assert!(!Euronics::cached(&w, &state));
        let (_, state) = euronics.check(&w, &settings, state, &mut reporter).await.unwrap();
        assert!(Euronics::cached(&w, &state), "nach erneuter Suche ist der Zeitstempel frisch");
    }

    #[tokio::test]
    async fn blocked_shop_is_reported_and_missing_product_is_no_offer() {
        let router = Router::new().route("/p/blocked-0000000000001", get(|| async { (axum::http::StatusCode::FORBIDDEN, [("retry-after", "0.01")]) }));
        let euronics = Euronics::new(&serve(router).await, "t4-");
        let mut reporter = crate::activity::ScanReporter::new("euronics-test", "Euronics", "Gerät", None);
        let blocked = online_only_watch("https://www.euronics.de/p/blocked-0000000000001", "");
        // Pfad wird aus der URL übernommen, abgefragt wird aber die (Test-)Basis
        let err = euronics.check(&blocked, &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        assert!(matches!(err, CheckError::RateLimited(_)), "{err:?}");

        let router = Router::new().route("/p/gone-0000000000002", get(|| async { Redirect::temporary("/") }))
            .route("/", get(|| async { axum::response::Html("<html>Startseite</html>") }));
        let euronics = Euronics::new(&serve(router).await, "t5-");
        let missing = online_only_watch("https://www.euronics.de/p/gone-0000000000002", "");
        let (result, _) = euronics.check(&missing, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert!(!result.available && result.error.is_none());
    }
}
