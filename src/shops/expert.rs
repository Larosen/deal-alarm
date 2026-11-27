//! Preisabfrage bei expert – basierend auf expert-checker reborn (MyDealz @Barney & @Gorex).

use super::common::{best_name_match, http_client, name_terms, plz_coordinates};
use super::{Hit, Identity, OptionDef, ResolveError, Source};
use crate::model::{CheckError, Coords, Offer, ShopResult, StoreState};
use crate::ratelimit::{self, AdaptiveLimiter};
use crate::scan::{self, FetchError, Scannable};
use crate::storage::{Settings, Watch};
use crate::util::haversine_km;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const API: &str = "https://production.brntgs.expert.de";
pub const ONLINE_STORE: &str = "e_2879130";
const BRANCH_BACKUP: &str = include_str!("../../assets/expert_branches.json");
const BRANCH_CACHE: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug)]
pub struct ExpertStore {
    pub id: String,
    pub exp_id: String,
    pub name: String,
    pub coordinates: Option<Coords>,
}

impl Scannable for ExpertStore {
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

#[derive(Clone, Debug, PartialEq)]
pub struct Article {
    pub title: String,
    pub webcode: String,
    pub ean: String,
    pub url: String,
}

pub struct Expert {
    api: String,
    limiter_name: String,
    client: reqwest::Client,
    branches: Mutex<Option<(Instant, Arc<Vec<Value>>)>>,
}

impl Default for Expert {
    fn default() -> Self {
        Expert::new(API, "expert")
    }
}

fn article_info(article: &Value) -> Article {
    let title = article["seoPageTitle"].as_str().filter(|t| !t.is_empty()).or(article["title"].as_str()).unwrap_or("");
    Article {
        title: title.split(" - bei expert kaufen").next().unwrap_or("").to_string(),
        webcode: match &article["webcode"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        },
        ean: article["internationalArticleNumber"].as_str().unwrap_or("").to_string(),
        url: article["link"].as_str().map(|l| format!("https://www.expert.de{l}")).unwrap_or_default(),
    }
}

/// Webcode aus einer expert-URL oder direkter Eingabe ermitteln.
pub fn extract_webcode(query: &str) -> Option<String> {
    let query = query.trim();
    if query.contains("expert.de") {
        let path = query.split('?').next()?.split('#').next()?;
        let slug = path.trim_end_matches('/').rsplit('/').next()?;
        let digits: String = slug.chars().take_while(|c| c.is_ascii_digit()).collect();
        return (!digits.is_empty()).then_some(digits);
    }
    (!query.is_empty() && query.chars().all(|c| c.is_ascii_digit())).then(|| query.to_string())
}

pub fn is_ean(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) && matches!(value.len(), 8 | 12 | 13 | 14)
}

pub fn store_url(product_url: &str, store_id: &str) -> String {
    if store_id == ONLINE_STORE { product_url.to_string() } else { format!("{product_url}?branch_id={store_id}") }
}

