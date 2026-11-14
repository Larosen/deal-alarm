//! Gemeinsame Hilfsfunktionen für die Shop-Abfragen.

use crate::model::Coords;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT as UA};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0";

/// HTTP-Client mit Browser-Kennung; `accept` setzt den Accept-Header.
pub fn http_client(accept: &str) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(UA, HeaderValue::from_static(USER_AGENT));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE,de;q=0.9,en;q=0.5"));
    headers.insert(ACCEPT, HeaderValue::from_str(accept).unwrap_or_else(|_| HeaderValue::from_static("*/*")));
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP-Client")
}

static PLZ_CACHE: OnceLock<Mutex<HashMap<String, Option<Coords>>>> = OnceLock::new();

/// Geo-Koordinaten für eine deutsche PLZ (`None` bei Fehler; Erfolge werden gemerkt).
pub async fn plz_coordinates(plz: &str) -> Option<Coords> {
    let plz = plz.trim();
    if plz.len() != 5 || !plz.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let cache = PLZ_CACHE.get_or_init(Default::default);
    if let Some(Some(hit)) = cache.lock().unwrap().get(plz) {
        return Some(*hit);
    }
    let found = lookup_plz(plz).await;
    if found.is_some() {
        cache.lock().unwrap().insert(plz.to_string(), found);
    }
    found
}

async fn lookup_plz(plz: &str) -> Option<Coords> {
    let response = reqwest::Client::new()
        .get(format!("https://api.zippopotam.us/de/{plz}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: serde_json::Value = response.json().await.ok()?;
    let place = data["places"].get(0)?;
    Some((place["latitude"].as_str()?.parse().ok()?, place["longitude"].as_str()?.parse().ok()?))
}

#[cfg(test)]
pub fn seed_plz(plz: &str, coords: Coords) {
    PLZ_CACHE.get_or_init(Default::default).lock().unwrap().insert(plz.to_string(), Some(coords));
}

/// Wie [`http_client`], aber mit Cookie-Speicher (Shops, die eine Sitzung erwarten).
pub fn http_client_with_cookies(accept: &str) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(UA, HeaderValue::from_static(USER_AGENT));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE,de;q=0.9,en;q=0.5"));
    headers.insert(ACCEPT, HeaderValue::from_str(accept).unwrap_or_else(|_| HeaderValue::from_static("*/*")));
    reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP-Client")
}
