//! Quellen (Shops), die ein Gerät beobachten kann.
//!
//! Jede Quelle implementiert [`Source`] und wird in [`all`] eingetragen – mehr ist für eine
//! neue Quelle nicht nötig: Warteschlange, Zeitplan, Alarm-Logik, API und Oberfläche
//! (Formularfelder aus [`Source::options`]) arbeiten generisch über die Quellen.
//! Ein Schritt-für-Schritt-Leitfaden steht im README.

pub mod amazon;
pub mod common;
pub mod euronics;
pub mod expert;
#[cfg(test)]
mod fixtures;
pub mod idealo;
pub mod mediamarkt;

use crate::activity::ScanReporter;
use crate::model::{CheckError, Coords, Offer, ShopResult, StoreState};
use crate::storage::{Settings, Watch};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

/// Ein Formularfeld für die Optionen einer Quelle (die Oberfläche baut daraus den Dialog).
#[derive(Serialize, Clone, Debug)]
pub struct OptionDef {
    pub key: &'static str,
    pub label: &'static str,
    /// "bool", "number" oder "select"
    pub kind: &'static str,
    pub default: Value,
    /// Nur bei "select": (Wert, Beschriftung)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<(&'static str, &'static str)>,
}

impl OptionDef {
    pub fn bool(key: &'static str, label: &'static str, default: bool) -> Self {
        OptionDef { key, label, kind: "bool", default: json!(default), choices: vec![] }
    }
    pub fn number(key: &'static str, label: &'static str, default: f64) -> Self {
        OptionDef { key, label, kind: "number", default: json!(default), choices: vec![] }
    }
    pub fn select(key: &'static str, label: &'static str, default: &'static str, choices: Vec<(&'static str, &'static str)>) -> Self {
        OptionDef { key, label, kind: "select", default: json!(default), choices }
    }
}

/// Beschreibung einer Quelle für die Oberfläche.
#[derive(Serialize, Clone, Debug)]
pub struct SourceInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub query_label: &'static str,
    pub query_placeholder: &'static str,
    pub default_enabled: bool,
    /// Zeigt „Alle Filialpreise“ (Tabelle mit Preisen je Filiale/Angebotsart)
    pub options: Vec<OptionDef>,
}

/// Ein Artikel, wie ihn eine Quelle erkannt hat.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub title: String,
    pub ean: String,
    /// Was im Gerät als Produkt-Angabe der Quelle gespeichert wird (URL, ASIN, …)
    pub query: String,
}

/// Suchtreffer einer Quelle.
#[derive(Serialize, Clone, Debug)]
pub struct Hit {
    pub title: String,
    pub ean: String,
    pub query: String,
    pub source: String,
}

/// Per Suche gefundene Zuordnungen werden nach dieser Zeit erneut gesucht (neue/bessere Angebote).
/// Eingetragene URLs gelten unbegrenzt.
pub const REDISCOVER_HOURS: f64 = 7.0 * 24.0;

/// Wie sicher eine automatische Zuordnung ist.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// gleiche EAN (bzw. eingetragene URL)
    Ean,
    /// Name und Modellnummer stimmen überein
    Name,
    /// ähnliches Gerät, evtl. Händler-Variante – der Nutzer muss bestätigen
    Variant,
}

/// Ergebnis der Suche nach einem Gerät bei einer Quelle (beim Anlegen).
#[derive(Serialize, Clone, Debug)]
pub struct Located {
    /// Produkt-Angabe für das Gerät (URL, ASIN, …)
    pub query: String,
    pub title: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    NotFound(String),
    Invalid(String),
    Failed(String),
}

