//! Persistente Ablage von Einstellungen und Watchlist als JSON-Datei.
//!
//! Der Zustand liegt im Speicher und wird bei jeder Änderung atomar nach `state.json`
//! geschrieben. Dateien der Python-Version (Shop-Felder flach am Gerät) werden beim Laden
//! automatisch ins neue Format mit `sources` überführt.

use crate::model::{HistoryEntry, ShopResult, StoreState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const HISTORY_LIMIT: usize = 100;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub ntfy_server: String,
    pub ntfy_topic: String,
    pub ntfy_token: String,
    pub ntfy_priority: i64,
    pub interval_minutes: i64,
    pub jitter_minutes: i64,
    pub plz: String,
    pub scheduler_enabled: bool,
    // Abfragetempo pro Shop
    pub expert_rate_per_min: i64,
    pub expert_workers: i64,
    /// 0 = alle Filialen in jedem Zyklus
    pub expert_stores_per_cycle: i64,
    /// gilt je Domain (euronics.de, mediaathome.de)
    pub euronics_rate_per_min: i64,
    pub euronics_stores_per_cycle: i64,
    /// alle Märkte aus dem Händlerverzeichnis
    pub euronics_use_directory: bool,
    /// zusätzliche Markt-URLs, eine pro Zeile
    pub euronics_stores: String,
    /// Amazon blockt schnell – bewusst langsam
    pub amazon_rate_per_min: i64,
    /// idealo (Preisvergleich) – eine Anfrage pro Prüfung
    pub idealo_rate_per_min: i64,
    /// ältere Filialpreise zählen nicht mehr für den Dealalarm
    pub price_max_age_hours: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ntfy_server: "https://ntfy.sh".into(),
            ntfy_topic: String::new(),
            ntfy_token: String::new(),
            ntfy_priority: 4,
            interval_minutes: 120,
            jitter_minutes: 10,
            plz: String::new(),
            scheduler_enabled: true,
            expert_rate_per_min: 300,
            expert_workers: 8,
            expert_stores_per_cycle: 0,
            euronics_rate_per_min: 6,
            euronics_stores_per_cycle: 0,
            euronics_use_directory: true,
            euronics_stores: String::new(),
            amazon_rate_per_min: 4,
            idealo_rate_per_min: 10,
            price_max_age_hours: 24,
        }
    }
}

/// Einstellungen einer Quelle (Shop) für ein Gerät. `options` sind quellenspezifisch.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SourceConfig {
    pub enabled: bool,
    pub query: String,
    pub options: Map<String, Value>,
}

impl SourceConfig {
    pub fn opt_bool(&self, key: &str, default: bool) -> bool {
        self.options.get(key).and_then(Value::as_bool).unwrap_or(default)
    }
    pub fn opt_f64(&self, key: &str, default: f64) -> f64 {
        self.options.get(key).and_then(Value::as_f64).unwrap_or(default)
    }
    pub fn opt_str<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.options.get(key).and_then(Value::as_str).unwrap_or(default)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Watch {
    pub id: String,
    pub name: String,
    pub target_price: f64,
    pub enabled: bool,
    pub ean: String,
    pub sources: BTreeMap<String, SourceConfig>,
    pub results: BTreeMap<String, ShopResult>,
    pub store_prices: BTreeMap<String, StoreState>,
    pub history: Vec<HistoryEntry>,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            id: String::new(),
            name: String::new(),
            target_price: 0.0,
            enabled: true,
            ean: String::new(),
            sources: BTreeMap::new(),
            results: BTreeMap::new(),
            store_prices: BTreeMap::new(),
            history: Vec::new(),
        }
    }
}

