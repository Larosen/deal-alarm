//! Preisabfrage bei **MediaMarkt** und **Saturn** (gleiche Plattform) – Onlinepreis.
//!
//! Aufbau laut Shop-Bericht vom 28.11.2026 (echter Browser, Privat-IP, ohne Cookies abrufbar):
//!
//! - Suche `/de/search.html?query=<EAN oder Name>` enthält ein JSON-LD `ItemList` mit Name, URL und Preis.
//! - Produktseite `/de/product/_<slug>-<Artikelnummer>.html` enthält JSON-LD `BuyAction`, dessen
//!   `object` (Typ `ProductGroup`) `gtin13`, `name` und `offers` (Preis, Verfügbarkeit) trägt.
//!
//! Preise je Markt liefert nur eine GraphQL-Schnittstelle, die ohne Sitzung 403 antwortet – deshalb
//! wird (vorerst) nur der Onlinepreis geprüft. Eine Anfrage pro Prüfung.

use super::common::{best_name_match, http_client_with_cookies, name_match, name_terms, NAME_MATCH_MIN};
use super::euronics::{jsonld_items, same_ean};
use super::{Confidence, Identity, Located, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Offer, ShopResult, StoreEntry, StoreState, Suggestion};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::storage::{Settings, Watch};
use crate::util::{age_hours, format_price, now_iso, parse_price_value};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const STORE_ID: &str = "online";

/// Ein Treffer der Suche (aus dem `ItemList`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub name: String,
    pub url: String,
    pub price: Option<f64>,
}

/// Produktdaten der Produktseite.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Product {
    pub name: String,
    pub gtin: String,
    pub price: Option<f64>,
    pub available: bool,
}

/// Treffer aus dem JSON-LD `ItemList` der Suchseite.
pub fn parse_search(page: &str) -> Vec<SearchHit> {
    let mut hits = vec![];
    for item in jsonld_items(page) {
        if item["@type"] != "ItemList" {
            continue;
        }
        for element in item["itemListElement"].as_array().into_iter().flatten() {
            let product = if element["item"].is_object() { &element["item"] } else { element };
            let Some(url) = product["url"].as_str().filter(|u| u.contains("/product/")) else { continue };
            hits.push(SearchHit {
                name: product["name"].as_str().unwrap_or("").to_string(),
                url: url.to_string(),
                price: parse_price_value(&product["offers"]["price"]),
            });
        }
    }
    hits
}

