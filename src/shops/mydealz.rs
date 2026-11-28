//! **mydealz** – Deals der Community zum Gerät (Stichwortsuche).
//!
//! Pro Prüfung eine Suche `https://www.mydealz.de/search?q=<Stichwort>`. Jeder Deal steht als
//! `article[id^="thread_"]` in der Seite, die Daten als JSON im Attribut `data-vue3`
//! (`props.thread`: Titel, Preis, Händler, Temperatur, abgelaufen, Veröffentlichung).
//! Aufbau laut Shop-Bericht vom 28.11.2026.
//!
//! Deal-Titel sind frei formuliert („Samsung 65" S95H OLED …“) und enthalten selten die volle
//! Modellnummer. Ein Deal passt, wenn der Name sicher passt oder die Serien-/Modellbezeichnung
//! vorkommt und keine andere Bildschirmgröße genannt ist (siehe [`deal_matches`]).

use super::common::{model_tokens, name_match, name_tokens, NAME_MATCH_MIN};
use super::{OptionDef, Source};
use crate::model::{CheckError, Coords, Offer, ShopResult, StoreEntry, StoreState};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::storage::{Settings, Watch};
use crate::util::{format_price, now_iso, parse_price_value};
use async_trait::async_trait;
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const BASE: &str = "https://www.mydealz.de";

/// Ein Deal aus der Suche.
#[derive(Debug, Clone, PartialEq)]
pub struct Deal {
    pub id: String,
    pub title: String,
    pub price: Option<f64>,
    pub merchant: String,
    pub temperature: f64,
    pub expired: bool,
    /// Unix-Zeit (Sekunden)
    pub published: i64,
    pub url: String,
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("CSS-Selektor")
}