fn f64_of(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Wandelt eine pricepds-Antwort in ein Angebot um (`None` = nicht verfügbar).
pub fn parse_offer(data: &Value) -> Option<Offer> {
    let price_info = &data["price"];
    let brutto = f64_of(&price_info["bruttoPrice"]);
    if brutto == 0.0 {
        return None;
    }
    let checkout = f64_of(&data["promotionPrice"]["checkoutPrice"]);
    let price = if checkout != 0.0 { checkout } else { brutto };
    let online_stock = price_info["onlineStock"].as_i64().unwrap_or(0);
    let mut shipping = 0.0;
    if online_stock > 0 {
        if let Some(first) = price_info["shipmentArray"].get(0) {
            shipping = f64_of(&first["shipmentBruttoPrice"]);
        }
    }
    let round = |v: f64| (v * 100.0).round() / 100.0;
    Some(Offer {
        price: round(price),
        shipping: Some(round(shipping)),
        total_price: round(price + shipping),
        online_stock,
        store_stock: price_info["storeStock"].as_i64().unwrap_or(0),
        on_display: price_info["itemOnDisplay"]["onDisplay"].as_bool().unwrap_or(false),
        ..Default::default()
    })
}

/// Filter des Geräts: Ausstellungsstücke, nur lokal verfügbar, Entfernung.
pub fn apply_filters(offers: Vec<Offer>, cfg: &crate::storage::SourceConfig, user_coords: Option<Coords>) -> Vec<Offer> {
    let include_local = cfg.opt_bool("include_local", false);
    let user_coords = if include_local { user_coords } else { None };
    let max_distance = match cfg.opt_f64("max_distance", 0.0) {
        d if d > 0.0 => d,
        _ => f64::INFINITY,
    };
    offers
        .into_iter()
        .filter_map(|mut offer| {
            if let (Some(user), Some(store)) = (user_coords, offer.coordinates) {
                offer.distance_km = Some(haversine_km(user, store).round() as i64);
            }
            if offer.on_display && !cfg.opt_bool("include_display", false) {
                return None;
            }
            if offer.online_stock <= 0 {
                // nur lokal verfügbar: nur wenn gewünscht und in Reichweite
                let distance = offer.distance_km?;
                if user_coords.is_none() || offer.store_stock <= 0 || distance as f64 > max_distance {
                    return None;
                }
            }
            Some(offer)
        })
        .collect()
}

fn describe(best: Option<&Offer>, count: usize) -> String {
    let Some(best) = best else { return "In keiner Filiale verfügbar".into() };
    let mut parts = vec![best.store_name.clone().unwrap_or_default()];
    if best.online_stock > 0 {
        parts.push("versandfähig".into());
    } else {
        parts.push(format!(
            "nur lokal ({} km)",
            best.distance_km.map_or("?".to_string(), |d| d.to_string())
        ));
    }
    if best.on_display {
        parts.push("Ausstellungsstück".into());
    }
    if count > 1 {
        parts.push(format!("{count} passende Angebote"));
    }
    parts.join(", ")
}

impl Expert {
    pub fn new(api: &str, limiter_name: &str) -> Self {
        Expert {
            api: api.trim_end_matches('/').to_string(),
            limiter_name: limiter_name.to_string(),
            client: http_client("application/json"),
            branches: Mutex::new(None),
        }
    }

    pub async fn search_articles(&self, term: &str) -> Result<Vec<Article>, String> {
        let response = self
            .client
            .get(format!("{}/api/search/suggest", self.api))
            .query(&[("q", term), ("storeId", ONLINE_STORE)])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let data: Value = response.json().await.map_err(|e| e.to_string())?;
        Ok(data["articleSuggest"]
            .as_array()
            .map(|list| list.iter().map(|entry| article_info(&entry["article"])).collect())
            .unwrap_or_default())
    }

    async fn article_by(&self, key: &str, value: &str) -> Result<Option<Article>, String> {
        let response = self
            .client
            .get(format!("{}/api/search/article", self.api))
            .query(&[(key, value)])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == 204 {
            return Ok(None);
        }
        let data: Value = response.error_for_status().map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
        let article = article_info(&data);
        Ok((!article.webcode.is_empty()).then_some(article))
    }

    /// Artikel zur EAN oder `None`, wenn expert ihn nicht führt.
    pub async fn find_by_ean(&self, ean: &str) -> Result<Option<Article>, String> {
        self.article_by("ean", ean).await
    }

    /// Bester zum Namen passender Artikel der expert-Suche (Modellnummer muss übereinstimmen).
    pub async fn find_by_name(&self, name: &str) -> Result<Option<Article>, String> {
        let hits = self.search_articles(name).await?;
        Ok(best_name_match(name, hits.iter().map(|a| a.title.as_str())).map(|(i, _)| hits[i].clone()))
    }

    /// Löst URL, Webcode, EAN oder Suchbegriff (erster Treffer) zu Produktinfos auf.
    pub async fn resolve(&self, query: &str) -> Result<Article, ResolveError> {
        let fail = |e: String| ResolveError::Failed(e);
        if is_ean(query) {
            return self
                .find_by_ean(query.trim())
                .await
                .map_err(fail)?
                .ok_or_else(|| ResolveError::NotFound(format!("EAN {} gibt es bei expert nicht", query.trim())));
        }
        let Some(webcode) = extract_webcode(query) else {
            let hits = self.search_articles(query).await.map_err(fail)?;
            return hits
                .into_iter()
                .next()
                .ok_or_else(|| ResolveError::NotFound(format!("Kein expert-Produkt für '{query}' gefunden")));
        };
        self.article_by("webcode", &webcode)
            .await
            .map_err(fail)?
            .ok_or_else(|| ResolveError::NotFound(format!("Webcode {webcode} bei expert unbekannt")))
    }

    /// Alle Webcodes, unter denen expert denselben Artikel (gleiche EAN) führt – {webcode: URL}.
    ///
    /// expert listet einen Artikel teils mehrfach mit unterschiedlichen Preisen.
    pub async fn find_listings(&self, product: &Article) -> BTreeMap<String, String> {
        let mut listings = BTreeMap::from([(product.webcode.clone(), product.url.clone())]);
        if product.ean.is_empty() {
            return listings;
        }
        if let Ok(hits) = self.search_articles(&product.title).await {
            for hit in hits {
                if hit.ean == product.ean && !hit.webcode.is_empty() && !hit.url.is_empty() {
                    listings.entry(hit.webcode).or_insert(hit.url);
                }
            }
        } // Zusatzsuche ist optional
        listings
    }

    /// Alle expert-Filialen (24 h gecacht, mitgelieferte Liste als Fallback).
    async fn get_branches(&self) -> Arc<Vec<Value>> {
        let mut cache = self.branches.lock().await;
        if let Some((time, branches)) = cache.as_ref() {
            if time.elapsed() < BRANCH_CACHE {
                return branches.clone();
            }
        }
        let params = json!({
            "lat": 0, "lng": 0, "maxResults": 500, "device": "Mozilla/5.0", "source": "HTML5",
            "withWebsite": true, "conditions": {"storeFinderResultFilter": "ALL"},
        });
        let fetched = async {
            let response = self
                .client
                .post(format!("{}/_api/storeFinder/searchStoresByGeoLocation", self.api))
                .header("Cookie", format!("fmarktcookie={ONLINE_STORE}"))
                .json(&params)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            let list: Vec<Value> = response.json().await.ok()?;
            (!list.is_empty()).then_some(list)
        }
        .await;
        let branches = Arc::new(fetched.unwrap_or_else(|| serde_json::from_str(BRANCH_BACKUP).expect("expert_branches.json")));
        *cache = Some((Instant::now(), branches.clone()));
        branches
    }

    /// Alle zu prüfenden Filialen im einheitlichen Scanner-Format.
    pub async fn list_stores(&self, all_branches: bool) -> Vec<ExpertStore> {
        let mut stores = vec![];
        if all_branches {
            for branch in self.get_branches().await.iter() {
                let store = &branch["store"];
                let mut name = store["name"].as_str().unwrap_or("").to_string();
                if let Some(city) = store["city"].as_str().filter(|c| !c.is_empty() && !name.contains(c)) {
                    name = format!("{name} {city}");
                }
                let coordinates = match (store["latitude"].as_f64(), store["longitude"].as_f64()) {
                    (Some(lat), Some(lon)) if lat != 0.0 => Some((lat, lon)),
                    _ => None,
                };
                let (Some(id), Some(exp_id)) = (store["id"].as_str(), store["expId"].as_str()) else { continue };
                stores.push(ExpertStore { id: id.into(), exp_id: exp_id.into(), name, coordinates });
            }
        }
        stores.push(ExpertStore {
            id: ONLINE_STORE.into(),
            exp_id: ONLINE_STORE.split('_').nth(1).unwrap_or("").into(),
            name: "expert Onlineshop".into(),
            coordinates: None,
        });
        stores
    }

    async fn fetch_store(
        client: reqwest::Client,
        api: String,
        limiter: Arc<AdaptiveLimiter>,
        webcodes: Arc<Vec<String>>,
        listings: Arc<BTreeMap<String, String>>,
        store: ExpertStore,
    ) -> Result<Option<Offer>, FetchError> {
        let mut best: Option<Offer> = None;
        let mut errors = vec![];
        for webcode in webcodes.iter() {
            let response = ratelimit::get(
                &client, &limiter, &format!("{api}/api/pricepds"),
                &[("webcode", webcode.as_str()), ("storeId", store.exp_id.as_str())],
                ratelimit::THROTTLE_STATUS, 4, Duration::from_secs(20),
            )
            .await?;
            if response.status() == 404 {
                continue;
            }
            let data = match response.error_for_status() {
                Ok(r) => r.json::<Value>().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    // expert antwortet für einzelne Artikelnummern teils mit 500 – die übrigen zählen trotzdem
                    errors.push(e);
                    continue;
                }
            };
            if let Some(mut offer) = parse_offer(&data) {
                if best.as_ref().is_none_or(|b| offer.total_price < b.total_price) {
                    offer.webcode = Some(webcode.clone());
                    offer.url = Some(store_url(&listings[webcode], &store.id));
                    best = Some(offer);
                }
            }
        }
        if !errors.is_empty() && errors.len() == webcodes.len() {
            return Err(FetchError::Other(errors.pop().unwrap()));
        }
        Ok(best)
    }
}

