//! Preisabfrage bei Amazon (amazon.de) – Neuware und **Amazon Warehouse** (Gebrauchtware).
//!
//! Amazon bietet ohne Partner-Zugang keine Preis-API, deshalb wird die „Alle Angebote“-Liste
//! eines Artikels (ASIN) wie im Browser abgerufen und ausgewertet. Pro Prüfung ist das genau
//! **eine** Anfrage. Aus der Liste entstehen bis zu sechs „Angebotsarten“, jeweils mit dem
//! günstigsten Angebot:
//!
//! | Kennung               | Bedeutung                                            |
//! |-----------------------|------------------------------------------------------|
//! | `new`                 | Neuware, verkauft von Amazon                         |
//! | `new_marketplace`     | Neuware von Drittanbietern (Marketplace)             |
//! | `warehouse_like_new`  | Amazon Warehouse – Wie neu                           |
//! | `warehouse_very_good` | Amazon Warehouse – Sehr gut                          |
//! | `warehouse_good`      | Amazon Warehouse – Gut                               |
//! | `warehouse_acceptable`| Amazon Warehouse – Akzeptabel                        |
//!
//! Welche davon für den Dealalarm zählen, bestimmen die Optionen des Geräts (z. B. „Warehouse
//! ab Zustand Gut“). Alle Arten stehen in der Tabelle „Alle Angebote“.
//!
//! Amazon blockt Automaten schnell (Captcha-Seite, HTTP 503). Das Tempo ist deshalb niedrig
//! (Standard 4 Anfragen/Minute) und ein Captcha wird wie eine Drosselung behandelt.

use super::common::{best_name_match, http_client_with_cookies, name_match, name_terms, NAME_MATCH_MIN};
use super::{Identity, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Coords, Offer, ShopResult, StoreEntry, StoreState};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::scan;
use crate::storage::{Settings, Watch};
use crate::util::{format_price, now_iso, parse_price};
use async_trait::async_trait;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const BASE: &str = "https://www.amazon.de";

/// Angebotsarten in Anzeigereihenfolge: (Kennung, Beschriftung)
pub const KINDS: &[(&str, &str)] = &[
    ("new", "Neuware (verkauft von Amazon)"),
    ("new_marketplace", "Neuware (Drittanbieter)"),
    ("warehouse_like_new", "Amazon Warehouse – Wie neu"),
    ("warehouse_very_good", "Amazon Warehouse – Sehr gut"),
    ("warehouse_good", "Amazon Warehouse – Gut"),
    ("warehouse_acceptable", "Amazon Warehouse – Akzeptabel"),
];

/// Zustand -> Rang (höher = besser)
fn condition_rank(condition: &str) -> u8 {
    match condition {
        "new" => 5,
        "like_new" => 4,
        "very_good" => 3,
        "good" => 2,
        "acceptable" => 1,
        _ => 0,
    }
}

/// "Gebraucht - Sehr gut" -> very_good, "Neu" -> new
pub fn condition_key(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    if t.contains("wie neu") || t.contains("like new") {
        Some("like_new")
    } else if t.contains("sehr gut") || t.contains("very good") {
        Some("very_good")
    } else if t.contains("akzeptabel") || t.contains("acceptable") {
        Some("acceptable")
    } else if t.contains("gut") || t.contains("good") {
        Some("good")
    } else if (t.trim_start().starts_with("neu") || t.trim_start().starts_with("new")) && !t.contains("gebraucht") && !t.contains("used") {
        Some("new")
    } else {
        None
    }
}

/// Ein Angebot aus der Angebotsliste.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOffer {
    pub price: f64,
    pub shipping: Option<f64>,
    pub condition: &'static str,
    pub seller: String,
    pub warehouse: bool,
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("CSS-Selektor")
}

fn text_of(el: ElementRef) -> String {
    el.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shipping_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(?:EUR\s*)?(\d+(?:\.\d{3})*,\d{2})\s*(?:€|EUR)?\s*(?:Versand|Lieferung|Lieferkosten)").unwrap())
}

fn free_shipping_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(?:kostenlose[rn]?\s+(?:lieferung|versand)|versand:?\s*gratis|free (?:delivery|shipping))").unwrap())
}

fn parse_shipping(text: &str) -> Option<f64> {
    if free_shipping_re().is_match(text) {
        return Some(0.0);
    }
    shipping_re().captures(text).and_then(|c| parse_price(&c[1]))
}