/// Deals einer Suchergebnisseite.
pub fn parse_deals(page: &str, base: &str) -> Vec<Deal> {
    let doc = Html::parse_document(page);
    let article_sel = sel(r#"article[id^="thread_"]"#);
    let vue_sel = sel("[data-vue3]");
    let link_sel = sel("a.thread-link");
    let mut deals = vec![];
    for article in doc.select(&article_sel) {
        // data-vue3 kann am Artikel selbst oder an einem inneren Element stehen; gesucht ist der mit props.thread
        let thread = std::iter::once(article)
            .chain(article.select(&vue_sel))
            .filter_map(|el| el.value().attr("data-vue3"))
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .find_map(|v| v["props"]["thread"].as_object().map(|_| v["props"]["thread"].clone()));
        let Some(t) = thread else { continue };
        let id = match &t["threadId"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => article.value().id().unwrap_or("").trim_start_matches("thread_").to_string(),
        };
        let href = article.select(&link_sel).filter_map(|a| a.value().attr("href")).next();
        let url = match (href, t["titleSlug"].as_str(), t["shareableLink"].as_str()) {
            (Some(h), _, _) if h.starts_with("http") => h.to_string(),
            (Some(h), _, _) => format!("{base}{h}"),
            (None, Some(slug), _) => format!("{base}/deals/{slug}-{id}"),
            (None, None, Some(link)) => link.to_string(),
            _ => format!("{base}/share-deal/{id}"),
        };
        deals.push(Deal {
            id,
            title: t["title"].as_str().unwrap_or("").to_string(),
            price: parse_price_value(&t["price"]).filter(|p| *p > 0.0),
            merchant: t["merchant"].as_str().map(str::to_string).or_else(|| t["merchant"]["merchantName"].as_str().map(str::to_string)).unwrap_or_default(),
            temperature: t["temperature"].as_f64().unwrap_or(0.0),
            expired: t["isExpired"].as_bool().unwrap_or(false) || t["isGone"].as_bool().unwrap_or(false),
            published: t["publishedAt"].as_i64().unwrap_or(0),
            url,
        });
    }
    deals
}

/// Bildschirmgrößen in Zoll: „65"“, „65 Zoll“, „65-Zoll“ und in Modellnummern wie „GQ65S95H“.
fn sizes(text: &str) -> Vec<u32> {
    static INCH: OnceLock<Regex> = OnceLock::new();
    static MODEL: OnceLock<Regex> = OnceLock::new();
    let inch = INCH.get_or_init(|| Regex::new(r#"(?i)\b(\d{2,3})\s*(?:"|''|”|-?\s*zoll|inch)"#).unwrap());
    let model = MODEL.get_or_init(|| Regex::new(r"(?i)\b[a-z]{2}(\d{2})[a-z]\w{2,}").unwrap());
    let mut out: Vec<u32> = inch.captures_iter(text).chain(model.captures_iter(text)).filter_map(|c| c[1].parse().ok()).filter(|s| (24..=120).contains(s)).collect();
    out.sort();
    out.dedup();
    out
}

/// Passt ein Deal-Titel zum Gerät?
///
/// - sicher passender Name (Modellnummer wie bei den Shops) → ja
/// - sonst: eine Serien-/Modellbezeichnung des Geräts (z. B. „S95H“, „WH-1000XM6“) kommt im Titel vor,
///   und der Titel nennt keine andere Bildschirmgröße als das Gerät (S95H in 55" ≠ 65")
pub fn deal_matches(wanted: &str, title: &str) -> bool {
    if name_match(wanted, title) >= NAME_MATCH_MIN {
        return true;
    }
    let title_tokens = name_tokens(title);
    let joined: String = title_tokens.concat();
    let series = model_tokens(wanted).into_iter().filter(|m| m.len() <= 9).any(|m| title_tokens.contains(&m) || joined.contains(&m));
    if !series {
        return false;
    }
    let (w, t) = (sizes(wanted), sizes(title));
    w.is_empty() || t.is_empty() || w.iter().any(|s| t.contains(s))
}

/// Suchbegriff: Marke + Serien-/Modellbezeichnungen (mydealz-Titel enthalten selten die volle Modellnummer).
pub fn search_term(name: &str) -> String {
    let brand = name.split_whitespace().next().unwrap_or("");
    // Schreibweise aus dem Namen behalten („S95H“)
    let short: Vec<String> = model_tokens(name).into_iter().filter(|m| m.len() <= 6).collect();
    let models: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| short.contains(&w.to_lowercase()))
        .map(str::to_string)
        .take(2)
        .collect();
    if models.is_empty() {
        name.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
    } else {
        format!("{brand} {}", models.join(" ")).trim().to_string()
    }
}

pub struct Mydealz {
    base: String,
    client: reqwest::Client,
}

impl Default for Mydealz {
    fn default() -> Self {
        Mydealz::new(BASE)
    }
}

impl Mydealz {
    pub fn new(base: &str) -> Self {
        Mydealz {
            base: base.trim_end_matches('/').to_string(),
            client: super::common::http_client("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        }
    }

    fn limiter(&self, settings: &Settings) -> Arc<AdaptiveLimiter> {
        ratelimit::get_limiter("www.mydealz.de", settings.mydealz_rate_per_min.max(1) as f64, 1.0)
    }

    async fn search(&self, limiter: &AdaptiveLimiter, term: &str) -> Result<Vec<Deal>, CheckError> {
        let url = format!("{}/search", self.base);
        let response = ratelimit::get(&self.client, limiter, &url, &[("q", term)], ratelimit::THROTTLE_STATUS, 2, Duration::from_secs(30))
            .await
            .map_err(|_| CheckError::RateLimited("mydealz drosselt die Anfragen (HTTP 429/503) – später erneut versuchen.".into()))?;
        let status = response.status().as_u16();
        if status == 403 {
            return Err(CheckError::RateLimited("mydealz blockiert die Anfragen (HTTP 403).".into()));
        }
        let response = response.error_for_status()?;
        let final_url = response.url().to_string();
        let body = response.text().await?;
        crate::debug::capture("mydealz", &final_url, status, &body);
        Ok(parse_deals(&body, &self.base))
    }
}

#[async_trait]
impl Source for Mydealz {
    fn id(&self) -> &'static str {
        "mydealz"
    }
    fn label(&self) -> &'static str {
        "mydealz"
    }
    fn query_label(&self) -> &'static str {
        "Suchbegriff für mydealz (leer = aus dem Gerätenamen, z. B. „Samsung S95H“)"
    }
    fn query_placeholder(&self) -> &'static str {
        "Samsung S95H"
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![
            OptionDef::number("max_age_days", "Nur Deals der letzten … Tage", 14.0),
            OptionDef::number("min_temperature", "Mindest-Temperatur (°)", 0.0),
        ]
    }

    fn is_active(&self, watch: &Watch) -> bool {
        let cfg = watch.source("mydealz");
        cfg.enabled && (!cfg.query.trim().is_empty() || watch.name.trim().chars().count() >= 3)
    }

    fn filter_offers(&self, offers: Vec<Offer>, watch: &Watch, _settings: &Settings, _user_coords: Option<Coords>) -> Vec<Offer> {
        let cfg = watch.source("mydealz");
        let min_temp = cfg.opt_f64("min_temperature", 0.0);
        let temperature = |o: &Offer| o.webcode.as_deref().and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0);
        offers.into_iter().filter(|o| temperature(o) >= min_temp).collect()
    }

    async fn check(
        &self,
        watch: &Watch,
        settings: &Settings,
        _state: StoreState,
        progress: &mut crate::activity::ScanReporter,
    ) -> Result<(ShopResult, StoreState), CheckError> {
        let cfg = watch.source("mydealz");
        let term = if cfg.query.trim().is_empty() { search_term(&watch.name) } else { cfg.query.trim().to_string() };
        let limiter = self.limiter(settings);
        limiter.start_scan();
        let deals = self.search(&limiter, &term).await?;
        let max_age = cfg.opt_f64("max_age_days", 14.0).max(1.0);
        let now = chrono::Utc::now().timestamp();
        let matching: Vec<&Deal> = deals
            .iter()
            .filter(|d| !d.expired && d.price.is_some())
            .filter(|d| d.published == 0 || (now - d.published) as f64 <= max_age * 86400.0)
            .filter(|d| deal_matches(&watch.name, &d.title))
            .collect();
        crate::info!("mydealz · {}: Suche „{term}“ – {} Deals, {} passend", watch.name, deals.len(), matching.len());

        let checked = now_iso();
        let mut state = StoreState::default();
        for d in &matching {
            let price = d.price.unwrap_or_default();
            let offer = Offer {
                price,
                shipping: None,
                total_price: price,
                availability: Some(format!("{} · {:.0}° · {}", if d.merchant.is_empty() { "mydealz" } else { &d.merchant }, d.temperature, d.title)),
                title: Some(d.title.clone()),
                url: Some(d.url.clone()),
                // Temperatur für den Filter (das Feld wird bei mydealz sonst nicht gebraucht und nicht angezeigt)
                webcode: Some(format!("{:.0}", d.temperature)),
                ..Default::default()
            };
            let name = if d.merchant.is_empty() { format!("Deal {}", d.id) } else { format!("{} (Deal {})", d.merchant, d.id) };
            state.stores.insert(format!("deal:{}", d.id), StoreEntry { name, checked: checked.clone(), coordinates: None, offer: Some(offer) });
        }
        state.meta.insert("search_term".into(), json!(term));

        let offers = self.filter_offers(crate::scan::fresh_offers(&state, f64::INFINITY), watch, settings, None);
        let best = offers.first().cloned();
        let n = state.stores.len();
        let stats = crate::model::Stats { planned: n.max(1), checked: n.max(1), offers: offers.len(), total: n.max(1), known: n.max(1), ..Default::default() };
        progress.report(&stats, &state, best.as_ref());
        let detail = match &best {
            Some(b) => format!(
                "{}{}",
                b.availability.clone().unwrap_or_default(),
                if offers.len() > 1 { format!(" · {} passende Deals", offers.len()) } else { String::new() }
            ),
            None if deals.is_empty() => format!("Keine Deals zu „{term}“"),
            None => format!("{} Deals zu „{term}“, keiner passt zum Gerät", deals.len()),
        };
        if let Some(b) = &best {
            crate::info!("mydealz · {}: günstigster passender Deal {}", watch.name, format_price(b.price));
        }
        Ok((
            ShopResult {
                title: best.as_ref().and_then(|b| b.title.clone()).unwrap_or_default(),
                url: best.as_ref().and_then(|b| b.url.clone()).unwrap_or_else(|| format!("{}/search?q={}", self.base, urlencoding::encode(&term))),
                price: best.as_ref().map(|b| b.total_price),
                available: best.is_some(),
                detail,
                scan: Some(stats),
                ..Default::default()
            },
            state,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, response::Html as AxHtml, routing::get, Router};
    use std::collections::HashMap;

    const TV: &str = "Samsung GQ65S95HFTXZG S95H Vision AI Smart TV (2026) OLED, 65\"";

    /// Deal wie in der echten Suche (Shop-Bericht 28.11.2026): JSON in `data-vue3`, HTML-escaped im Attribut.
    fn article(id: &str, title: &str, price: f64, temp: f64, expired: bool, published: i64) -> String {
        let data = json!({"name": "ThreadMainListItemNormalizer", "props": {"thread": {
            "threadId": id, "title": title, "titleSlug": "slug", "price": price, "nextBestPrice": price + 100.0,
            "temperature": temp, "merchant": "expert", "isExpired": expired, "publishedAt": published,
            "shareableLink": format!("https://www.mydealz.de/share-deal/{id}")}}});
        let escaped = data.to_string().replace('&', "&amp;").replace('"', "&quot;");
        format!(r#"<article id="thread_{id}" data-t-d='{{"id":{id}}}'><div data-vue3="{escaped}"></div>
            <strong class="thread-title"><a class="thread-link" href="/deals/slug-{id}">{title}</a></strong></article>"#)
    }

    #[test]
    fn matches_deal_titles_conservatively() {
        assert!(deal_matches(TV, "Samsung GQ65S95HFTXZG für 1.599€ bei expert"));
        assert!(deal_matches(TV, "Samsung 65\" S95H OLED TV + 300€ Cashback"));
        assert!(deal_matches(TV, "Samsung S95H OLED (alle Größen) ab 1.099€"));
        assert!(!deal_matches(TV, "Samsung 55\" S95H OLED für 1.199€"), "andere Größe");
        assert!(!deal_matches(TV, "Samsung 83\" S94H OLED TV + 500€ Samsung Cashback"), "andere Serie");
        assert!(!deal_matches(TV, "LG OLED65C5 für 1.299€"));
        assert_eq!(search_term(TV), "Samsung S95H");
        assert_eq!(search_term("Apple AirPods Pro 3"), "Apple AirPods Pro 3");
        assert_eq!(sizes("Samsung 65\" S95H und GQ55S95H, 77 Zoll"), vec![55, 65, 77]);
    }

    #[test]
    fn parses_real_search_page() {
        let page = format!("<html>{}{}</html>", article("2848938", "Samsung 83\" S94H OLED TV", 2948.9, 179.14, false, 1791228490), article("1", "x", 1.0, 0.0, true, 0));
        let deals = parse_deals(&page, BASE);
        assert_eq!(deals.len(), 2);
        let d = &deals[0];
        assert_eq!((d.id.as_str(), d.price, d.temperature, d.merchant.as_str()), ("2848938", Some(2948.9), 179.14, "expert"));
        assert_eq!(d.url, "https://www.mydealz.de/deals/slug-2848938");
        assert!(deals[1].expired);
    }

    #[tokio::test]
    async fn reports_cheapest_matching_fresh_deal() {
        let now = chrono::Utc::now().timestamp();
        let page = format!(
            "<html>{}{}{}{}{}</html>",
            article("10", "Samsung 65\" S95H OLED für 1.499€", 1499.0, 250.0, false, now - 3600),
            article("11", "Samsung 55\" S95H OLED für 1.099€", 1099.0, 300.0, false, now - 3600), // andere Größe
            article("12", "Samsung GQ65S95HFT für 1.399€", 1399.0, 20.0, true, now - 3600),        // abgelaufen
            article("13", "Samsung S95H 65 Zoll 1.299€", 1299.0, 50.0, false, now - 40 * 86400),   // zu alt
            article("14", "Samsung GQ65S95HFTXZG bei Otto", 1449.0, 5.0, false, now - 7200),
        );
        let router = Router::new().route("/search", get(move |Query(q): Query<HashMap<String, String>>| {
            let page = page.clone();
            async move {
                assert_eq!(q["q"], "Samsung S95H");
                AxHtml(page)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let md = Mydealz::new(&base);
        let mut w = Watch { name: TV.into(), target_price: 1500.0, ..Default::default() };
        w.sources.insert("mydealz".into(), SourceConfig { enabled: true, ..Default::default() });
        assert!(md.is_active(&w));
        let settings = Settings { mydealz_rate_per_min: 60000, ..Default::default() };
        let mut reporter = crate::activity::ScanReporter::new("md-test", "mydealz", "x", None);
        let (result, state) = md.check(&w, &settings, StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(1449.0));
        assert_eq!(state.stores.len(), 2, "nur Deal 10 und 14 passen");
        // Mindest-Temperatur 100° -> nur Deal 10
        w.sources.get_mut("mydealz").unwrap().options = json!({"min_temperature": 100}).as_object().unwrap().clone();
        let (result, _) = md.check(&w, &settings, StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(1499.0));
        assert!(result.detail.contains("250°"), "{}", result.detail);
    }
}