#[async_trait]
impl Source for Expert {
    fn id(&self) -> &'static str {
        "expert"
    }
    fn label(&self) -> &'static str {
        "expert"
    }
    fn query_label(&self) -> &'static str {
        "Produkt-URL oder Webcode (leer = automatisch über EAN)"
    }
    fn query_placeholder(&self) -> &'static str {
        "https://www.expert.de/shop/unsere-produkte/…html"
    }
    fn options(&self) -> Vec<OptionDef> {
        vec![
            OptionDef::bool("all_branches", "Alle Filialen durchsuchen (versandfähige Angebote)", true),
            OptionDef::bool("include_display", "Ausstellungsstücke berücksichtigen", false),
            OptionDef::bool("include_local", "Auch nur lokal verfügbare Angebote", false),
            OptionDef::number("max_distance", "Max. Entfernung (km, PLZ in den Einstellungen)", 50.0),
        ]
    }

    fn matches_input(&self, input: &str) -> bool {
        input.contains("expert.de") || is_ean(input) || extract_webcode(input).is_some()
    }

    async fn identify(&self, input: &str) -> Result<Identity, ResolveError> {
        let product = self.resolve(input).await?;
        Ok(Identity { title: product.title, ean: product.ean, query: product.url })
    }

    async fn lookup_ean(&self, ean: &str) -> Result<Option<Identity>, String> {
        Ok(self.find_by_ean(ean).await?.map(|p| Identity { title: p.title, ean: p.ean, query: p.url }))
    }

    async fn search(&self, term: &str) -> Option<Result<Vec<Hit>, String>> {
        Some(self.search_articles(term).await.map(|list| {
            list.into_iter().map(|a| Hit { title: a.title, ean: a.ean, query: a.url, source: "expert".into() }).collect()
        }))
    }

    fn filter_offers(&self, offers: Vec<Offer>, watch: &Watch, _settings: &Settings, user_coords: Option<Coords>) -> Vec<Offer> {
        apply_filters(offers, &watch.source("expert"), user_coords)
    }

    fn offer_url(&self, state: &StoreState, store_id: &str) -> Option<String> {
        Some(store_url(state.meta_str("product_url")?, store_id))
    }

    async fn check(
        &self,
        watch: &Watch,
        settings: &Settings,
        state: StoreState,
        progress: &mut crate::activity::ScanReporter,
    ) -> Result<(ShopResult, StoreState), CheckError> {
        let cfg = watch.source("expert");
        let query = cfg.query.trim();
        let product = if !query.is_empty() {
            self.resolve(query).await.map_err(CheckError::from)?
        } else {
            // Artikel wurde über einen anderen Shop ausgewählt: per EAN zuordnen, sonst über den Namen
            let mut found = None;
            if !watch.ean.trim().is_empty() {
                found = self.find_by_ean(watch.ean.trim()).await.map_err(CheckError::Other)?;
            }
            if found.is_none() {
                for name in name_terms(watch, "expert") {
                    if let Some(p) = self.find_by_name(&name).await.map_err(CheckError::Other)? {
                        crate::info!("expert · {}: über Namenssuche „{name}“ gefunden: {} (Webcode {})", watch.name, p.title, p.webcode);
                        found = Some(p);
                        break;
                    }
                }
            }
            match found {
                Some(p) => p,
                None => {
                    return Ok((
                        ShopResult {
                            ean: watch.ean.clone(),
                            detail: if watch.ean.trim().is_empty() {
                                format!("Nicht im expert-Sortiment („{}“)", watch.name)
                            } else {
                                format!("Nicht im expert-Sortiment (EAN {}, auch per Namenssuche nicht gefunden)", watch.ean)
                            },
                            ..Default::default()
                        },
                        StoreState::default(),
                    ))
                }
            }
        };
        let listings = self.find_listings(&product).await;
        let webcodes: Vec<String> = listings.keys().cloned().collect(); // BTreeMap: sortiert
        // Filialstatus gilt nur für diese Artikel
        let state = if state.meta.get("webcodes") != Some(&json!(webcodes)) { StoreState::default() } else { state };

        let limiter = ratelimit::get_limiter(&self.limiter_name, settings.expert_rate_per_min.max(1) as f64, 2.0);
        limiter.start_scan();
        let stores = self.list_stores(cfg.opt_bool("all_branches", true)).await;
        let budget = Some(settings.expert_stores_per_cycle).filter(|b| *b > 0).map(|b| b as usize);
        crate::info!(
            "expert · {}: {}, {} Artikelnummer(n) {}, {} Filialen",
            watch.name, product.title, webcodes.len(), webcodes.join(", "), stores.len()
        );
        let user_coords = plz_coordinates(&settings.plz).await;
        let (client, api) = (self.client.clone(), self.api.clone());
        let (webcodes_arc, listings_arc) = (Arc::new(webcodes.clone()), Arc::new(listings));
        let fetch = {
            let limiter = limiter.clone();
            move |store: ExpertStore| {
                Self::fetch_store(client.clone(), api.clone(), limiter.clone(), webcodes_arc.clone(), listings_arc.clone(), store)
            }
        };
        let (mut state, stats) = scan::run(
            &stores, state, fetch, budget, settings.expert_workers.max(1) as usize, Some(progress),
            |offer| !apply_filters(vec![offer.clone()], &cfg, user_coords).is_empty(),
        )
        .await;

        let fresh = scan::fresh_offers(&state, settings.price_max_age_hours.max(1) as f64);
        let offers = apply_filters(fresh, &cfg, user_coords);
        let best = offers.first();
        let mut detail = describe(best, offers.len());
        if webcodes.len() > 1 {
            detail.push_str(&format!(" · {} expert-Artikelnummern", webcodes.len()));
        }
        let result = ShopResult {
            title: product.title.clone(),
            url: best.and_then(|b| b.url.clone()).unwrap_or_else(|| product.url.clone()),
            ean: product.ean.clone(),
            price: best.map(|b| b.total_price),
            available: best.is_some(),
            detail,
            scan: Some(stats),
            ..Default::default()
        };
        state.meta.insert("webcodes".into(), json!(webcodes));
        state.meta.insert("product_url".into(), json!(product.url));
        Ok((result, state))
    }
}