impl Watch {
    pub fn source(&self, id: &str) -> SourceConfig {
        self.sources.get(id).cloned().unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SchedulerState {
    pub last_start: Option<String>,
    pub last_end: Option<String>,
    pub next_run: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct State {
    pub settings: Settings,
    pub watches: Vec<Watch>,
    pub scheduler: SchedulerState,
}

/// Überführt das Format der Python-Version (flache Shop-Felder, `_`-Metadaten im Filialstatus).
pub fn migrate(mut state: Value) -> Value {
    let Some(watches) = state.get_mut("watches").and_then(Value::as_array_mut) else {
        return state;
    };
    for watch in watches {
        let Some(obj) = watch.as_object_mut() else { continue };
        if !obj.contains_key("sources") {
            let take = |obj: &Map<String, Value>, key: &str, default: Value| obj.get(key).cloned().unwrap_or(default);
            let mut sources = Map::new();
            sources.insert(
                "expert".into(),
                json!({
                    "enabled": take(obj, "expert_enabled", json!(true)),
                    "query": take(obj, "expert_query", json!("")),
                    "options": {
                        "all_branches": take(obj, "expert_all_branches", json!(true)),
                        "include_display": take(obj, "expert_include_display", json!(false)),
                        "include_local": take(obj, "expert_include_local", json!(false)),
                        "max_distance": take(obj, "expert_max_distance", json!(50)),
                    }
                }),
            );
            sources.insert(
                "euronics".into(),
                json!({
                    "enabled": take(obj, "euronics_enabled", json!(true)),
                    "query": take(obj, "euronics_query", json!("")),
                    "options": {
                        "all_stores": take(obj, "euronics_all_stores", json!(true)),
                        "max_distance": take(obj, "euronics_max_distance", json!(0)),
                    }
                }),
            );
            for key in [
                "expert_enabled", "expert_query", "expert_all_branches", "expert_include_display", "expert_include_local",
                "expert_max_distance", "euronics_enabled", "euronics_query", "euronics_all_stores", "euronics_max_distance",
            ] {
                obj.remove(key);
            }
            obj.insert("sources".into(), Value::Object(sources));
        }
        // idealo kam später dazu: bei bestehenden Geräten einmalig einschalten (danach zählt die Auswahl des Nutzers)
        if let Some(sources) = obj.get_mut("sources").and_then(Value::as_object_mut) {
            sources.entry("idealo").or_insert_with(|| json!({"enabled": true, "query": "", "options": {}}));
        }
        if let Some(prices) = obj.get_mut("store_prices").and_then(Value::as_object_mut) {
            for state in prices.values_mut() {
                let Some(old) = state.as_object() else { continue };
                if old.contains_key("stores") && old.contains_key("meta") {
                    continue;
                }
                let (mut meta, mut stores) = (Map::new(), Map::new());
                for (key, value) in old {
                    match key.strip_prefix('_') {
                        Some(name) => meta.insert(name.to_string(), value.clone()),
                        None => stores.insert(key.clone(), value.clone()),
                    };
                }
                *state = json!({"meta": meta, "stores": stores});
            }
        }
    }
    state
}

pub struct Storage {
    path: PathBuf,
    state: Mutex<State>,
}

impl Storage {
    /// Lädt `<dir>/state.json` (oder startet leer).
    pub fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join("state.json");
        let state = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let value: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            serde_json::from_value(migrate(value)).map_err(|e| format!("{}: {e}", path.display()))?
        } else {
            State::default()
        };
        Ok(Storage { path, state: Mutex::new(state) })
    }

    fn save(&self, state: &State) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        let result = serde_json::to_string_pretty(state)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&tmp, text).map_err(|e| e.to_string()))
            .and_then(|_| std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string()));
        if let Err(e) = result {
            crate::error!("Speichern von {} fehlgeschlagen: {e}", self.path.display());
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.state.lock().unwrap();
        let out = f(&mut state);
        self.save(&state);
        out
    }

    pub fn get_state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn get_settings(&self) -> Settings {
        self.state.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        self.with(|s| s.settings = settings);
    }

    pub fn list_watches(&self) -> Vec<Watch> {
        self.state.lock().unwrap().watches.clone()
    }

    pub fn get_watch(&self, id: &str) -> Option<Watch> {
        self.state.lock().unwrap().watches.iter().find(|w| w.id == id).cloned()
    }

    pub fn add_watch(&self, mut watch: Watch) -> Watch {
        watch.id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        watch.results.clear();
        watch.store_prices.clear();
        watch.history.clear();
        self.with(|s| s.watches.push(watch.clone()));
        watch
    }

    /// Ändert ein Gerät über eine Funktion; `None`, wenn es das Gerät nicht gibt.
    pub fn update_watch(&self, id: &str, f: impl FnOnce(&mut Watch)) -> Option<Watch> {
        self.with(|s| {
            let watch = s.watches.iter_mut().find(|w| w.id == id)?;
            f(watch);
            Some(watch.clone())
        })
    }

    pub fn delete_watch(&self, id: &str) -> bool {
        self.with(|s| {
            let before = s.watches.len();
            s.watches.retain(|w| w.id != id);
            s.watches.len() < before
        })
    }

    /// Speichert das Prüfergebnis eines Shops. Liefert das bisherige Ergebnis zurück.
    pub fn save_result(
        &self,
        id: &str,
        shop: &str,
        result: ShopResult,
        history: Option<HistoryEntry>,
        store_state: Option<StoreState>,
    ) -> Option<ShopResult> {
        self.with(|s| {
            let watch = s.watches.iter_mut().find(|w| w.id == id)?;
            let previous = watch.results.insert(shop.to_string(), result);
            if let Some(state) = store_state {
                watch.store_prices.insert(shop.to_string(), state);
            }
            if let Some(entry) = history {
                watch.history.push(entry);
                let excess = watch.history.len().saturating_sub(HISTORY_LIMIT);
                watch.history.drain(..excess);
            }
            previous
        })
    }

    /// Zwischenstand eines laufenden Scans speichern (Filialpreise).
    pub fn save_store_state(&self, id: &str, shop: &str, state: &StoreState) {
        self.with(|s| {
            if let Some(watch) = s.watches.iter_mut().find(|w| w.id == id) {
                watch.store_prices.insert(shop.to_string(), state.clone());
            }
        });
    }

    pub fn set_ean(&self, id: &str, ean: &str) {
        self.update_watch(id, |w| w.ean = ean.to_string());
    }

    pub fn update_scheduler(&self, f: impl FnOnce(&mut SchedulerState)) {
        self.with(|s| f(&mut s.scheduler));
    }

    pub fn get_scheduler(&self) -> SchedulerState {
        self.state.lock().unwrap().scheduler.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enables_idealo_once_for_existing_watches() {
        let old = json!({"watches": [
            {"id": "a", "name": "A", "sources": {"expert": {"enabled": true}}},
            {"id": "b", "name": "B", "sources": {"idealo": {"enabled": false}}},
        ]});
        let state: State = serde_json::from_value(migrate(old)).unwrap();
        assert!(state.watches[0].source("idealo").enabled);
        assert!(state.watches[0].source("expert").enabled);
        // Wer idealo bewusst ausgeschaltet hat, behält das
        assert!(!state.watches[1].source("idealo").enabled);
    }

    #[test]
    fn migrates_python_state() {
        let old = json!({
            "settings": {"ntfy_topic": "abc", "interval_minutes": 90},
            "watches": [{
                "id": "w1", "name": "AirPods", "target_price": 199.0, "enabled": true, "ean": "0195950543742",
                "expert_enabled": true, "expert_query": "https://www.expert.de/x.html", "expert_all_branches": false,
                "expert_include_display": true, "expert_include_local": false, "expert_max_distance": 30,
                "euronics_enabled": false, "euronics_query": "", "euronics_all_stores": true, "euronics_max_distance": 0,
                "results": {"expert": {"title": "AirPods", "price": 189.0, "available": true, "detail": "x", "checked_at": "2026-01-01T00:00:00+01:00"}},
                "store_prices": {"expert": {
                    "_webcodes": ["123"], "_product_url": "https://www.expert.de/x.html",
                    "e_1": {"name": "expert A", "checked": "2026-01-01T00:00:00+01:00", "coordinates": [1.0, 2.0],
                            "offer": {"price": 189.0, "shipping": 0.0, "total_price": 189.0, "online_stock": 1, "store_stock": 0, "on_display": false}}
                }},
                "history": [{"time": "2026-01-01T00:00:00+01:00", "shop": "expert", "price": 189.0}]
            }],
            "scheduler": {"next_run": "2026-01-01T02:00:00+01:00"}
        });
        let state: State = serde_json::from_value(migrate(old)).unwrap();
        assert_eq!(state.settings.ntfy_topic, "abc");
        assert_eq!(state.settings.interval_minutes, 90);
        assert_eq!(state.settings.amazon_rate_per_min, 4);
        let watch = &state.watches[0];
        let expert = watch.source("expert");
        assert!(expert.enabled && expert.query.contains("expert.de"));
        assert!(!expert.opt_bool("all_branches", true) && expert.opt_bool("include_display", false));
        assert_eq!(expert.opt_f64("max_distance", 0.0), 30.0);
        assert!(!watch.source("euronics").enabled);
        assert!(!watch.source("amazon").enabled);
        assert!(watch.source("idealo").enabled);
        let prices = &watch.store_prices["expert"];
        assert_eq!(prices.meta_str("product_url"), Some("https://www.expert.de/x.html"));
        assert_eq!(prices.meta["webcodes"][0], "123");
        assert_eq!(prices.stores["e_1"].offer.as_ref().unwrap().total_price, 189.0);
        assert_eq!(watch.results["expert"].price, Some(189.0));
        assert_eq!(state.scheduler.next_run.as_deref(), Some("2026-01-01T02:00:00+01:00"));
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let mut watch = Watch { name: "TV".into(), target_price: 500.0, ..Default::default() };
        watch.sources.insert("amazon".into(), SourceConfig { enabled: true, query: "B0TEST1234".into(), ..Default::default() });
        let watch = storage.add_watch(watch);
        assert_eq!(watch.id.len(), 12);
        storage.save_result(&watch.id, "amazon", ShopResult { price: Some(450.0), ..Default::default() },
            Some(HistoryEntry { time: "t".into(), shop: "amazon".into(), price: 450.0 }), None);
        let reloaded = Storage::open(dir.path()).unwrap();
        let again = reloaded.get_watch(&watch.id).unwrap();
        assert_eq!(again.results["amazon"].price, Some(450.0));
        assert_eq!(again.history.len(), 1);
        assert_eq!(again.source("amazon").query, "B0TEST1234");
        assert!(reloaded.delete_watch(&watch.id));
        assert!(!reloaded.delete_watch(&watch.id));
    }

    #[test]
    fn history_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let watch = storage.add_watch(Watch::default());
        for i in 0..(HISTORY_LIMIT + 20) {
            storage.save_result(&watch.id, "x", ShopResult::default(),
                Some(HistoryEntry { time: i.to_string(), shop: "x".into(), price: i as f64 }), None);
        }
        let history = storage.get_watch(&watch.id).unwrap().history;
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.last().unwrap().time, (HISTORY_LIMIT + 19).to_string());
    }
}
