//! Protokoll und Fortschritt für die Weboberfläche.
//!
//! Alle Log-Meldungen landen im Ringpuffer, den die Oberfläche unter „Protokoll“ zeigt, und
//! zusätzlich auf stderr (Docker-Log). Laufende Scans melden ihren Fortschritt hier.

use crate::model::{Offer, Stats, StoreState};
use crate::util::now_iso;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const LOG_LIMIT: usize = 500;

#[derive(Serialize, Clone, Debug)]
pub struct LogLine {
    pub id: u64,
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

static LINES: Mutex<VecDeque<LogLine>> = Mutex::new(VecDeque::new());
static SEQ: AtomicU64 = AtomicU64::new(1);
static PROGRESS: OnceLock<Mutex<HashMap<String, Map<String, Value>>>> = OnceLock::new();
static MIN_LEVEL: OnceLock<Level> = OnceLock::new();

fn progress_map() -> &'static Mutex<HashMap<String, Map<String, Value>>> {
    PROGRESS.get_or_init(Default::default)
}

fn min_level() -> Level {
    *MIN_LEVEL.get_or_init(|| match std::env::var("LOG_LEVEL").unwrap_or_default().to_uppercase().as_str() {
        "DEBUG" => Level::Debug,
        "WARNING" | "WARN" => Level::Warning,
        "ERROR" => Level::Error,
        _ => Level::Info,
    })
}

pub fn log(level: Level, message: String) {
    if level < min_level() {
        return;
    }
    let time = now_iso();
    eprintln!("{time} {} {message}", level.name());
    // Die Oberfläche zeigt erst ab INFO
    if level < Level::Info {
        return;
    }
    let line = LogLine { id: SEQ.fetch_add(1, Ordering::SeqCst), time, level: level.name(), message };
    let mut lines = LINES.lock().unwrap();
    if lines.len() >= LOG_LIMIT {
        lines.pop_front();
    }
    lines.push_back(line);
}

#[macro_export]
macro_rules! info { ($($arg:tt)*) => { $crate::activity::log($crate::activity::Level::Info, format!($($arg)*)) }; }
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { $crate::activity::log($crate::activity::Level::Warning, format!($($arg)*)) }; }
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { $crate::activity::log($crate::activity::Level::Error, format!($($arg)*)) }; }

pub fn recent(after: u64, limit: usize) -> Vec<LogLine> {
    let lines = LINES.lock().unwrap();
    let selected: Vec<LogLine> = lines.iter().filter(|l| l.id > after).cloned().collect();
    let skip = selected.len().saturating_sub(limit);
    selected.into_iter().skip(skip).collect()
}

pub fn set_progress(shop: &str, values: Value) {
    let mut map = progress_map().lock().unwrap();
    let entry = map.entry(shop.to_string()).or_default();
    if let Value::Object(values) = values {
        entry.extend(values);
    }
}

pub fn clear_progress(shop: &str) {
    progress_map().lock().unwrap().remove(shop);
}

pub fn progress(shop: &str) -> Option<Value> {
    progress_map().lock().unwrap().get(shop).map(|m| Value::Object(m.clone()))
}

type SaveFn = Box<dyn Fn(&StoreState) + Send + Sync>;

/// Fortschrittsmeldungen eines Scans für ein Gerät: protokolliert alle `every` Filialen
/// bzw. spätestens alle `interval` Sekunden und sichert dabei den Zwischenstand.
pub struct ScanReporter {
    shop: String,
    shop_label: String,
    watch_name: String,
    on_save: Option<SaveFn>,
    every: usize,
    interval: f64,
    last_log: Instant,
    last_count: usize,
}

impl ScanReporter {
    pub fn new(shop: &str, shop_label: &str, watch_name: &str, on_save: Option<SaveFn>) -> Self {
        ScanReporter {
            shop: shop.into(),
            shop_label: shop_label.into(),
            watch_name: watch_name.into(),
            on_save,
            every: 25,
            interval: 60.0,
            last_log: Instant::now(),
            last_count: 0,
        }
    }

    pub fn with_cadence(mut self, every: usize, interval: f64) -> Self {
        self.every = every;
        self.interval = interval;
        self
    }

    pub fn report(&mut self, stats: &Stats, state: &StoreState, best: Option<&Offer>) {
        let done = stats.checked + stats.failed;
        set_progress(
            &self.shop,
            serde_json::json!({
                "done": done, "planned": stats.planned, "offers": stats.offers,
                "best_price": best.map(|b| b.total_price),
                "best_store": best.and_then(|b| b.store_name.clone()),
                "throttled": stats.aborted,
            }),
        );
        let due = done - self.last_count >= self.every || self.last_log.elapsed().as_secs_f64() >= self.interval;
        if !due && done < stats.planned {
            return;
        }
        self.last_log = Instant::now();
        self.last_count = done;
        let best_text = best
            .map(|b| {
                format!(", bisher günstigster {} ({})", crate::util::format_price(b.total_price), b.store_name.clone().unwrap_or_default())
            })
            .unwrap_or_default();
        crate::info!(
            "{} · {}: {}/{} Filialen geprüft, {} mit Angebot{}",
            self.shop_label, self.watch_name, done, stats.planned, stats.offers, best_text
        );
        if let Some(save) = &self.on_save {
            save(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reporter_logs_progress_and_saves() {
        let saved = Arc::new(Mutex::new(0));
        let counter = saved.clone();
        let mut reporter = ScanReporter::new("testshop", "TestShop", "Gerät", Some(Box::new(move |_| *counter.lock().unwrap() += 1)))
            .with_cadence(2, 3600.0);
        let state = StoreState::default();
        let mut stats = Stats { planned: 5, ..Default::default() };
        stats.checked = 1;
        reporter.report(&stats, &state, None);
        assert_eq!(*saved.lock().unwrap(), 0);
        stats.checked = 2;
        let best = Offer { total_price: 99.0, store_name: Some("Hamburg".into()), ..Default::default() };
        reporter.report(&stats, &state, Some(&best));
        assert_eq!(*saved.lock().unwrap(), 1);
        let progress = progress("testshop").unwrap();
        assert_eq!(progress["done"], 2);
        assert_eq!(progress["best_store"], "Hamburg");
        assert!(recent(0, 500).iter().any(|l| l.message.contains("2/5 Filialen") && l.message.contains("99,00 €")));
        clear_progress("testshop");
        assert!(super::progress("testshop").is_none());
    }
}