impl From<ResolveError> for CheckError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::NotFound(m) | ResolveError::Invalid(m) => CheckError::lookup(m),
            ResolveError::Failed(m) => CheckError::Other(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SourceConfig;
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;

    #[test]
    fn extracts_webcodes() {
        let url = "https://www.expert.de/shop/unsere-produkte/tv-audio/kopfhorer/12940100537-airpods-pro-3.html?branch_id=e_1";
        assert_eq!(extract_webcode(url).as_deref(), Some("12940100537"));
        assert_eq!(extract_webcode("12940100537").as_deref(), Some("12940100537"));
        assert_eq!(extract_webcode("airpods pro"), None);
        assert!(is_ean("0195950543742") && !is_ean("12345") && !is_ean("abc"));
    }

    #[test]
    fn offer_uses_checkout_price_and_shipping() {
        let data = json!({
            "price": {"bruttoPrice": 249, "onlineStock": 2, "storeStock": 0,
                      "shipmentArray": [{"shipmentBruttoPrice": 4.99}], "itemOnDisplay": {"onDisplay": false}},
            "promotionPrice": {"checkoutPrice": 229},
        });
        let offer = parse_offer(&data).unwrap();
        assert_eq!(offer.price, 229.0);
        assert_eq!(offer.total_price, 233.99);
        assert!(parse_offer(&json!({"price": {}})).is_none());
    }

    fn offer(online: i64, local: i64, display: bool, coords: Option<Coords>, price: f64) -> Offer {
        Offer { price, total_price: price, online_stock: online, store_stock: local, on_display: display, coordinates: coords, ..Default::default() }
    }

    fn cfg(options: Value) -> SourceConfig {
        SourceConfig { enabled: true, options: options.as_object().unwrap().clone(), ..Default::default() }
    }

    #[test]
    fn filters_display_and_local_offers() {
        let hamburg = Some((53.55, 9.99));
        let munich = Some((48.14, 11.58));
        let offers = vec![
            offer(1, 0, false, None, 100.0),
            offer(1, 0, true, None, 90.0),        // Ausstellungsstück
            offer(0, 1, false, hamburg, 80.0),     // nur lokal, nah
            offer(0, 1, false, munich, 70.0),      // nur lokal, weit
        ];
        let user = Some((53.5, 10.0));
        let prices = |c: &SourceConfig, user| apply_filters(offers.clone(), c, user).iter().map(|o| o.total_price).collect::<Vec<_>>();
        assert_eq!(prices(&cfg(json!({})), user), [100.0]);
        assert_eq!(prices(&cfg(json!({"include_display": true})), user), [100.0, 90.0]);
        assert_eq!(prices(&cfg(json!({"include_local": true, "max_distance": 50})), user), [100.0, 80.0]);
        assert_eq!(prices(&cfg(json!({"include_local": true, "max_distance": 0})), user), [100.0, 80.0, 70.0]);
        // ohne PLZ keine lokalen Angebote
        assert_eq!(prices(&cfg(json!({"include_local": true})), None), [100.0]);
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn mock() -> Router {
        Router::new()
            .route("/api/search/article", get(|Query(q): Query<HashMap<String, String>>| async move {
                if q.get("ean").map(String::as_str) == Some("4006381333931") || q.contains_key("webcode") {
                    Json(json!({"webcode": "111", "title": "Tool", "internationalArticleNumber": "4006381333931", "link": "/p/111-tool.html"}))
                } else {
                    Json(json!({}))
                }
            }))
            .route("/api/search/suggest", get(|| async {
                Json(json!({"articleSuggest": [
                    {"article": {"webcode": "111", "title": "Tool", "internationalArticleNumber": "4006381333931", "link": "/p/111-tool.html"}},
                    {"article": {"webcode": "222", "title": "Tool", "internationalArticleNumber": "4006381333931", "link": "/p/222-tool.html"}},
                    {"article": {"webcode": "333", "title": "Other", "internationalArticleNumber": "999", "link": "/p/333.html"}},
                ]}))
            }))
            .route("/api/pricepds", get(|Query(q): Query<HashMap<String, String>>| async move {
                let webcode = q["webcode"].as_str();
                let price = if webcode == "111" { 120.0 } else { 99.0 };
                if webcode == "222" && q["storeId"] == "2879130" {
                    // einzelne Artikelnummern antworten teils mit Fehler
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
                }
                (axum::http::StatusCode::OK, Json(json!({"price": {"bruttoPrice": price, "onlineStock": 3, "shipmentArray": [{"shipmentBruttoPrice": 0}]}})))
            }))
    }

    #[tokio::test]
    async fn resolves_by_ean_and_finds_all_listings() {
        let expert = Expert::new(&serve(mock()).await, "expert-test-1");
        let product = expert.resolve("4006381333931").await.unwrap();
        assert_eq!(product.webcode, "111");
        assert!(product.url.ends_with("/p/111-tool.html"));
        let listings = expert.find_listings(&product).await;
        assert_eq!(listings.keys().collect::<Vec<_>>(), ["111", "222"]);
        assert!(matches!(expert.resolve("0000000000000").await, Err(ResolveError::NotFound(_))));
    }

    #[tokio::test]
    async fn check_takes_cheapest_listing_and_survives_single_listing_error() {
        let expert = Expert::new(&serve(mock()).await, "expert-test-2");
        let mut watch = Watch { name: "Tool".into(), target_price: 100.0, ean: "4006381333931".into(), ..Default::default() };
        watch.sources.insert("expert".into(), SourceConfig {
            enabled: true,
            options: json!({"all_branches": false}).as_object().unwrap().clone(),
            ..Default::default()
        });
        let settings = Settings { expert_rate_per_min: 60000, ..Default::default() };
        let mut reporter = crate::activity::ScanReporter::new("expert-test", "expert", "Tool", None);
        let (result, state) = expert.check(&watch, &settings, StoreState::default(), &mut reporter).await.unwrap();
        // Listing 222 liefert im Onlineshop 500 – 111 (120 €) zählt trotzdem
        assert_eq!(result.price, Some(120.0));
        assert!(result.detail.contains("2 expert-Artikelnummern"));
        assert_eq!(state.meta["webcodes"], json!(["111", "222"]));
        assert_eq!(state.stores[ONLINE_STORE].offer.as_ref().unwrap().webcode.as_deref(), Some("111"));
        assert_eq!(expert.offer_url(&state, ONLINE_STORE).unwrap(), state.meta["product_url"].as_str().unwrap());
        assert!(expert.offer_url(&state, "e_5").unwrap().ends_with("?branch_id=e_5"));
    }

    #[tokio::test]
    async fn unknown_ean_is_no_error() {
        let expert = Expert::new(&serve(mock()).await, "expert-test-3");
        let mut watch = Watch { ean: "0000000000000".into(), ..Default::default() };
        watch.sources.insert("expert".into(), SourceConfig { enabled: true, ..Default::default() });
        let mut reporter = crate::activity::ScanReporter::new("expert-test", "expert", "x", None);
        let (result, _) = expert.check(&watch, &Settings::default(), StoreState::default(), &mut reporter).await.unwrap();
        assert!(result.error.is_none() && !result.available && result.detail.contains("Nicht im expert-Sortiment"));
    }

    #[tokio::test]
    async fn unknown_ean_falls_back_to_name() {
        let expert = Expert::new(&serve(mock()).await, "expert-test-5");
        let mut watch = Watch { name: "Tool".into(), ean: "0000000000000".into(), ..Default::default() };
        watch.sources.insert("expert".into(), SourceConfig {
            enabled: true,
            options: json!({"all_branches": false}).as_object().unwrap().clone(),
            ..Default::default()
        });
        let settings = Settings { expert_rate_per_min: 60000, ..Default::default() };
        let mut reporter = crate::activity::ScanReporter::new("expert-test", "expert", "Tool", None);
        let (result, _) = expert.check(&watch, &settings, StoreState::default(), &mut reporter).await.unwrap();
        assert_eq!(result.ean, "4006381333931");
        assert!(result.price.is_some());
        assert!(expert.find_by_name("Other 42").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bundled_branches_cover_all_stores() {
        let expert = Expert::new("http://127.0.0.1:9", "expert-test-4"); // unerreichbar → Fallback
        let stores = expert.list_stores(true).await;
        assert!(stores.len() > 300, "{}", stores.len());
        assert_eq!(stores.last().unwrap().id, ONLINE_STORE);
        assert!(stores.iter().all(|s| !s.name.is_empty()));
    }
}