/// Produktdaten aus `BuyAction.object` (bzw. einem `Product`/`ProductGroup` auf oberster Ebene).
pub fn parse_product(page: &str) -> Option<Product> {
    for item in jsonld_items(page) {
        let object = if item["@type"] == "BuyAction" { &item["object"] } else { &item };
        let kind = object["@type"].as_str().unwrap_or("");
        if kind != "Product" && kind != "ProductGroup" {
            continue;
        }
        let mut offers = object["offers"].clone();
        if let Value::Array(list) = &offers {
            offers = list.first().cloned().unwrap_or(Value::Null);
        }
        let price = parse_price_value(&offers["price"]).or_else(|| parse_price_value(&offers["lowPrice"]));
        let availability = offers["availability"].as_str().unwrap_or("");
        let gtin = [&object["gtin13"], &object["gtin"], &object["gtin14"]]
            .iter()
            .find_map(|v| match v {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .unwrap_or_default();
        if price.is_none() && gtin.is_empty() {
            continue;
        }
        return Some(Product {
            name: object["name"].as_str().unwrap_or("").to_string(),
            gtin,
            price,
            available: price.is_some() && !["OutOfStock", "SoldOut", "Discontinued"].iter().any(|k| availability.contains(k)),
        });
    }
    None
}

fn is_product_url(url: &str) -> bool {
    url.contains("/product/") && url.contains(".html")
}

/// Bundles sind nicht das gesuchte Gerät, auch wenn die Modellnummer passt.
fn is_bundle(title: &str, wanted: &str) -> bool {
    let t = title.to_lowercase();
    let w = wanted.to_lowercase();
    ["inkl.", "bundle", "+ "].iter().any(|k| t.contains(k) && !w.contains(k))
}

pub struct MediaSaturn {
    id: &'static str,
    label: &'static str,
    host: &'static str,
    base: String,
    client: reqwest::Client,
}

impl MediaSaturn {
    pub fn mediamarkt() -> Self {
        Self::new("mediamarkt", "MediaMarkt", "mediamarkt.de", "https://www.mediamarkt.de")
    }

    pub fn saturn() -> Self {
        Self::new("saturn", "Saturn", "saturn.de", "https://www.saturn.de")
    }

    pub fn new(id: &'static str, label: &'static str, host: &'static str, base: &str) -> Self {
        MediaSaturn {
            id,
            label,
            host,
            base: base.trim_end_matches('/').to_string(),
            client: http_client_with_cookies("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        }
    }

    fn limiter(&self, settings: &Settings) -> Arc<AdaptiveLimiter> {
        ratelimit::get_limiter(&format!("www.{}", self.host), settings.mediamarkt_rate_per_min.max(1) as f64, 1.0)
    }

    /// Eingetragene URL auf die (Test-)Basis umschreiben – der Pfad bleibt.
    fn rebase(&self, url: &str) -> String {
        match reqwest::Url::parse(url) {
            Ok(u) => format!("{}{}", self.base, u.path()),
            Err(_) => url.to_string(),
        }
    }

    async fn fetch(&self, limiter: &AdaptiveLimiter, url: &str, query: &[(&str, &str)]) -> Result<Option<String>, CheckError> {
        let label = self.label;
        let response = ratelimit::get(&self.client, limiter, url, query, ratelimit::THROTTLE_STATUS, 2, Duration::from_secs(30))
            .await
            .map_err(|_| CheckError::RateLimited(format!("{label} drosselt die Anfragen (HTTP 429/503) – später erneut versuchen.")))?;
        let status = response.status().as_u16();
        if matches!(status, 404 | 410) {
            return Ok(None);
        }
        if status == 403 {
            return Err(CheckError::RateLimited(format!(
                "{label} blockiert die Anfragen (HTTP 403) – Rechenzentrums-IPs werden oft gesperrt, am besten zuhause laufen lassen."
            )));
        }
        let response = response.error_for_status()?;
        let final_url = response.url().to_string();
        let body = response.text().await?;
        crate::debug::capture(self.id, &final_url, status, &body);
        Ok(Some(body))
    }

    async fn search(&self, limiter: &AdaptiveLimiter, term: &str) -> Result<Vec<SearchHit>, CheckError> {
        let url = format!("{}/de/search.html", self.base);
        Ok(self.fetch(limiter, &url, &[("query", term)]).await?.map(|p| parse_search(&p)).unwrap_or_default())
    }

    async fn product(&self, limiter: &AdaptiveLimiter, url: &str) -> Result<Option<Product>, CheckError> {
        Ok(self.fetch(limiter, &self.rebase(url), &[]).await?.and_then(|p| parse_product(&p)))
    }

    fn cache_key(&self, watch: &Watch) -> String {
        let q = watch.source(self.id).query.trim().to_string();
        format!("{}|{}", if q.is_empty() { watch.ean.trim().to_string() } else { q }, watch.name.trim())
    }

    fn cached(&self, watch: &Watch, state: &StoreState) -> bool {
        state.meta_str("product_url").is_some()
            && state.meta_str("search_key") == Some(self.cache_key(watch).as_str())
            && state.meta_str("resolved_at").is_none_or(|t| age_hours(t) < super::REDISCOVER_HOURS)
    }

    /// Produkt-URL: eingetragen, sonst über die EAN-Suche (gtin der Produktseite muss passen), sonst über den Namen.
    async fn resolve(&self, watch: &Watch, limiter: &AdaptiveLimiter, state: &StoreState) -> Result<(String, Confidence), CheckError> {
        let label = self.label;
        let query = watch.source(self.id).query.trim().to_string();
        if query.contains(self.host) && is_product_url(&query) {
            return Ok((self.rebase(query.split(['?', '#']).next().unwrap_or(&query)), Confidence::Ean));
        }
        if self.cached(watch, state) {
            return Ok((state.meta_str("product_url").unwrap_or_default().to_string(), Confidence::Ean));
        }
        let ean = watch.ean.trim().to_string();
        let term = if query.is_empty() { ean.clone() } else { query.clone() };
        let names = if query.is_empty() { name_terms(watch, self.id) } else { vec![] };
        if term.is_empty() && names.is_empty() {
            return Err(CheckError::Other(format!("Keine {label}-URL, EAN oder Gerätename hinterlegt")));
        }
        let mut suggestion: Option<(String, String)> = None;
        let mut tried = vec![];
        if !term.is_empty() {
            tried.push(term.clone());
            let hits = self.search(limiter, &term).await?;
            if !query.is_empty() {
                if let Some(hit) = hits.first() {
                    return Ok((self.rebase(&hit.url), Confidence::Name));
                }
            } else {
                for hit in hits.iter().take(2) {
                    let Some(product) = self.product(limiter, &hit.url).await? else { continue };
                    if same_ean(&product.gtin, &ean) {
                        return Ok((self.rebase(&hit.url), Confidence::Ean));
                    }
                    if suggestion.is_none() && !is_bundle(&hit.name, &watch.name) {
                        suggestion = Some((self.rebase(&hit.url), hit.name.clone()));
                    }
                }
            }
        }
        for name in names {
            tried.push(name.clone());
            let hits: Vec<SearchHit> = self.search(limiter, &name).await?.into_iter().filter(|h| !is_bundle(&h.name, &name)).collect();
            let Some((i, score)) = best_name_match(&name, hits.iter().map(|h| h.name.as_str())) else { continue };
            let hit = &hits[i];
            if ean.is_empty() {
                crate::info!("{label} · {}: über Namenssuche „{name}“ gefunden: {}", watch.name, hit.name);
                return Ok((self.rebase(&hit.url), Confidence::Name));
            }
            match self.product(limiter, &hit.url).await? {
                Some(p) if same_ean(&p.gtin, &ean) => return Ok((self.rebase(&hit.url), Confidence::Ean)),
                Some(p) if p.gtin.is_empty() && score >= NAME_MATCH_MIN => return Ok((self.rebase(&hit.url), Confidence::Name)),
                _ => {
                    if suggestion.is_none() {
                        suggestion = Some((self.rebase(&hit.url), hit.name.clone()));
                    }
                }
            }
        }
        match suggestion {
            Some((url, title)) if name_match(&watch.name, &title) >= NAME_MATCH_MIN => Err(CheckError::Lookup {
                message: format!(
                    "{label} führt EAN {ean} nicht. Gefunden wurde „{title}“ – falls das dasselbe Gerät ist (Händler-Variante), als {label}-Produkt übernehmen."
                ),
                suggestion: Some(Suggestion { url, title }),
            }),
            _ => Err(CheckError::lookup(format!(
                "Bei {label} nicht gefunden (Suche nach {} ohne passenden Treffer) – falls vorhanden, Produkt-URL im Gerät eintragen",
                tried.iter().map(|t| format!("„{t}“")).collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

#[async_trait]
impl Source for MediaSaturn {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        self.label
    }
    fn query_label(&self) -> &'static str {
        "Produkt-URL (leer = automatisch über EAN bzw. Gerätename)"
    }
    fn query_placeholder(&self) -> &'static str {
        if self.id == "saturn" { "https://www.saturn.de/de/product/…" } else { "https://www.mediamarkt.de/de/product/…" }
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![]
    }

    fn matches_input(&self, input: &str) -> bool {
        input.contains(self.host) && is_product_url(input)
    }

    async fn identify(&self, input: &str) -> Result<Identity, ResolveError> {
        let url = input.trim().split(['?', '#']).next().unwrap_or("").to_string();
        let limiter = ratelimit::get_limiter(&format!("www.{}", self.host), 10.0, 1.0);
        let product = self.product(&limiter, &url).await.ok().flatten();
        Ok(Identity {
            title: product.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            ean: product.map(|p| p.gtin).unwrap_or_default(),
            query: url,
        })
    }

    async fn locate(&self, watch: &Watch, settings: &Settings) -> Result<Option<Located>, String> {
        let limiter = self.limiter(settings);
        match self.resolve(watch, &limiter, &StoreState::default()).await {
            Ok((url, confidence)) => Ok(Some(Located { title: String::new(), query: url, confidence })),
            Err(CheckError::Lookup { suggestion: Some(s), .. }) => Ok(Some(Located { query: s.url, title: s.title, confidence: Confidence::Variant })),
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
        let label = self.label;
        let limiter = self.limiter(settings);
        limiter.start_scan();
        let from_cache = self.cached(watch, &state);
        let (url, _) = self.resolve(watch, &limiter, &state).await?;
        let page = self.fetch(&limiter, &url, &[]).await?.ok_or_else(|| CheckError::lookup(format!("{label}-Produktseite gibt es nicht mehr ({url})")))?;
        let product = parse_product(&page)
            .ok_or_else(|| CheckError::Other(format!("{label}-Produktseite nicht lesbar (blockiert oder Seitenaufbau geändert)")))?;
        let offer = product.price.filter(|_| product.available).map(|price| Offer {
            price,
            shipping: None,
            total_price: price,
            availability: Some("Onlineshop".into()),
            title: Some(product.name.clone()),
            url: Some(url.clone()),
            ..Default::default()
        });
        let mut new_state = StoreState::default();
        new_state.stores.insert(
            STORE_ID.into(),
            StoreEntry { name: format!("{label} Onlineshop"), checked: now_iso(), coordinates: None, offer: offer.clone() },
        );
        new_state.meta.insert("product_url".into(), json!(url));
        new_state.meta.insert("title".into(), json!(product.name));
        new_state.meta.insert("search_key".into(), json!(self.cache_key(watch)));
        let resolved_at = state
            .meta_str("resolved_at")
            .filter(|_| from_cache && state.meta_str("product_url") == Some(url.as_str()))
            .map(str::to_string)
            .unwrap_or_else(now_iso);
        new_state.meta.insert("resolved_at".into(), json!(resolved_at));
        let stats = crate::model::Stats { planned: 1, checked: 1, offers: offer.is_some() as usize, total: 1, known: 1, ..Default::default() };
        progress.report(&stats, &new_state, offer.as_ref());
        if let Some(o) = &offer {
            crate::info!("{label} · {}: {}", watch.name, format_price(o.price));
        }
        Ok((
            ShopResult {
                title: product.name.clone(),
                url,
                ean: product.gtin.clone(),
                price: offer.as_ref().map(|o| o.total_price),
                available: offer.is_some(),
                detail: if offer.is_some() { "Onlineshop (Versand nicht eingerechnet)".into() } else { "Online derzeit nicht verfügbar".into() },
                ..Default::default()
            },
            new_state,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, response::Html, routing::get, Router};
    use std::collections::HashMap;

    const TV_URL_PATH: &str = "/de/product/_samsung-gq65s95h-oled-4k-samsung-vision-ai-smart-tv-65-zoll-163-cm-uhd-4k-smart-tv-3047334.html";
    const TV_EAN: &str = "8806099134839";
    const TV_NAME: &str = "Samsung GQ65S95HFTXZG S95H Vision AI Smart TV (2026) OLED, 65\"";

    fn ld(v: Value) -> String {
        format!(r#"<script type="application/ld+json">{v}</script>"#)
    }

    /// Suchseite wie am 28.11.2026 (gekürzt).
    fn search_page(base: &str, items: &[(&str, &str, f64)]) -> String {
        let list: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(i, (name, path, price))| json!({"@type": "ListItem", "position": i + 1,
                "item": {"@type": "Product", "name": name, "offers": {"@type": "Offer", "price": price, "priceCurrency": "EUR"}, "url": format!("{base}{path}")}}))
            .collect();
        format!("<html>{}<script>window.__PRELOADED_STATE__ = {{}}</script></html>", ld(json!({"@context": "https://schema.org", "@type": "ItemList", "itemListElement": list})))
    }

    /// Produktseite: BuyAction → ProductGroup mit gtin13 und offers (kein Product auf oberster Ebene).
    fn product_page(name: &str, gtin: &str, price: f64) -> String {
        format!(
            "<html>{}{}</html>",
            ld(json!({"@context": "https://schema.org/", "@type": "BuyAction", "object": {"@type": "ProductGroup", "brand": {"@type": "Brand", "name": "SAMSUNG"},
                "sku": "3047334", "gtin13": gtin, "name": name, "energyEfficiencyScaleMin": "G",
                "offers": {"@type": "Offer", "priceCurrency": "EUR", "price": price, "availability": "https://schema.org/InStock"}}})),
            ld(json!({"@type": "BreadcrumbList", "itemListElement": []}))
        )
    }

    async fn serve() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let b = base.clone();
        let router = Router::new()
            .route("/de/search.html", get(move |Query(q): Query<HashMap<String, String>>| {
                let b = b.clone();
                async move {
                    Html(match q["query"].as_str() {
                        TV_EAN => search_page(&b, &[("SAMSUNG 65 Zoll OLED 4K S95H Vision AI Smart TV (GQ65S95HFTXZG), Glare Free (2026)", TV_URL_PATH, 1699.0)]),
                        _ => search_page(&b, &[
                            ("SAMSUNG 65 Zoll OLED 4K S95H Vision AI Smart TV (GQ65S95HFTXZG), Glare Free (2026)", TV_URL_PATH, 1699.0),
                            ("SAMSUNG QE65Q80AAT", "/de/product/_samsung-qe65q80aat-1.html", 899.0),
                        ]),
                    })
                }
            }))
            .route(TV_URL_PATH, get(|| async { Html(product_page("SAMSUNG 65 Zoll OLED 4K S95H Vision AI Smart TV (GQ65S95HFTXZG)", TV_EAN, 1699.0)) }));
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    }

    fn watch(query: &str, ean: &str, name: &str) -> Watch {
        let mut w = Watch { name: name.into(), ean: ean.into(), target_price: 1500.0, ..Default::default() };
        w.sources.insert("mediamarkt".into(), SourceConfig { enabled: true, query: query.into(), ..Default::default() });
        w
    }

    fn settings() -> Settings {
        Settings { mediamarkt_rate_per_min: 60000, ..Default::default() }
    }

    #[test]
    fn parses_real_structures() {
        let hits = parse_search(&search_page("https://www.mediamarkt.de", &[("SAMSUNG GQ65S95H", TV_URL_PATH, 1699.0)]));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].price, Some(1699.0));
        assert!(hits[0].url.ends_with("-3047334.html"));
        let p = parse_product(&product_page("SAMSUNG …", TV_EAN, 1699.0)).unwrap();
        assert_eq!((p.gtin.as_str(), p.price, p.available), (TV_EAN, Some(1699.0), true));
        assert!(name_match(TV_NAME, "SAMSUNG 65 Zoll OLED 4K S95H Vision AI Smart TV (GQ65S95HFTXZG), Glare Free (2026)") >= NAME_MATCH_MIN);
    }

    #[tokio::test]
    async fn finds_by_ean_and_by_name_and_reads_price() {
        let base = serve().await;
        let mm = MediaSaturn::new("mediamarkt", "MediaMarkt", "mediamarkt.de", &base);
        let mut reporter = crate::activity::ScanReporter::new("mm-test", "MediaMarkt", "x", None);
        let (result, state) = mm.check(&watch("", TV_EAN, TV_NAME), &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!((result.price, result.ean.as_str()), (Some(1699.0), TV_EAN));
        assert!(state.meta_str("product_url").unwrap().ends_with(TV_URL_PATH));
        // nur über den Namen
        let (result, _) = mm.check(&watch("", "", TV_NAME), &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(1699.0));
        // eingetragene URL
        let w = watch(&format!("https://www.mediamarkt.de{TV_URL_PATH}"), "", "egal");
        let (result, _) = mm.check(&w, &settings(), StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.price, Some(1699.0));
        // andere Größe -> nicht gefunden
        let err = mm.check(&watch("", "", "Samsung GQ55S95HFTXZG"), &settings(), StoreState::default(), &mut reporter).await.unwrap_err();
        assert!(matches!(err, CheckError::Lookup { .. }), "{err:?}");
        let hit = mm.locate(&watch("", TV_EAN, TV_NAME), &settings()).await.unwrap().unwrap();
        assert_eq!(hit.confidence, Confidence::Ean);
        assert!(mm.matches_input(&format!("https://www.mediamarkt.de{TV_URL_PATH}")) && !mm.matches_input("https://www.saturn.de/de/product/_x-1.html"));
    }
}