/// Liest die Angebote aus der „Alle Angebote“-Antwort (AOD, `aodAjaxMain`).
pub fn parse_offers(page: &str) -> Vec<ParsedOffer> {
    let doc = Html::parse_document(page);
    let offer_sel = sel("#aod-pinned-offer, #aod-offer");
    let price_sel = sel(".a-price .a-offscreen");
    let heading_sel = sel("#aod-offer-heading");
    let sold_by_sel = sel("#aod-offer-soldBy");
    let pinned_sel = sel("#aod-pinned-offer");
    let mut offers = vec![];
    for el in doc.select(&offer_sel) {
        let Some(price) = el.select(&price_sel).next().map(text_of).and_then(|t| parse_price(&t)) else { continue };
        let is_pinned = pinned_sel.matches(&el);
        let heading = el.select(&heading_sel).next().map(text_of).unwrap_or_default();
        // Das angeheftete Angebot (Buy Box) hat keine Zustandszeile und ist Neuware
        let Some(condition) = (if heading.is_empty() && is_pinned { Some("new") } else { condition_key(&heading) }) else { continue };
        let seller = el.select(&sold_by_sel).next().map(text_of).unwrap_or_default();
        let seller_lc = seller.to_lowercase();
        let warehouse = seller_lc.contains("warehouse") || seller_lc.contains("amazon resale");
        offers.push(ParsedOffer { price, shipping: parse_shipping(&text_of(el)), condition, seller, warehouse });
    }
    offers
}

/// Kennung der Angebotsart, zu der ein Angebot gehört (None = zählt nicht: Gebrauchtware von Drittanbietern).
fn kind_of(offer: &ParsedOffer) -> Option<String> {
    if offer.condition == "new" {
        let amazon = offer.seller.to_lowercase().contains("amazon") && !offer.warehouse;
        // Ohne Verkäuferangabe (Buy Box ohne Zeile) gilt Neuware als von Amazon
        Some(if amazon || offer.seller.is_empty() { "new" } else { "new_marketplace" }.into())
    } else if offer.warehouse {
        Some(format!("warehouse_{}", offer.condition))
    } else {
        None
    }
}

/// Günstigstes Angebot je Angebotsart.
pub fn best_per_kind(offers: &[ParsedOffer]) -> Vec<(&'static str, Option<ParsedOffer>)> {
    KINDS
        .iter()
        .map(|(kind, _)| {
            let best = offers
                .iter()
                .filter(|o| kind_of(o).as_deref() == Some(kind))
                .min_by(|a, b| (a.price + a.shipping.unwrap_or(0.0)).total_cmp(&(b.price + b.shipping.unwrap_or(0.0))))
                .cloned();
            (*kind, best)
        })
        .collect()
}

pub fn kind_label(kind: &str) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l).unwrap_or("Amazon")
}

/// ASIN aus einer Amazon-URL oder direkter Eingabe.
pub fn extract_asin(input: &str) -> Option<String> {
    static URL: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    let input = input.trim();
    let url = URL.get_or_init(|| Regex::new(r"(?i)/(?:dp|gp/product|gp/aw/d|product|exec/obidos/asin)/([A-Z0-9]{10})(?:[/?#]|$)").unwrap());
    if let Some(c) = url.captures(input) {
        return Some(c[1].to_uppercase());
    }
    let bare = BARE.get_or_init(|| Regex::new(r"^[A-Z0-9]{10}$").unwrap());
    bare.is_match(input).then(|| input.to_string())
}

fn is_blocked(page: &str) -> bool {
    let lower = page.to_lowercase();
    lower.contains("validatecaptcha")
        || lower.contains("geben sie die angezeigten zeichen")
        || lower.contains("type the characters you see")
        || lower.contains("api-services-support@amazon")
}

/// Ein Treffer der Amazon-Suche.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub asin: String,
    pub title: String,
    pub sponsored: bool,
}

/// Treffer einer Amazon-Suchseite in Reihenfolge (ohne leere Platzhalter).
pub fn parse_search(page: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(page);
    let results = sel("div[data-component-type=\"s-search-result\"][data-asin]");
    let title_sel = sel("h2");
    let sponsored_sel = sel(".puis-sponsored-label-text, .s-sponsored-label-text, [data-component-type=\"sp-sponsored-result\"]");
    doc.select(&results)
        .filter_map(|el| {
            let asin = el.value().attr("data-asin").filter(|a| !a.is_empty())?.to_string();
            let classes = el.value().attr("class").unwrap_or("");
            let sponsored = classes.contains("AdHolder") || el.select(&sponsored_sel).next().is_some();
            let title = el.select(&title_sel).map(text_of).max_by_key(|t| t.len()).unwrap_or_default();
            Some(SearchResult { asin, title, sponsored })
        })
        .collect()
}

