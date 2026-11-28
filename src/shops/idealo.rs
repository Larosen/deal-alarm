//! Bestpreis bei **idealo** (idealo.de) – der günstigste Preis über alle Händler im Preisvergleich.
//!
//! idealo ist kein Shop, sondern ein Preisvergleich: Pro Prüfung wird die Produktseite
//! (`/preisvergleich/OffersOfProduct/<id>_….html`) abgerufen. Daraus kommen die Angebotsliste
//! (Händler, Preis, Gesamtpreis inkl. Versand; Tabelle „Alle Angebote“) und als Rückfall der
//! niedrigste Preis aus den strukturierten Produktdaten (`AggregateOffer.lowPrice`). Einmal am Tag
//! wird zusätzlich der Preisverlauf gelesen (Tiefstpreis der letzten 3 Monate). Der Dealalarm
//! kommt, sobald irgendein Händler bei idealo den Wunschpreis erreicht.
//!
//! Seitenaufbau laut Shop-Bericht vom 28.11.2026 (echter Browser, Privat-IP).
//!
//! Zugeordnet wird ein Gerät über die hinterlegte idealo-URL, sonst über die idealo-Suche nach
//! der EAN und – findet die nichts – nach dem Gerätenamen (Modellnummer muss passen).

use super::common::{best_name_match, http_client_with_cookies, name_terms};
use super::euronics::{parse_product_page, same_ean};
use super::{Confidence, Identity, Located, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Offer, ShopResult, StoreEntry, StoreState};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::storage::{Settings, Watch};
use crate::util::{age_hours, first_price, format_price, now_iso, parse_price_value};
use scraper::{ElementRef, Html, Selector};
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

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("CSS-Selektor")
}