impl ResolveError {
    pub fn message(&self) -> &str {
        match self {
            ResolveError::NotFound(m) | ResolveError::Invalid(m) | ResolveError::Failed(m) => m,
        }
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    /// Kurze, stabile Kennung (Schlüssel in `watch.sources`, Name des Limiters, …)
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn query_label(&self) -> &'static str;
    fn query_placeholder(&self) -> &'static str;
    /// Optionen pro Gerät (Formularfelder)
    fn options(&self) -> Vec<OptionDef>;
    /// Wird bei neuen Geräten vorausgewählt
    fn default_enabled(&self) -> bool {
        true
    }

    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: self.id(),
            label: self.label(),
            query_label: self.query_label(),
            query_placeholder: self.query_placeholder(),
            default_enabled: self.default_enabled(),
            options: self.options(),
        }
    }

    /// Soll dieses Gerät bei der Quelle geprüft werden? Ohne eigene Angabe wird über die EAN
    /// und – wenn die nichts findet – über den Gerätenamen gesucht.
    fn is_active(&self, watch: &Watch) -> bool {
        let cfg = watch.source(self.id());
        cfg.enabled && (!cfg.query.trim().is_empty() || !watch.ean.trim().is_empty() || !watch.name.trim().is_empty())
    }

    /// Prüft ein Gerät und liefert Ergebnis und neuen Filialstatus.
    async fn check(
        &self,
        watch: &Watch,
        settings: &Settings,
        state: StoreState,
        progress: &mut ScanReporter,
    ) -> Result<(ShopResult, StoreState), CheckError>;

    /// Wendet die Filter des Geräts an (z. B. Zustand, Entfernung). `user_coords` stammt aus der PLZ.
    fn filter_offers(&self, offers: Vec<Offer>, _watch: &Watch, _settings: &Settings, _user_coords: Option<Coords>) -> Vec<Offer> {
        offers
    }

    /// Link zu einem Angebot, falls das Angebot selbst keinen trägt.
    fn offer_url(&self, _state: &StoreState, _store_id: &str) -> Option<String> {
        None
    }

    /// Gehört diese Eingabe (URL, EAN, …) zu dieser Quelle?
    fn matches_input(&self, _input: &str) -> bool {
        false
    }

    /// Erkennt den Artikel aus einer Eingabe, für die `matches_input` zutrifft.
    async fn identify(&self, _input: &str) -> Result<Identity, ResolveError> {
        Err(ResolveError::Invalid("Eingabe wird von dieser Quelle nicht unterstützt".into()))
    }

    /// Sucht den Artikel anhand der EAN (wenn die Quelle das schnell kann; sonst `None`).
    async fn lookup_ean(&self, _ean: &str) -> Result<Option<Identity>, String> {
        Ok(None)
    }

    /// Sucht das Gerät (EAN, sonst Name) wie bei einer Prüfung, ohne Preise abzufragen.
    /// `Ok(None)` = nicht gefunden. Standard: nur über [`Source::lookup_ean`].
    async fn locate(&self, watch: &Watch, _settings: &Settings) -> Result<Option<Located>, String> {
        if watch.ean.trim().is_empty() {
            return Ok(None);
        }
        Ok(self.lookup_ean(watch.ean.trim()).await?.map(|i| Located { query: i.query, title: i.title, confidence: Confidence::Ean }))
    }

    /// Freitextsuche – `None`, wenn die Quelle keine anbietet.
    async fn search(&self, _term: &str) -> Option<Result<Vec<Hit>, String>> {
        None
    }
}

static SOURCES: OnceLock<Vec<Arc<dyn Source>>> = OnceLock::new();
static EURONICS: OnceLock<Arc<euronics::Euronics>> = OnceLock::new();

/// Die Euronics-Quelle mit ihrem Händlerverzeichnis (für die Marktvorschau in den Einstellungen).
pub fn euronics() -> Arc<euronics::Euronics> {
    EURONICS.get_or_init(|| Arc::new(euronics::Euronics::default())).clone()
}

/// Alle Quellen in Anzeigereihenfolge. **Neue Quellen werden hier eingetragen.**
pub fn all() -> &'static [Arc<dyn Source>] {
    SOURCES.get_or_init(|| {
        vec![
            Arc::new(expert::Expert::default()) as Arc<dyn Source>,
            euronics() as Arc<dyn Source>,
            Arc::new(amazon::Amazon::default()),
            Arc::new(mediamarkt::MediaSaturn::mediamarkt()),
            Arc::new(mediamarkt::MediaSaturn::saturn()),
            Arc::new(idealo::Idealo::default()),
        ]
    })
}

pub fn get(id: &str) -> Option<Arc<dyn Source>> {
    all().iter().find(|s| s.id() == id).cloned()
}