/// Bei der EAN-Suche zählt der erste nicht gesponserte Treffer – passt er nicht zum Gerätenamen,
/// wird er verworfen (Amazon zeigt zur EAN gern Ähnliches statt nichts).
fn pick_ean_result(results: &[SearchResult], name: &str) -> Option<String> {
    let first = results.iter().find(|r| !r.sponsored).or(results.first())?;
    let name_ok = name.trim().is_empty() || first.title.is_empty() || name_match(name, &first.title) >= NAME_MATCH_MIN;
    name_ok.then(|| first.asin.clone())
}

/// Bester zum Namen passender Treffer (gesponserte zählen nur, wenn sonst keiner passt).
fn pick_name_result(results: &[SearchResult], name: &str) -> Option<(String, String)> {
    let organic: Vec<&SearchResult> = results.iter().filter(|r| !r.sponsored).collect();
    for list in [organic, results.iter().collect()] {
        if let Some((i, _)) = best_name_match(name, list.iter().map(|r| r.title.as_str())) {
            return Some((list[i].asin.clone(), list[i].title.clone()));
        }
    }
    None
}

fn product_title(page: &str) -> Option<String> {
    let doc = Html::parse_document(page);
    ["#aod-asin-title-text", "#productTitle"]
        .iter()
        .find_map(|s| doc.select(&sel(s)).next().map(text_of))
        .filter(|t| !t.is_empty())
}

pub struct Amazon {
    base: String,
    limiter_name: String,
    /// Tempo, solange keine Einstellungen vorliegen (Artikelerkennung beim Anlegen)
    default_rate: i64,
    client: reqwest::Client,
}

impl Default for Amazon {
    fn default() -> Self {
        Amazon::new(BASE, "www.amazon.de")
    }
}