fn text_of(el: ElementRef) -> String {
    el.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn absolute(base: &str, href: &str) -> String {
    if href.starts_with("http") { href.to_string() } else { format!("{base}{href}") }
}

/// Produkt-Treffer einer Suchergebnisseite: (absolute URL, Titel), in Reihenfolge.
///
/// Gezählt werden nur Treffer in `[data-testid="result-list"] [data-testid="result-item"]` mit einem
/// Link auf eine Produktseite – Einzelangebote ohne Produktseite und die Empfehlungs-Slider weiter
/// unten (`data-testid="productLink"`) nicht. Fehlt die Liste (anderer Aufbau), werden alle
/// Produktlinks der Seite genommen.
pub fn search_links(page: &str, base: &str) -> Vec<(String, String)> {
    let doc = Html::parse_document(page);
    let items = sel(r#"[data-testid="result-list"] [data-testid="result-item"]"#);
    let link_sel = sel(r#"a[href*="OffersOfProduct"]"#);
    let title_sel = sel(r#"[data-testid="product-summary-title"]"#);
    let mut links: Vec<(String, String)> = vec![];
    let mut any_item = false;
    for item in doc.select(&items) {
        any_item = true;
        let Some(href) = item.select(&link_sel).filter_map(|a| a.value().attr("href")).next() else { continue };
        let url = absolute(base, href.split(['?', '#']).next().unwrap_or(href));
        if links.iter().any(|(u, _)| product_id(u) == product_id(&url)) {
            continue;
        }
        let title = item.select(&title_sel).next().map(text_of).filter(|t| !t.is_empty()).unwrap_or_else(|| title_from_url(&url));
        links.push((url, title));
    }
    if any_item {
        return links;
    }
    search_links_fallback(page, base)
}

fn search_links_fallback(page: &str, base: &str) -> Vec<(String, String)> {
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

/// Ein Angebot aus der Angebotsliste der Produktseite.
#[derive(Debug, Clone, PartialEq)]
pub struct ShopOffer {
    pub shop: String,
    pub price: f64,
    /// Gesamtpreis inkl. Versand, falls angegeben
    pub total: Option<f64>,
    /// Weiterleitung zum Händler (`/relocator/relocate?…`)
    pub link: String,
}

/// Angebotsliste (`li.productOffers-listItem`). Händler und Preis stehen im Attribut
/// `data-mtrx-click` (JSON), Gesamtpreis in `.productOffers-listItemOfferShippingDetails`.
pub fn parse_offers(page: &str, base: &str) -> Vec<ShopOffer> {
    let doc = Html::parse_document(page);
    let item_sel = sel("li.productOffers-listItem");
    let price_sel = sel(".productOffers-listItemOfferPrice");
    let shipping_sel = sel(".productOffers-listItemOfferShippingDetails");
    let logo_sel = sel("img.productOffers-listItemOfferShopV2LogoImage");
    let link_sel = sel("a.productOffers-listItemOfferCtaLeadout, a.productOffers-listItemOfferPrice, a.productOffers-listItemTitle");
    let mut offers = vec![];
    for item in doc.select(&item_sel) {
        let mtrx: serde_json::Value = item.value().attr("data-mtrx-click").and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
        let shop = mtrx["shop_name"]
            .as_str()
            .map(str::to_string)
            .or_else(|| item.select(&logo_sel).next().and_then(|i| i.value().attr("alt")).map(|a| a.split(" - Shop").next().unwrap_or(a).trim().to_string()))
            .unwrap_or_default();
        let price = item
            .select(&price_sel)
            .next()
            .map(text_of)
            .and_then(|t| first_price(&t))
            .or_else(|| parse_price_value(&mtrx["products"][0]["price"]));
        let Some(price) = price else { continue };
        let shipping_text = item.select(&shipping_sel).next().map(text_of).unwrap_or_default();
        // „1.699,00 € inkl. Versand“ – andere Formulierungen ohne Gesamtpreis lassen wir offen
        let total = if shipping_text.to_lowercase().contains("inkl") { first_price(&shipping_text) } else { None };
        let link = item.select(&link_sel).filter_map(|a| a.value().attr("href")).next().map(|h| absolute(base, h)).unwrap_or_default();
        offers.push(ShopOffer { shop, price, total, link });
    }
    offers
}

/// Name der Variante zur URL aus der `ProductGroup` (der `Product`-Name ist der Gruppenname ohne Größe,
/// z. B. „Samsung GQ-S95HFT“ statt „Samsung GQ65S95HFT (65 Zoll)“).
pub fn variant_name(page: &str, url: &str) -> Option<String> {
    let id = product_id(url)?;
    super::euronics::jsonld_items(page).into_iter().find_map(|item| {
        item["hasVariant"].as_array()?.iter().find_map(|v| {
            let vurl = v["url"].as_str()?;
            (product_id(vurl).as_deref() == Some(id.as_str())).then(|| v["name"].as_str().map(str::to_string)).flatten()
        })
    })
}

/// Tiefstpreis aus dem Preisverlauf (`/price-chart/…/history`, Preise in Cent).
pub fn history_low(json: &serde_json::Value) -> Option<f64> {
    json["data"].as_array()?.iter().filter_map(|p| p["y"].as_f64()).filter(|y| *y > 0.0).fold(None, |m: Option<f64>, y| Some(m.map_or(y, |m| m.min(y)))).map(|c| c / 100.0)
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

    /// Link zum Angebot eines bestimmten Händlers (z. B. „euronics“) über die idealo-Angebotsliste:
    /// die Weiterleitung wird verfolgt, geliefert wird die Ziel-URL ohne Parameter – nur wenn sie
    /// beim Händler (`host_contains`) landet. Hilft, wo die Suche des Händlers gesperrt ist.
    pub async fn merchant_url(&self, watch: &Watch, settings: &Settings, shop_contains: &str, host_contains: &str) -> Option<String> {
        let limiter = self.limiter(settings.idealo_rate_per_min);
        let url = self.resolve_url(watch, &limiter, &StoreState::default()).await.ok()?;
        let (page, _) = self.fetch(&limiter, &url, &[]).await.ok()??;
        let offer = parse_offers(&page, &self.base).into_iter().find(|o| o.shop.to_lowercase().contains(shop_contains) && !o.link.is_empty())?;
        let (_, target) = self.fetch(&limiter, &offer.link, &[]).await.ok()??;
        let target_url = reqwest::Url::parse(&target).ok()?;
        target_url.host_str()?.contains(host_contains).then(|| format!("{}://{}{}", target_url.scheme(), target_url.host_str().unwrap_or(""), target_url.path()))
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
        let title = variant_name(&page, &url)
            .or_else(|| Some(product.title.clone()).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| title_from_url(&url));

        // Angebotsliste: ein Eintrag je Händler (günstigstes Angebot), für die Tabelle „Alle Angebote“
        let checked = now_iso();
        let mut new_state = StoreState::default();
        let shop_offers = parse_offers(&page, &self.base);
        for o in &shop_offers {
            let key = format!("shop:{}", o.shop.to_lowercase());
            let total = o.total.unwrap_or(o.price);
            if new_state.stores.get(&key).and_then(|e| e.offer.as_ref()).is_some_and(|e| e.total_price <= total) {
                continue;
            }
            let offer = Offer {
                price: o.price,
                shipping: o.total.map(|t| ((t - o.price) * 100.0).round() / 100.0),
                total_price: total,
                availability: Some(format!("bei {} (über idealo)", o.shop)),
                title: Some(title.clone()),
                url: Some(if o.link.is_empty() { url.clone() } else { o.link.clone() }),
                ..Default::default()
            };
            new_state.stores.insert(key, StoreEntry { name: o.shop.clone(), checked: checked.clone(), coordinates: None, offer: Some(offer) });
        }
        // Ohne lesbare Liste: Bestpreis aus den Produktdaten
        if new_state.stores.is_empty() {
            let offer = product.price.filter(|_| product.available).map(|price| Offer {
                price,
                shipping: None,
                total_price: price,
                availability: Some("Bestpreis bei idealo (ohne Versand)".into()),
                title: Some(title.clone()),
                url: Some(url.clone()),
                ..Default::default()
            });
            new_state.stores.insert(STORE_ID.into(), StoreEntry { name: "idealo Bestpreis".into(), checked: checked.clone(), coordinates: None, offer });
        }

        // Preisverlauf einmal am Tag (Tiefstpreis 3 Monate)
        let mut low = state.meta.get("low_3m").and_then(|v| v.as_f64()).filter(|_| state.meta_str("product_url") == Some(url.as_str()));
        let mut low_at = state.meta_str("low_3m_at").map(str::to_string).filter(|_| low.is_some());
        if low_at.as_deref().is_none_or(|t| age_hours(t) >= 24.0) {
            if let Some(id) = product_id(&url) {
                let history_url = format!("{}/price-chart/sites/1/products/{id}/history", self.base);
                if let Ok(Some((body, _))) = self.fetch(&limiter, &history_url, &[("period", "P3M")]).await {
                    if let Some(l) = serde_json::from_str(&body).ok().as_ref().and_then(history_low) {
                        low = Some(l);
                        low_at = Some(now_iso());
                    }
                }
            }
        }
        if let (Some(l), Some(t)) = (low, &low_at) {
            new_state.meta.insert("low_3m".into(), json!(l));
            new_state.meta.insert("low_3m_at".into(), json!(t));
        }

        new_state.meta.insert("product_url".into(), json!(url));
        new_state.meta.insert("title".into(), json!(title));
        let query = watch.source("idealo").query.trim().to_string();
        let term = if query.is_empty() { watch.ean.trim().to_string() } else { query };
        new_state.meta.insert("search_key".into(), json!(format!("{term}|{}", watch.name.trim())));
        let resolved_at = state.meta_str("resolved_at").filter(|_| from_cache && state.meta_str("product_url") == Some(url.as_str())).map(str::to_string).unwrap_or_else(now_iso);
        new_state.meta.insert("resolved_at".into(), json!(resolved_at));

        let offers = crate::scan::fresh_offers(&new_state, f64::INFINITY);
        let best = offers.first().cloned();
        let n = offers.len();
        let stats = crate::model::Stats { planned: n.max(1), checked: n.max(1), offers: n, total: n.max(1), known: n.max(1), ..Default::default() };
        progress.report(&stats, &new_state, best.as_ref());
        let mut detail = match &best {
            Some(o) => {
                let shops = count.map(|c| c as usize).unwrap_or(n);
                format!("{} · {shops} Angebote", o.store_name.clone().or(o.availability.clone()).unwrap_or_default())
            }
            None => "Derzeit keine Angebote bei idealo".into(),
        };
        if let Some(l) = low {
            detail.push_str(&format!(" · Tiefstpreis 3 Monate {}", format_price(l)));
        }
        let result = ShopResult {
            title,
            url: url.clone(),
            ean: product.ean.clone(),
            price: best.as_ref().map(|o| o.total_price),
            available: best.is_some(),
            detail,
            scan: Some(stats),
            ..Default::default()
        };
        if let Some(o) = &best {
            crate::info!("idealo · {}: günstigstes Angebot {} ({})", watch.name, format_price(o.total_price), o.store_name.clone().unwrap_or_default());
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

    /// Aufbau der echten Seiten (Shop-Bericht 28.11.2026, gekürzt).
    const REAL_SEARCH: &str = r#"<div data-testid="result-list">
      <div data-testid="result-item" class="-mr-px"><div class="flex w-full" data-testid="resultItemLink">
        <a href="https://www.idealo.de/preisvergleich/OffersOfProduct/210372235_-gq65s95hft-65-zoll-samsung.html" data-testid="link">
          <div data-testid="product-summary-title">Samsung GQ65S95HFT (65 Zoll)</div></a></div>
        <div data-testid="detailed-price-info">24 Angebote | ab | <span data-testid="detailed-price-info-price">ab 1.699,00 €</span></div></div>
      <div data-testid="result-item"><div data-testid="single-offer-content"><span data-testid="shop-name-wrapper">Verkauf durch: gamingoase.de</span>
        <form action="https://www.idealo.de/ipc/prg"></form></div></div>
      <div data-testid="result-item"><a href="https://www.idealo.de/preisvergleich/OffersOfProduct/210372232_-gq-s95hft-samsung.html">
        <div data-testid="product-summary-title">Samsung GQ-S95HFT</div></a></div>
    </div>
    <a data-testid="productLink" href="https://www.idealo.de/preisvergleich/OffersOfProduct/999_-empfehlung.html">Empfehlung</a>"#;

    const REAL_OFFERS: &str = r#"<ul class="productOffers-list" data-import-time="Daten vom 28.11.2026 14:25">
      <li class="productOffers-listItem row row-24" data-mtrx-click='{"shop_name":"Amazon","products":[{"id":210372235,"price":1699.0}],"event":"leadout"}'>
        <a class="productOffers-listItemTitle" href="/relocator/relocate?offerKey=a&amp;pos=1"><span>Samsung OLED S95H</span></a>
        <a class="productOffers-listItemOfferPrice" href="/relocator/relocate?offerKey=a&amp;pos=1">1.699,00 €</a>
        <div class="productOffers-listItemOfferShippingDetails">1.699,00 € inkl. Versand</div>
        <img class="productOffers-listItemOfferShopV2LogoImage" alt="Amazon - Shop aus München">
        <a class="productOffers-listItemOfferCtaLeadout button" href="/relocator/relocate?offerKey=a&amp;pos=1">Zum Shop</a></li>
      <li class="productOffers-listItem" data-mtrx-click='{"shop_name":"euronics.de","products":[{"price":1749.0}]}'>
        <a class="productOffers-listItemOfferPrice" href="/relocator/relocate?offerKey=e">1.749,00 €</a>
        <div class="productOffers-listItemOfferShippingDetails">1.754,99 € inkl. Versand</div>
        <a class="productOffers-listItemOfferCtaLeadout" href="/relocator/relocate?offerKey=e">Zum Shop</a></li>
    </ul>
    <script type="application/ld+json">{"@type":"ProductGroup","name":"Samsung GQ-S95HFT","hasVariant":[
      {"@type":"Product","url":"https://www.idealo.de/preisvergleich/OffersOfProduct/210372235_-gq65s95hft-65-zoll-samsung.html","name":"Samsung GQ65S95HFT (65 Zoll)","sku":"8806099134839"},
      {"@type":"Product","url":"https://www.idealo.de/preisvergleich/OffersOfProduct/210372231_-gq55s95hft-55-zoll-samsung.html"}]}</script>"#;

    #[test]
    fn reads_real_search_offers_and_variant() {
        let links = search_links(REAL_SEARCH, BASE);
        assert_eq!(links.len(), 2, "{links:?}"); // Einzelangebot ohne Link und Empfehlung zählen nicht
        assert_eq!(links[0].1, "Samsung GQ65S95HFT (65 Zoll)");
        let wanted = "Samsung GQ65S95HFTXZG S95H Vision AI Smart TV (2026) OLED, 65\"";
        assert_eq!(best_name_match(wanted, links.iter().map(|(_, t)| t.as_str())).map(|x| x.0), Some(0));

        let offers = parse_offers(REAL_OFFERS, BASE);
        assert_eq!(offers.len(), 2);
        assert_eq!((offers[0].shop.as_str(), offers[0].price, offers[0].total), ("Amazon", 1699.0, Some(1699.0)));
        assert_eq!(offers[0].link, "https://www.idealo.de/relocator/relocate?offerKey=a&pos=1");
        assert_eq!(offers[1].total, Some(1754.99));
        let url = "https://www.idealo.de/preisvergleich/OffersOfProduct/210372235_-gq65s95hft-65-zoll-samsung.html";
        assert_eq!(variant_name(REAL_OFFERS, url).as_deref(), Some("Samsung GQ65S95HFT (65 Zoll)"));
        let history = json!({"siteId":1,"data":[{"x":"2026-08-28","y":209900},{"x":"2026-09-01","y":169900},{"x":"2026-09-02","y":0}]});
        assert_eq!(history_low(&history), Some(1699.0));
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
