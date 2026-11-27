//! Bestpreis bei **idealo** (idealo.de) – der günstigste Preis über alle Händler im Preisvergleich.
//!
//! idealo ist kein Shop, sondern ein Preisvergleich: Pro Prüfung wird die Produktseite
//! (`/preisvergleich/OffersOfProduct/<id>_….html`) abgerufen und der niedrigste Preis aus den
//! strukturierten Produktdaten (schema.org `AggregateOffer.lowPrice`) gelesen. Der Dealalarm
//! kommt, sobald irgendein Händler bei idealo den Wunschpreis erreicht.
//!
//! Zugeordnet wird ein Gerät über die hinterlegte idealo-URL, sonst über die idealo-Suche nach
//! der EAN und – findet die nichts – nach dem Gerätenamen (Modellnummer muss passen).

use super::common::{best_name_match, http_client_with_cookies, name_terms};
use super::euronics::{parse_product_page, same_ean};
use super::{Confidence, Identity, Located, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Offer, ShopResult, StoreEntry, StoreState};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::storage::{Settings, Watch};
use crate::util::{age_hours, format_price, now_iso};
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const BASE: &str = "https://www.idealo.de";
const STORE_ID: &str = "best";
/// idealo antwortet bei zu vielen Anfragen (und auf Rechenzentrums-IPs) mit 403
const THROTTLE_STATUS: &[u16] = &[403, 429, 503];

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("Regex"))
}

/// Produkt-ID aus einer idealo-URL (`…/OffersOfProduct/1234567_-name.html`).
pub fn product_id(url: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)OffersOfProduct/(\d+)").captures(url).map(|c| c[1].to_string())
}