impl Amazon {
    pub fn new(base: &str, limiter_name: &str) -> Self {
        Amazon {
            base: base.trim_end_matches('/').to_string(),
            limiter_name: limiter_name.to_string(),
            default_rate: 4,
            client: http_client_with_cookies("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        }
    }

    #[cfg(test)]
    fn fast(mut self) -> Self {
        self.default_rate = 60000;
        self
    }

    fn limiter(&self, per_minute: i64) -> Arc<AdaptiveLimiter> {
        ratelimit::get_limiter(&self.limiter_name, per_minute.max(1) as f64, 1.0)
    }

    /// Seite abrufen. Captcha/Sperre zählt wie eine Drosselung.
    async fn fetch(&self, limiter: &AdaptiveLimiter, url: &str, query: &[(&str, &str)]) -> Result<Option<(String, String)>, CheckError> {
        let blocked = || {
            CheckError::RateLimited(
                "Amazon blockiert die Anfragen (Captcha/HTTP 503). Tempo senken, später erneut versuchen – \
                 Rechenzentrums-IPs werden oft gesperrt, am besten zuhause laufen lassen."
                    .into(),
            )
        };
        let response = ratelimit::get(&self.client, limiter, url, query, ratelimit::THROTTLE_STATUS, 2, Duration::from_secs(30))
            .await
            .map_err(|_| blocked())?;
        if matches!(response.status().as_u16(), 404 | 410) {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        let final_url = response.url().to_string();
        let body = response.text().await?;
        if is_blocked(&body) {
            limiter.throttled(None);
            return Err(blocked());
        }
        Ok(Some((body, final_url)))
    }

    async fn search(&self, limiter: &AdaptiveLimiter, term: &str) -> Result<Vec<SearchResult>, CheckError> {
        let url = format!("{}/s", self.base);
        let (page, _) = self.fetch(limiter, &url, &[("k", term)]).await?.ok_or_else(|| CheckError::lookup("Amazon-Suche nicht erreichbar"))?;
        Ok(parse_search(&page))
    }

    /// ASIN aus URL/ASIN, sonst per Suche: erst EAN bzw. eigener Suchbegriff, dann Gerätename.
    async fn resolve_asin(&self, watch: &Watch, limiter: &AdaptiveLimiter, state: &StoreState) -> Result<String, CheckError> {
        let query = watch.source("amazon").query.trim().to_string();
        if let Some(asin) = extract_asin(&query) {
            return Ok(asin);
        }
        let ean = watch.ean.trim().to_string();
        let term = if query.is_empty() { ean.clone() } else { query.clone() };
        let names = name_terms(watch, "amazon");
        if term.is_empty() && names.is_empty() {
            return Err(CheckError::Other("Keine Amazon-URL, ASIN, EAN oder Gerätename hinterlegt".into()));
        }
        if state.meta_str("search_term") == Some(term.as_str()) && state.meta_str("search_name") == Some(watch.name.trim()) {
            if let Some(asin) = state.meta_str("asin") {
                return Ok(asin.to_string());
            }
        }
        if !term.is_empty() {
            let results = self.search(limiter, &term).await?;
            // Eigener Suchbegriff: erster Treffer; EAN: erster Treffer, wenn er zum Namen passt
            let picked = if query.is_empty() { pick_ean_result(&results, &watch.name) } else { results.iter().find(|r| !r.sponsored).or(results.first()).map(|r| r.asin.clone()) };
            if let Some(asin) = picked {
                return Ok(asin);
            }
            if !query.is_empty() {
                return Err(CheckError::lookup(format!("Bei Amazon nicht gefunden (Suche nach „{term}“ ohne Treffer)")));
            }
        }
        let mut tried = vec![];
        if !term.is_empty() {
            tried.push(term.clone());
        }
        for name in names {
            let results = self.search(limiter, &name).await?;
            if let Some((asin, title)) = pick_name_result(&results, &name) {
                crate::info!("Amazon · {}: über Namenssuche „{name}“ gefunden: {title} ({asin})", watch.name);
                return Ok(asin);
            }
            tried.push(name);
        }
        Err(CheckError::lookup(format!(
            "Bei Amazon nicht gefunden (Suche nach {} ohne passenden Treffer) – falls vorhanden, Produkt-URL oder ASIN im Gerät eintragen",
            tried.iter().map(|t| format!("„{t}“")).collect::<Vec<_>>().join(", ")
        )))
    }
}

#[async_trait]
impl Source for Amazon {
    fn id(&self) -> &'static str {
        "amazon"
    }
    fn label(&self) -> &'static str {
        "Amazon"
    }
    fn query_label(&self) -> &'static str {
        "Amazon-URL oder ASIN (leer = automatisch über EAN)"
    }
    fn query_placeholder(&self) -> &'static str {
        "https://www.amazon.de/dp/B0…"
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![
            OptionDef::bool("include_new", "Neuware von Amazon berücksichtigen", true),
            OptionDef::bool("include_marketplace", "Auch Neuware von Drittanbietern", false),
            OptionDef::bool("include_warehouse", "Amazon Warehouse (Gebrauchtware) berücksichtigen", true),
            OptionDef::select(
                "warehouse_min_condition",
                "Warehouse: mindestens Zustand",
                "good",
                vec![("like_new", "Wie neu"), ("very_good", "Sehr gut"), ("good", "Gut"), ("acceptable", "Akzeptabel")],
            ),
        ]
    }

    fn matches_input(&self, input: &str) -> bool {
        static BARE: OnceLock<Regex> = OnceLock::new();
        let input = input.trim();
        input.contains("amazon.") || input.contains("amzn.") || BARE.get_or_init(|| Regex::new(r"^B0[A-Z0-9]{8}$").unwrap()).is_match(input)
    }

    async fn identify(&self, input: &str) -> Result<Identity, ResolveError> {
        let input = input.trim();
        let mut asin = extract_asin(input);
        let limiter = self.limiter(self.default_rate);
        if asin.is_none() && input.contains("amzn.") {
            // Kurzlink: Weiterleitung auflösen
            if let Ok(Some((_, final_url))) = self.fetch(&limiter, input, &[]).await {
                asin = extract_asin(&final_url);
            }
        }
        let asin = asin.ok_or_else(|| ResolveError::Invalid("In der Amazon-URL steht keine ASIN – bitte die Produktseite (…/dp/ASIN) verwenden".into()))?;
        // Titel ist nur Zugabe – Amazon blockt gern, dann bleibt er leer
        let title = match self.fetch(&limiter, &format!("{}/dp/{asin}", self.base), &[]).await {
            Ok(Some((page, _))) => product_title(&page).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Identity { title, ean: String::new(), query: format!("{}/dp/{asin}", self.base) })
    }

    fn filter_offers(&self, offers: Vec<Offer>, watch: &Watch, _settings: &Settings, _user_coords: Option<Coords>) -> Vec<Offer> {
        let cfg = watch.source("amazon");
        let (new, marketplace, warehouse) = (cfg.opt_bool("include_new", true), cfg.opt_bool("include_marketplace", false), cfg.opt_bool("include_warehouse", true));
        let min_rank = condition_rank(cfg.opt_str("warehouse_min_condition", "good"));
        offers
            .into_iter()
            .filter(|o| match o.store_id.as_deref().unwrap_or("") {
                "new" => new,
                "new_marketplace" => marketplace,
                id if id.starts_with("warehouse_") => warehouse && condition_rank(o.condition.as_deref().unwrap_or("")) >= min_rank,
                _ => false,
            })
            .collect()
    }

    async fn check(
        &self,
        watch: &Watch,
        settings: &Settings,
        state: StoreState,
        progress: &mut crate::activity::ScanReporter,
    ) -> Result<(ShopResult, StoreState), CheckError> {
        let limiter = self.limiter(settings.amazon_rate_per_min);
        limiter.start_scan();
        let asin = self.resolve_asin(watch, &limiter, &state).await?;
        let state = if state.meta_str("asin") != Some(asin.as_str()) { StoreState::default() } else { state };
        crate::info!("Amazon · {}: ASIN {asin}, Tempo {} Anfragen/Min.", watch.name, settings.amazon_rate_per_min);

        let url = format!("{}/gp/product/ajax/ref=dp_aod_ALL_mbc", self.base);
        let query = [("asin", asin.as_str()), ("m", ""), ("qid", ""), ("smid", ""), ("sourcecode", ""), ("experienceId", "aodAjaxMain")];
        let (page, _) = self
            .fetch(&limiter, &url, &query)
            .await?
            .ok_or_else(|| CheckError::lookup(format!("ASIN {asin} gibt es bei Amazon nicht")))?;
        let offers = parse_offers(&page);
        if offers.is_empty() && !page.contains("aod") {
            return Err(CheckError::Other(
                "Amazon-Angebotsliste nicht lesbar (blockiert oder Seitenaufbau geändert)".into(),
            ));
        }

        let checked = now_iso();
        let mut new_state = state;
        let product_url = format!("{}/dp/{asin}", self.base);
        let mut found = 0;
        for (kind, best) in best_per_kind(&offers) {
            let offer = best.map(|o| {
                found += 1;
                let shipping = o.shipping;
                Offer {
                    price: o.price,
                    shipping,
                    total_price: ((o.price + shipping.unwrap_or(0.0)) * 100.0).round() / 100.0,
                    availability: Some(if o.seller.is_empty() { kind_label(kind).to_string() } else { format!("{} · {}", kind_label(kind), o.seller) }),
                    condition: Some(o.condition.to_string()),
                    url: Some(if kind.starts_with("warehouse_") { format!("{product_url}?aod=1") } else { product_url.clone() }),
                    ..Default::default()
                }
            });
            new_state.stores.insert(kind.to_string(), StoreEntry { name: kind_label(kind).into(), checked: checked.clone(), coordinates: None, offer });
        }
        let title = product_title(&page).or_else(|| new_state.meta_str("title").map(str::to_string)).unwrap_or_default();
        new_state.meta.insert("asin".into(), json!(asin));
        new_state.meta.insert("title".into(), json!(title));
        new_state.meta.insert("search_term".into(), json!(watch.source("amazon").query.trim().to_string()));
        if watch.source("amazon").query.trim().is_empty() {
            new_state.meta.insert("search_term".into(), json!(watch.ean.trim()));
        }
        new_state.meta.insert("search_name".into(), json!(watch.name.trim()));

        let fresh = scan::fresh_offers(&new_state, f64::INFINITY);
        let matching = self.filter_offers(fresh, watch, settings, None);
        let best = matching.first();
        let detail = match best {
            Some(b) => {
                let ship = match b.shipping {
                    Some(s) if s > 0.0 => format!(", {} Versand", format_price(s)),
                    _ => String::new(),
                };
                format!(
                    "{}{ship}{}",
                    b.availability.clone().unwrap_or_default(),
                    if matching.len() > 1 { format!(" · {} passende Angebotsarten", matching.len()) } else { String::new() }
                )
            }
            None if found == 0 => "Keine Angebote bei Amazon".into(),
            None => format!("{found} Angebotsart(en), keine passt zu den Filtern"),
        };
        let stats = crate::model::Stats { planned: KINDS.len(), checked: KINDS.len(), offers: found, total: KINDS.len(), known: KINDS.len(), ..Default::default() };
        progress.report(&stats, &new_state, best);
        let result = ShopResult {
            title,
            url: best.and_then(|b| b.url.clone()).unwrap_or(product_url),
            ean: watch.ean.clone(),
            price: best.map(|b| b.total_price),
            available: best.is_some(),
            detail,
            scan: Some(stats),
            ..Default::default()
        };
        Ok((result, new_state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

    /// Nachgebauter Ausschnitt der „Alle Angebote“-Antwort (Struktur wie bei Amazon, Werte erfunden).
    fn aod_page() -> String {
        fn offer(heading: &str, price: &str, seller: &str, delivery: &str) -> String {
            format!(
                r#"<div id="aod-offer"><div id="aod-offer-heading"><h5> {heading} </h5></div>
                <div class="a-section"><span class="a-price"><span class="a-offscreen">{price}</span></span></div>
                <div id="aod-offer-soldBy"><a>{seller}</a></div><div id="mir-layout-DELIVERY_BLOCK">{delivery}</div></div>"#
            )
        }
        format!(
            r#"<html><span id="aod-asin-title-text"> Sony WH-1000XM6 Kopfhörer </span>
            <div id="aod-pinned-offer"><span class="a-price"><span class="a-offscreen">399,00&nbsp;€</span></span>
              <div id="aod-offer-soldBy"><a>Amazon EU S.à r.l.</a></div> KOSTENLOSE Lieferung </div>
            <div id="aod-offer-list">
              {} {} {} {} {} {} {}
            </div></html>"#,
            offer("Neu", "389,00 €", "TechShop24", "+ 4,99 € Versand"),
            offer("Gebraucht - Wie neu", "329,00 €", "Amazon Resale", "KOSTENLOSE Lieferung"),
            offer("Gebraucht - Wie neu", "340,00 €", "Amazon Resale", "KOSTENLOSE Lieferung"),
            offer("Gebraucht - Sehr gut", "299,50 €", "Amazon Warehouse", "EUR 3,99 Lieferung"),
            offer("Gebraucht - Gut", "279,00 €", "Amazon Warehouse", ""),
            offer("Gebraucht - Akzeptabel", "249,00 €", "Amazon Warehouse", "KOSTENLOSE Lieferung"),
            offer("Gebraucht - Sehr gut", "199,00 €", "Flohmarkt GmbH", "KOSTENLOSE Lieferung"),
        )
    }

    #[test]
    fn maps_conditions() {
        assert_eq!(condition_key("Gebraucht - Wie neu"), Some("like_new"));
        assert_eq!(condition_key("Gebraucht - Sehr gut"), Some("very_good"));
        assert_eq!(condition_key("Gebraucht - Gut"), Some("good"));
        assert_eq!(condition_key("Gebraucht - Akzeptabel"), Some("acceptable"));
        assert_eq!(condition_key("Neu"), Some("new"));
        assert_eq!(condition_key("Used - Like New"), Some("like_new"));
        assert_eq!(condition_key(""), None);
    }

    #[test]
    fn extracts_asin() {
        assert_eq!(extract_asin("https://www.amazon.de/Sony-Kopfh%C3%B6rer/dp/B0DEMO1234/ref=sr_1_1?crid=x").as_deref(), Some("B0DEMO1234"));
        assert_eq!(extract_asin("https://www.amazon.de/gp/product/b0demo1234?th=1").as_deref(), Some("B0DEMO1234"));
        assert_eq!(extract_asin("B0DEMO1234").as_deref(), Some("B0DEMO1234"));
        assert_eq!(extract_asin("https://www.amazon.de/dp/B0DEMO1234").as_deref(), Some("B0DEMO1234"));
        assert_eq!(extract_asin("Kopfhörer"), None);
        assert_eq!(extract_asin("0195950543742"), None);
    }

    #[test]
    fn parses_offer_list_and_picks_cheapest_per_kind() {
        let offers = parse_offers(&aod_page());
        assert_eq!(offers.len(), 8);
        let best: std::collections::HashMap<_, _> = best_per_kind(&offers).into_iter().collect();
        let price = |k: &str| best[k].as_ref().map(|o| o.price);
        assert_eq!(price("new"), Some(399.0)); // Buy Box von Amazon
        assert_eq!(price("new_marketplace"), Some(389.0));
        assert_eq!(best["new_marketplace"].as_ref().unwrap().shipping, Some(4.99));
        assert_eq!(price("warehouse_like_new"), Some(329.0));
        assert_eq!(price("warehouse_very_good"), Some(299.5)); // Flohmarkt GmbH (199 €) ist kein Warehouse
        assert_eq!(best["warehouse_very_good"].as_ref().unwrap().shipping, Some(3.99));
        assert_eq!(price("warehouse_good"), Some(279.0));
        assert_eq!(best["warehouse_good"].as_ref().unwrap().shipping, None);
        assert_eq!(price("warehouse_acceptable"), Some(249.0));
        assert_eq!(best["warehouse_acceptable"].as_ref().unwrap().shipping, Some(0.0));
        assert_eq!(product_title(&aod_page()).as_deref(), Some("Sony WH-1000XM6 Kopfhörer"));
    }

    #[test]
    fn detects_captcha() {
        assert!(is_blocked("<form action=\"/errors/validateCaptcha\">"));
        assert!(is_blocked("Geben Sie die angezeigten Zeichen im Bild ein"));
        assert!(!is_blocked(&aod_page()));
    }

    fn watch(options: serde_json::Value, query: &str, ean: &str) -> Watch {
        let mut watch = Watch { name: "Kopfhörer".into(), target_price: 300.0, ean: ean.into(), ..Default::default() };
        watch.sources.insert("amazon".into(), SourceConfig { enabled: true, query: query.into(), options: options.as_object().unwrap().clone() });
        watch
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn router() -> Router {
        Router::new()
            .route("/gp/product/ajax/ref=dp_aod_ALL_mbc", get(|Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q["asin"], "B0DEMO1234");
                Html(aod_page())
            }))
            .route("/s", get(|Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q["k"], "4548736146000");
                Html(r#"<div data-component-type="s-search-result" data-asin=""></div>
                        <div data-component-type="s-search-result" data-asin="B0DEMO1234"></div>"#)
            }))
    }

    fn settings() -> Settings {
        Settings { amazon_rate_per_min: 60000, ..Default::default() }
    }

    #[tokio::test]
    async fn warehouse_deal_respects_minimum_condition() {
        let amazon = Amazon::new(&serve(router()).await, "amazon-test-1");
        let mut reporter = crate::activity::ScanReporter::new("amazon-test", "Amazon", "Kopfhörer", None);

        // Standard: Amazon-Neuware + Warehouse ab „Gut“ -> günstigstes ist „Gut“ für 279 €
        let w = watch(json!({}), "https://www.amazon.de/dp/B0DEMO1234", "");
        let (result, state) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(279.0));
        assert!(result.detail.contains("Amazon Warehouse – Gut"), "{}", result.detail);
        assert!(result.url.ends_with("?aod=1"));
        assert_eq!(result.title, "Sony WH-1000XM6 Kopfhörer");
        assert_eq!(state.stores.len(), 6);
        assert_eq!(state.meta_str("asin"), Some("B0DEMO1234"));

        // Warehouse ab „Sehr gut“ -> 299,50 + 3,99 Versand = 303,49
        let w = watch(json!({"warehouse_min_condition": "very_good"}), "B0DEMO1234", "");
        let (result, _) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(303.49));

        // Warehouse aus -> nur Buy Box (399 €)
        let w = watch(json!({"include_warehouse": false}), "B0DEMO1234", "");
        let (result, _) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(399.0));

        // Mit Drittanbieter-Neuware, ohne Warehouse -> 389 + 4,99
        let w = watch(json!({"include_warehouse": false, "include_marketplace": true}), "B0DEMO1234", "");
        let (result, _) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(393.99));

        // Alles aus -> kein passendes Angebot, aber kein Fehler
        let w = watch(json!({"include_warehouse": false, "include_new": false}), "B0DEMO1234", "");
        let (result, _) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert!(result.price.is_none() && result.error.is_none() && result.detail.contains("keine passt"));
    }

    #[tokio::test]
    async fn finds_asin_through_ean_search() {
        let amazon = Amazon::new(&serve(router()).await, "amazon-test-2");
        let mut reporter = crate::activity::ScanReporter::new("amazon-test", "Amazon", "Kopfhörer", None);
        let w = watch(json!({}), "", "4548736146000");
        assert!(amazon.is_active(&w));
        let (result, state) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(279.0));
        assert_eq!(state.meta_str("asin"), Some("B0DEMO1234"));
        assert_eq!(state.meta_str("search_term"), Some("4548736146000"));
    }

    fn result_html(asin: &str, title: &str, sponsored: bool) -> String {
        format!(
            r#"<div data-component-type="s-search-result" data-asin="{asin}" class="s-result-item{}"><h2><span>{title}</span></h2>{}</div>"#,
            if sponsored { " AdHolder" } else { "" },
            if sponsored { r#"<span class="puis-sponsored-label-text">Gesponsert</span>"# } else { "" }
        )
    }

    #[test]
    fn parses_search_results() {
        let page = format!("{}{}", result_html("B0AD000000", "Werbung", true), result_html("B0DEMO1234", "Sony WH-1000XM6 Kopfhörer", false));
        let results = parse_search(&page);
        assert_eq!(results.len(), 2);
        assert!(results[0].sponsored && !results[1].sponsored);
        assert_eq!(results[1].title, "Sony WH-1000XM6 Kopfhörer");
        assert_eq!(pick_ean_result(&results, "Sony WH-1000XM6").as_deref(), Some("B0DEMO1234"));
        assert_eq!(pick_ean_result(&results, "Bosch Akkuschrauber GSR 18V"), None);
        assert_eq!(pick_name_result(&results, "sony wh-1000xm6").map(|r| r.0).as_deref(), Some("B0DEMO1234"));
    }

    #[tokio::test]
    async fn falls_back_to_name_search() {
        let router = Router::new()
            .route("/gp/product/ajax/ref=dp_aod_ALL_mbc", get(|| async { Html(aod_page()) }))
            .route("/s", get(|Query(q): Query<HashMap<String, String>>| async move {
                Html(match q["k"].as_str() {
                    // EAN-Suche liefert nur Ähnliches
                    "4548736146000" => result_html("B0WRONG000", "Sony WH-1000XM4 Kopfhörer", false),
                    "Sony WH-1000XM6" => format!(
                        "{}{}{}",
                        result_html("B0AD000000", "Sony WH-1000XM6 Hülle", true),
                        result_html("B0WRONG000", "Sony WH-1000XM4", false),
                        result_html("B0DEMO1234", "Sony WH-1000XM6 Kabellose Kopfhörer, Schwarz", false)
                    ),
                    _ => String::new(),
                })
            }));
        let amazon = Amazon::new(&serve(router).await, "amazon-test-7");
        let mut reporter = crate::activity::ScanReporter::new("amazon-test", "Amazon", "x", None);
        let mut w = watch(json!({}), "", "4548736146000");
        w.name = "Sony WH-1000XM6".into();
        let (_, state) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(state.meta_str("asin"), Some("B0DEMO1234"));

        // Ohne EAN genügt der Name; ohne passenden Treffer gibt es eine verständliche Meldung
        w.ean = String::new();
        let (_, state) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(state.meta_str("asin"), Some("B0DEMO1234"));
        w.name = "Unbekanntes Gerät XY99".into();
        let err = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        assert!(matches!(err, CheckError::Lookup { .. }) && err.message().contains("XY99"), "{err:?}");
    }

    #[tokio::test]
    async fn captcha_page_counts_as_throttling() {
        let router = Router::new().route("/gp/product/ajax/ref=dp_aod_ALL_mbc", get(|| async {
            Html("<html><form action=\"/errors/validateCaptcha\"></form></html>")
        }));
        let amazon = Amazon::new(&serve(router).await, "amazon-test-3");
        let mut reporter = crate::activity::ScanReporter::new("amazon-test", "Amazon", "x", None);
        let w = watch(json!({}), "B0DEMO1234", "");
        let err = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        assert!(matches!(err, CheckError::RateLimited(_)), "{err:?}");
        assert!(ratelimit::snapshots()["amazon-test-3"].throttled >= 1);
    }

    #[tokio::test]
    async fn unreadable_page_is_an_error_but_empty_list_is_not() {
        let router = Router::new().route("/gp/product/ajax/ref=dp_aod_ALL_mbc", get(|| async { Html("<html>irgendwas</html>") }));
        let amazon = Amazon::new(&serve(router).await, "amazon-test-4");
        let mut reporter = crate::activity::ScanReporter::new("amazon-test", "Amazon", "x", None);
        let w = watch(json!({}), "B0DEMO1234", "");
        assert!(matches!(amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await, Err(CheckError::Other(_))));

        let router = Router::new().route("/gp/product/ajax/ref=dp_aod_ALL_mbc", get(|| async { Html("<div id=\"aod-offer-list\"></div>") }));
        let amazon = Amazon::new(&serve(router).await, "amazon-test-5");
        let (result, _) = amazon.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert!(!result.available && result.detail == "Keine Angebote bei Amazon");
    }

    #[tokio::test]
    async fn identifies_urls() {
        let router = Router::new().route("/dp/B0DEMO1234", get(|| async { Html(r#"<span id="productTitle"> Sony Kopfhörer </span>"#) }));
        let base = serve(router).await;
        let amazon = Amazon::new(&base, "amazon-test-6").fast();
        assert!(amazon.matches_input("https://www.amazon.de/dp/B0DEMO1234") && amazon.matches_input("B0DEMO1234"));
        assert!(!amazon.matches_input("airpods") && !amazon.matches_input("https://www.expert.de/x"));
        let id = amazon.identify("https://www.amazon.de/x/dp/B0DEMO1234?th=1").await.unwrap();
        assert_eq!(id.query, format!("{base}/dp/B0DEMO1234"));
        assert_eq!(id.title, "Sony Kopfhörer");
        assert!(matches!(amazon.identify("https://www.amazon.de/gp/help").await, Err(ResolveError::Invalid(_))));
    }
}
