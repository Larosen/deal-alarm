//! Gemeinsame Datentypen: Angebote, Filialstatus, Prüfergebnisse.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub type Coords = (f64, f64);

/// Ein Angebot einer Filiale / eines Marktes / einer Anbieter-Kategorie.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Offer {
    pub price: f64,
    pub shipping: Option<f64>,
    pub total_price: f64,
    pub online_stock: i64,
    pub store_stock: i64,
    pub on_display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_km: Option<i64>,
    /// Zustand bei Quellen mit Gebrauchtware (Amazon: new, like_new, very_good, good, acceptable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    // Beim Auslesen ergänzt (siehe scan::fresh_offers)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinates: Option<Coords>,
}

impl Offer {
    pub fn simple(price: f64) -> Self {
        Offer { price, total_price: price, ..Default::default() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct StoreEntry {
    pub name: String,
    pub checked: String,
    pub coordinates: Option<Coords>,
    pub offer: Option<Offer>,
}

/// Gemerkte Preise je Filiale plus Metadaten des Scans (z. B. Webcodes, Produktpfad).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct StoreState {
    pub meta: Map<String, Value>,
    pub stores: BTreeMap<String, StoreEntry>,
}

impl StoreState {
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.meta.get(key).and_then(Value::as_str)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Stats {
    pub planned: usize,
    pub checked: usize,
    pub failed: usize,
    pub aborted: bool,
    pub offers: usize,
    pub total: usize,
    pub known: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Suggestion {
    pub url: String,
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ShopResult {
    pub title: String,
    pub url: String,
    pub ean: String,
    pub price: Option<f64>,
    pub available: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan: Option<Stats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Suggestion>,
    pub checked_at: String,
    pub is_deal: bool,
    pub alerted: bool,
    pub alert_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notified_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_error: Option<String>,
}

impl ShopResult {
    pub fn error(message: impl Into<String>) -> Self {
        ShopResult { error: Some(message.into()), ..Default::default() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    pub time: String,
    pub shop: String,
    pub price: f64,
}

/// Fehler einer Shop-Prüfung.
#[derive(Debug, Clone)]
pub enum CheckError {
    /// Produkt nicht (eindeutig) gefunden – alte Filialpreise gehören evtl. zu einem anderen Produkt.
    Lookup { message: String, suggestion: Option<Suggestion> },
    /// Der Shop blockiert oder drosselt.
    RateLimited(String),
    Other(String),
}

impl CheckError {
    pub fn lookup(message: impl Into<String>) -> Self {
        CheckError::Lookup { message: message.into(), suggestion: None }
    }
    pub fn message(&self) -> &str {
        match self {
            CheckError::Lookup { message, .. } => message,
            CheckError::RateLimited(m) | CheckError::Other(m) => m,
        }
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl From<reqwest::Error> for CheckError {
    fn from(e: reqwest::Error) -> Self {
        CheckError::Other(e.to_string())
    }
}

impl From<crate::ratelimit::RateLimited> for CheckError {
    fn from(e: crate::ratelimit::RateLimited) -> Self {
        CheckError::RateLimited(e.0)
    }
}