/// Lesbarer Titel aus dem Pfad: `1234567_-sony-wh-1000xm5-sony.html` -> „sony wh 1000xm5 sony“.
pub fn title_from_url(url: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)OffersOfProduct/\d+_-?([^/?#]*?)(?:\.html)?(?:[?#]|$)")
        .captures(url)
        .map(|c| c[1].replace('-', " ").trim().to_string())
        .unwrap_or_default()
}

/// Produktlinks einer Suchergebnisseite: (absolute URL, Titel aus dem Pfad), in Reihenfolge.
pub fn search_links(page: &str, base: &str) -> Vec<(String, String)> {
    static R: OnceLock<Regex> = OnceLock::new();
    let mut seen = vec![];
    let mut links = vec![];
    for cap in re(&R, r#"(?i)href=["']((?:https?://[^/"']+)?/preisvergleich/OffersOfProduct/(\d+)_[^"'#?]*)"#).captures_iter(page) {
        if seen.contains(&cap[2].to_string()) {
            continue;
        }
        seen.push(cap[2].to_string());
        let url = if cap[1].starts_with("http") { cap[1].to_string() } else { format!("{base}{}", &cap[1]) };
        let title = title_from_url(&url);
        links.push((url, title));
    }
    links
}

/// Anzahl der Angebote (schema.org `offerCount`), falls angegeben.
fn offer_count(page: &str) -> Option<u64> {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#""offerCount"\s*:\s*"?(\d+)"#).captures(page).and_then(|c| c[1].parse().ok())
}

fn is_product_page(url: &str) -> bool {
    product_id(url).is_some()
}

pub struct Idealo {
    base: String,
    limiter_name: String,
    default_rate: i64,
    client: reqwest::Client,
}

impl Default for Idealo {
    fn default() -> Self {
        Idealo::new(BASE, "www.idealo.de")
    }
}

impl Idealo {
    pub fn new(base: &str, limiter_name: &str) -> Self {
        Idealo {
            base: base.trim_end_matches('/').to_string(),
            limiter_name: limiter_name.to_string(),
            default_rate: 10,
            client: http_client_with_cookies("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        }
    }

    fn limiter(&self, per_minute: i64) -> Arc<AdaptiveLimiter> {
        ratelimit::get_limiter(&self.limiter_name, per_minute.max(1) as f64, 1.0)
    }

    /// Eingetragene idealo-URL auf die (Test-)Basis umschreiben – der Pfad bleibt.
    fn rebase(&self, url: &str) -> String {
        match reqwest::Url::parse(url) {
            Ok(u) => format!("{}{}", self.base, u.path()),
            Err(_) => url.to_string(),
        }
    }

    async fn fetch(&self, limiter: &AdaptiveLimiter, url: &str, query: &[(&str, &str)]) -> Result<Option<(String, String)>, CheckError> {
        let response = ratelimit::get(&self.client, limiter, url, query, THROTTLE_STATUS, 2, Duration::from_secs(30))
            .await
            .map_err(|_| {
                CheckError::RateLimited(
                    "idealo blockiert oder drosselt die Anfragen (HTTP 403/429) – Rechenzentrums-IPs werden oft gesperrt, \
                     am besten zuhause laufen lassen."
                        .into(),
                )
            })?;
        if matches!(response.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let status = response.status().as_u16();
        let response = response.error_for_status()?;
        let final_url = response.url().to_string();
        let body = response.text().await?;
        crate::debug::capture("idealo", &final_url, status, &body);
        Ok(Some((body, final_url)))
    }

    /// Suche bei idealo: führt sie direkt auf eine Produktseite, ist das der einzige Treffer.
    async fn search(&self, limiter: &AdaptiveLimiter, term: &str) -> Result<Vec<(String, String)>, CheckError> {
        let url = format!("{}/preisvergleich/MainSearchProductCategory.html", self.base);
        let Some((page, final_url)) = self.fetch(limiter, &url, &[("q", term)]).await? else { return Ok(vec![]) };
        if is_product_page(&final_url) {
            let title = parse_product_page(&page, &final_url).map(|p| p.title).filter(|t| !t.is_empty()).unwrap_or_else(|| title_from_url(&final_url));
            return Ok(vec![(final_url.split('?').next().unwrap_or(&final_url).to_string(), title)]);
        }
        Ok(search_links(&page, &self.base))
    }

    /// Produkt-URL: hinterlegt, sonst über EAN-Suche, sonst über Namenssuche.
    async fn resolve_url(&self, watch: &Watch, limiter: &AdaptiveLimiter, state: &StoreState) -> Result<String, CheckError> {
        self.find_url(watch, limiter, state).await.map(|(url, _)| url)
    }

    async fn find_url(&self, watch: &Watch, limiter: &AdaptiveLimiter, state: &StoreState) -> Result<(String, Confidence), CheckError> {
        let query = watch.source("idealo").query.trim().to_string();
        if query.contains("idealo.") && is_product_page(&query) {
            return Ok((self.rebase(&query), Confidence::Ean));
        }
        let ean = watch.ean.trim().to_string();
        let term = if query.is_empty() { ean.clone() } else { query.clone() };
        let names = if query.is_empty() { name_terms(watch, "idealo") } else { vec![] };
        if term.is_empty() && names.is_empty() {
            return Err(CheckError::Other("Keine idealo-URL, EAN oder Gerätename hinterlegt".into()));
        }
        let key = format!("{term}|{}", watch.name.trim());
        if state.meta_str("search_key") == Some(key.as_str()) && state.meta_str("resolved_at").is_none_or(|t| age_hours(t) < super::REDISCOVER_HOURS) {
            if let Some(url) = state.meta_str("product_url") {
                return Ok((url.to_string(), Confidence::Ean));
            }
        }
        let mut tried = vec![];
        if !term.is_empty() {
            tried.push(term.clone());
            let hits = self.search(limiter, &term).await?;
            // EAN-Suche bei idealo ist zuverlässig: der erste Treffer, sofern er nicht offensichtlich ein anderes Gerät ist
            let pick = if hits.len() == 1 || !query.is_empty() {
                hits.first()
            } else {
                hits.iter().find(|(_, t)| watch.name.trim().is_empty() || super::common::name_match(&watch.name, t) > 0.0).or(hits.first())
            };
            if let Some((url, _)) = pick {
                return Ok((url.clone(), Confidence::Ean));
            }
        }
        for name in names {
            let hits = self.search(limiter, &name).await?;
            if let Some((i, _)) = best_name_match(&name, hits.iter().map(|(_, t)| t.as_str())) {
                crate::info!("idealo · {}: über Namenssuche „{name}“ gefunden: {}", watch.name, hits[i].1);
                return Ok((hits[i].0.clone(), Confidence::Name));
            }
            tried.push(name);
        }
        Err(CheckError::lookup(format!(
            "Bei idealo nicht gefunden (Suche nach {} ohne passenden Treffer) – falls vorhanden, idealo-URL im Gerät eintragen",
            tried.iter().map(|t| format!("„{t}“")).collect::<Vec<_>>().join(", ")
        )))
    }
}

#[async_trait]
impl Source for Idealo {
    fn id(&self) -> &'static str {
        "idealo"
    }
    fn label(&self) -> &'static str {
        "idealo"
    }
    fn query_label(&self) -> &'static str {
        "idealo-URL (leer = automatisch über EAN bzw. Gerätename)"
    }
    fn query_placeholder(&self) -> &'static str {
        "https://www.idealo.de/preisvergleich/OffersOfProduct/…"
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![]
    }

    fn matches_input(&self, input: &str) -> bool {
        input.contains("idealo.") && is_product_page(input)
    }

    async fn identify(&self, input: &str) -> Result<Identity, ResolveError> {
        let url = input.trim().split(['?', '#']).next().unwrap_or("").to_string();
        if !is_product_page(&url) {
            return Err(ResolveError::Invalid("Bitte die idealo-Produktseite (…/OffersOfProduct/…) verwenden".into()));
        }
        // Titel/EAN sind Zugabe – blockt idealo, reicht der Titel aus der URL
        let limiter = self.limiter(self.default_rate);
        let product = match self.fetch(&limiter, &self.rebase(&url), &[]).await {
            Ok(Some((page, final_url))) => parse_product_page(&page, &final_url),
            _ => None,
        };
        let title = product.as_ref().map(|p| p.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| title_from_url(&url));
        Ok(Identity { title, ean: product.map(|p| p.ean).unwrap_or_default(), query: url })
    }

    async fn locate(&self, watch: &Watch, settings: &Settings) -> Result<Option<Located>, String> {
        let limiter = self.limiter(settings.idealo_rate_per_min);
        match self.find_url(watch, &limiter, &StoreState::default()).await {
            Ok((url, confidence)) => Ok(Some(Located { title: title_from_url(&url), query: url, confidence })),
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
        let limiter = self.limiter(settings.idealo_rate_per_min);
        limiter.start_scan();
        let key = |w: &Watch| {
            let q = w.source("idealo").query.trim().to_string();
            format!("{}|{}", if q.is_empty() { w.ean.trim().to_string() } else { q }, w.name.trim())
        };
        let from_cache = state.meta_str("search_key") == Some(key(watch).as_str())
            && state.meta_str("resolved_at").is_some_and(|t| age_hours(t) < super::REDISCOVER_HOURS);
        let url = self.resolve_url(watch, &limiter, &state).await?;
        crate::info!("idealo · {}: {url}", watch.name);
        let (page, final_url) = self
            .fetch(&limiter, &url, &[])
            .await?
            .ok_or_else(|| CheckError::lookup(format!("idealo-Produktseite gibt es nicht mehr ({url})")))?;
        let product = parse_product_page(&page, &final_url)
            .ok_or_else(|| CheckError::Other("idealo-Produktseite nicht lesbar (blockiert oder Seitenaufbau geändert)".into()))?;
        let count = offer_count(&page);

        let ean = watch.ean.trim();
        if !ean.is_empty() && !product.ean.is_empty() && !same_ean(ean, &product.ean) {
            crate::info!("idealo · {}: Produkt hat EAN {}, das Gerät EAN {ean}", watch.name, product.ean);
        }
        let offer = product.price.filter(|_| product.available).map(|price| Offer {
            price,
            shipping: None, // Bestpreis ohne Versand
            total_price: price,
            availability: Some(match count {
                Some(n) => format!("Bestpreis bei idealo ({n} Angebote)"),
                None => "Bestpreis bei idealo".into(),
            }),
            title: Some(product.title.clone()),
            url: Some(url.clone()),
            ..Default::default()
        });
        let mut new_state = StoreState::default();
        new_state.stores.insert(
            STORE_ID.into(),
            StoreEntry { name: "idealo Bestpreis".into(), checked: now_iso(), coordinates: None, offer: offer.clone() },
        );
        let title = if product.title.is_empty() { title_from_url(&url) } else { product.title.clone() };
        new_state.meta.insert("product_url".into(), json!(url));
        new_state.meta.insert("title".into(), json!(title));
        let query = watch.source("idealo").query.trim().to_string();
        let term = if query.is_empty() { watch.ean.trim().to_string() } else { query };
        new_state.meta.insert("search_key".into(), json!(format!("{term}|{}", watch.name.trim())));
        let resolved_at = state.meta_str("resolved_at").filter(|_| from_cache && state.meta_str("product_url") == Some(url.as_str())).map(str::to_string).unwrap_or_else(now_iso);
        new_state.meta.insert("resolved_at".into(), json!(resolved_at));

        let stats = crate::model::Stats { planned: 1, checked: 1, offers: offer.is_some() as usize, total: 1, known: 1, ..Default::default() };
        progress.report(&stats, &new_state, offer.as_ref());
        let detail = match &offer {
            Some(o) => format!("{} · ohne Versand", o.availability.clone().unwrap_or_default()),
            None => "Derzeit keine Angebote bei idealo".into(),
        };
        let result = ShopResult {
            title,
            url,
            ean: product.ean.clone(),
            price: offer.as_ref().map(|o| o.total_price),
            available: offer.is_some(),
            detail,
            ..Default::default()
        };
        if let Some(o) = &offer {
            crate::info!("idealo · {}: Bestpreis {}", watch.name, format_price(o.price));
        }
        Ok((result, new_state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

    fn product_page(name: &str, low: &str, ean: &str) -> String {
        format!(
            r#"<html><script type="application/ld+json">{}</script></html>"#,
            json!({"@type": "Product", "name": name, "gtin13": ean,
                   "offers": {"@type": "AggregateOffer", "lowPrice": low, "highPrice": "999", "offerCount": 37, "priceCurrency": "EUR"}})
        )
    }

    #[test]
    fn url_helpers() {
        let url = "https://www.idealo.de/preisvergleich/OffersOfProduct/205412345_-wh-1000xm6-sony.html";
        assert_eq!(product_id(url).as_deref(), Some("205412345"));
        assert_eq!(title_from_url(url), "wh 1000xm6 sony");
        assert!(product_id("https://www.idealo.de/preisvergleich/ProductCategory/2520.html").is_none());
        let page = r#"<a href="/preisvergleich/OffersOfProduct/1_-sony-wh-1000xm4.html">a</a>
                      <a href="/preisvergleich/OffersOfProduct/2_-sony-wh-1000xm6.html?x=1">b</a>
                      <a href="/preisvergleich/OffersOfProduct/2_-sony-wh-1000xm6.html">b</a>"#;
        let links = search_links(page, "https://www.idealo.de");
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].1, "sony wh 1000xm6");
        assert_eq!(offer_count(&product_page("x", "1", "")), Some(37));
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn watch(query: &str, ean: &str, name: &str) -> Watch {
        let mut watch = Watch { name: name.into(), target_price: 300.0, ean: ean.into(), ..Default::default() };
        watch.sources.insert("idealo".into(), SourceConfig { enabled: true, query: query.into(), ..Default::default() });
        watch
    }

    fn settings() -> Settings {
        Settings { idealo_rate_per_min: 60000, ..Default::default() }
    }

    fn router() -> Router {
        Router::new()
            .route("/preisvergleich/MainSearchProductCategory.html", get(|Query(q): Query<HashMap<String, String>>| async move {
                Html(match q["q"].as_str() {
                    "4548736146000" => String::new(), // EAN unbekannt
                    _ => r#"<a href="/preisvergleich/OffersOfProduct/1_-sony-wh-1000xm4.html">a</a>
                            <a href="/preisvergleich/OffersOfProduct/2_-sony-wh-1000xm6-schwarz.html">b</a>"#.into(),
                })
            }))
            .route("/preisvergleich/OffersOfProduct/{file}", get(|axum::extract::Path(file): axum::extract::Path<String>| async move {
                if file.starts_with("2_") { Html(product_page("Sony WH-1000XM6", "289.90", "4548736199999")) } else { Html(product_page("Sony WH-1000XM4", "199.00", "1")) }
            }))
    }

    #[tokio::test]
    async fn finds_product_by_name_and_reads_best_price() {
        let idealo = Idealo::new(&serve(router()).await, "idealo-test-1");
        let mut reporter = crate::activity::ScanReporter::new("idealo-test", "idealo", "x", None);
        let w = watch("", "4548736146000", "Sony WH-1000XM6");
        assert!(idealo.is_active(&w));
        let (result, state) = idealo.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(289.9));
        assert_eq!(result.title, "Sony WH-1000XM6");
        assert!(result.detail.contains("37 Angebote"), "{}", result.detail);
        assert!(result.url.contains("OffersOfProduct/2_"));
        assert_eq!(state.stores[STORE_ID].offer.as_ref().unwrap().total_price, 289.9);
        let hit = idealo.locate(&w, &settings()).await.unwrap().unwrap();
        assert_eq!(hit.confidence, Confidence::Name);
        // Ergebnis der Suche wird gemerkt
        assert!(state.meta_str("search_key").unwrap().starts_with("4548736146000|"));
    }

    #[tokio::test]
    async fn uses_given_url_and_reports_unknown_device() {
        let idealo = Idealo::new(&serve(router()).await, "idealo-test-2");
        let mut reporter = crate::activity::ScanReporter::new("idealo-test", "idealo", "x", None);
        let w = watch("https://www.idealo.de/preisvergleich/OffersOfProduct/1_-sony-wh-1000xm4.html", "", "egal");
        let (result, _) = idealo.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(199.0));

        let w = watch("", "", "Sony WH-1000XM9");
        let err = idealo.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        assert!(matches!(err, CheckError::Lookup { .. }), "{err:?}");
        assert!(idealo.matches_input("https://www.idealo.de/preisvergleich/OffersOfProduct/1_-x.html"));
        assert!(!idealo.matches_input("https://www.amazon.de/dp/B0DEMO1234"));
    }
}
