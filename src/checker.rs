//! Prüft alle Watches, vergleicht mit dem Wunschpreis und löst Dealalarme aus.

use crate::activity::{self, ScanReporter};
use crate::model::{CheckError, HistoryEntry, ShopResult, StoreState};
use crate::notifier;
use crate::shops::{self, Source};
use crate::storage::{Settings, Storage, Watch};
use crate::util::{format_price, now_iso};
use rand::RngExt;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Ermittelt, ob ein Alarm fällig ist, und ergänzt den Alarmstatus im Ergebnis.
pub fn evaluate(watch: &Watch, result: &mut ShopResult, previous: Option<&ShopResult>) -> bool {
    let default = ShopResult::default();
    let previous = previous.unwrap_or(&default);
    if result.error.is_some() {
        // Fehler setzen den Alarmstatus nicht zurück, sonst käme nach jedem Aussetzer ein neuer Alarm
        result.alerted = previous.alerted;
        result.alert_price = previous.alert_price;
        return false;
    }
    let target = watch.target_price;
    let is_deal = matches!(result.price, Some(price) if target > 0.0 && price <= target);
    result.is_deal = is_deal;
    if !is_deal {
        result.alerted = false;
        result.alert_price = None;
        return false;
    }
    let price = result.price.unwrap();
    let last_alert = if previous.alerted { previous.alert_price } else { None };
    let should_alert = last_alert.is_none_or(|last| price < last - 0.005);
    result.alerted = true;
    result.alert_price = Some(if should_alert { price } else { last_alert.unwrap() });
    should_alert
}

async fn notify_deal(
    client: &reqwest::Client,
    settings: &Settings,
    watch: &Watch,
    label: &str,
    result: &ShopResult,
) -> Result<String, String> {
    let name = if !watch.name.is_empty() { watch.name.as_str() } else if !result.title.is_empty() { result.title.as_str() } else { "Gerät" };
    let title = if result.title.is_empty() { name } else { result.title.as_str() };
    let message = format!(
        "{title}\nPreis: {} (Wunschpreis {})\n{}",
        format_price(result.price.unwrap_or(0.0)),
        format_price(watch.target_price),
        result.detail
    );
    notifier::send(
        client,
        settings,
        notifier::Message {
            title: &format!("Dealalarm: {name} bei {label}"),
            message: message.trim(),
            click: Some(&result.url),
            tags: &["rotating_light", "moneybag"],
        },
    )
    .await
}

/// Prüft ein Gerät bei einer Quelle, speichert das Ergebnis und löst ggf. den Dealalarm aus.
pub async fn check_shop(
    storage: &Arc<Storage>,
    client: &reqwest::Client,
    source: &Arc<dyn Source>,
    mut watch: Watch,
    settings: &Settings,
) -> ShopResult {
    let (shop, label) = (source.id(), source.label());
    let name = if watch.name.is_empty() { "Gerät".to_string() } else { watch.name.clone() };
    let previous = watch.results.get(shop).cloned();
    let store_state = watch.store_prices.get(shop).cloned().unwrap_or_default();
    let mut reporter = {
        let (storage, id) = (storage.clone(), watch.id.clone());
        ScanReporter::new(shop, label, &name, Some(Box::new(move |state| storage.save_store_state(&id, shop, state))))
    };
    let started = Instant::now();
    crate::info!("{label} · {name}: Prüfung gestartet");

    let mut new_state: Option<StoreState> = None;
    let mut result = match source.check(&watch, settings, store_state, &mut reporter).await {
        Ok((result, state)) => {
            new_state = Some(state);
            result
        }
        Err(CheckError::Lookup { message, suggestion }) => {
            // Produkt nicht (eindeutig) gefunden: alte Filialpreise gehören evtl. zu einem anderen Produkt
            crate::warn!("{label} · {name}: {message}");
            new_state = Some(StoreState::default());
            ShopResult { suggestion, ..ShopResult::error(message) }
        }
        Err(error) => {
            // einzelne Shopfehler sollen den Lauf nicht abbrechen; bisher Gesammeltes bleibt erhalten
            crate::warn!("{label} · {name}: Fehler – {error}");
            ShopResult::error(error.message())
        }
    };
    result.checked_at = now_iso();

    if !result.ean.is_empty() && watch.ean.is_empty() {
        watch.ean = result.ean.clone();
        storage.set_ean(&watch.id, &result.ean);
    }

    if evaluate(&watch, &mut result, previous.as_ref()) {
        match notify_deal(client, settings, &watch, label, &result).await {
            Ok(message_id) => {
                result.notified_at = Some(result.checked_at.clone());
                crate::info!("{label} · {name}: Dealalarm gesendet ({:.2} €, ntfy-ID {message_id})", result.price.unwrap_or(0.0));
            }
            Err(error) => {
                crate::error!("{label} · {name}: ntfy-Versand fehlgeschlagen – {error}");
                result.notify_error = Some(error);
                // Beim nächsten Lauf erneut versuchen
                result.alerted = false;
                result.alert_price = None;
            }
        }
    }
    let history = result.price.map(|price| HistoryEntry { time: result.checked_at.clone(), shop: shop.to_string(), price });
    storage.save_result(&watch.id, shop, result.clone(), history, new_state);

    if result.error.is_none() {
        let minutes = started.elapsed().as_secs_f64() / 60.0;
        let price = result.price.map_or("kein Angebot".to_string(), |p| format!("{p:.2} €"));
        let throttled = result.scan.as_ref().is_some_and(|s| s.aborted);
        crate::info!(
            "{label} · {name}: fertig nach {minutes:.1} Min. – {price} ({}){}",
            result.detail,
            if throttled { " · gedrosselt, Rest folgt im nächsten Lauf" } else { "" }
        );
    }
    result
}

#[derive(Default)]
struct WorkerState {
    queue: VecDeque<String>,
    current: Option<String>,
}

/// Warteschlange einer Quelle – unabhängig von den anderen Quellen.
///
/// So muss expert nicht warten, bis der langsame Euronics-Scan fertig ist, und
/// ein neu angelegtes Gerät wird sofort bei jeder Quelle eingereiht.
struct Worker {
    state: Mutex<WorkerState>,
    wake: Notify,
}

impl Worker {
    fn enqueue(&self, watch_id: &str, front: bool) {
        let mut s = self.state.lock().unwrap();
        if s.current.as_deref() == Some(watch_id) {
            return;
        }
        if let Some(pos) = s.queue.iter().position(|id| id == watch_id) {
            if front && pos > 0 {
                let id = s.queue.remove(pos).unwrap();
                s.queue.push_front(id);
            }
            return;
        }
        if front {
            s.queue.push_front(watch_id.to_string());
        } else {
            s.queue.push_back(watch_id.to_string());
        }
        drop(s);
        self.wake.notify_one();
    }

    fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.current.is_some() || !s.queue.is_empty()
    }
}

pub struct Engine {
    storage: Arc<Storage>,
    client: reqwest::Client,
    sources: Vec<Arc<dyn Source>>,
    workers: HashMap<&'static str, Arc<Worker>>,
}

impl Engine {
    /// Engine mit allen registrierten Quellen.
    pub fn new(storage: Arc<Storage>) -> Arc<Self> {
        Self::with_sources(storage, shops::all().to_vec())
    }

    pub fn with_sources(storage: Arc<Storage>, sources: Vec<Arc<dyn Source>>) -> Arc<Self> {
        let workers = sources
            .iter()
            .map(|s| (s.id(), Arc::new(Worker { state: Default::default(), wake: Notify::new() })))
            .collect();
        Arc::new(Engine { storage, client: reqwest::Client::new(), sources, workers })
    }

    /// Startet je Quelle einen Arbeiter (Hintergrundaufgabe).
    pub fn start_workers(self: &Arc<Self>) {
        for source in &self.sources {
            let (engine, source) = (self.clone(), source.clone());
            let worker = self.workers[source.id()].clone();
            tokio::spawn(async move {
                loop {
                    let id = loop {
                        // Erst eintragen, dann prüfen – so gibt es keinen Moment, in dem das Gerät weder wartet noch läuft
                        let next = {
                            let mut s = worker.state.lock().unwrap();
                            let next = s.queue.pop_front();
                            s.current = next.clone();
                            next
                        };
                        match next {
                            Some(id) => break id,
                            None => worker.wake.notified().await,
                        }
                    };
                    let run = {
                        let (engine, source) = (engine.clone(), source.clone());
                        let id = id.clone();
                        tokio::spawn(async move { engine.run_one(&source, &id).await })
                    };
                    if let Err(error) = run.await {
                        crate::error!("{}: Prüfung abgebrochen ({error})", source.label());
                    }
                    activity::clear_progress(source.id());
                    worker.state.lock().unwrap().current = None;
                    engine.finish_if_idle();
                }
            });
        }
    }

    async fn run_one(&self, source: &Arc<dyn Source>, id: &str) {
        let Some(watch) = self.storage.get_watch(id) else { return };
        if !source.is_active(&watch) {
            return;
        }
        activity::set_progress(
            source.id(),
            json!({"watch_id": watch.id, "watch_name": watch.name, "started": now_iso(), "done": 0, "planned": Value::Null}),
        );
        let settings = self.storage.get_settings();
        check_shop(&self.storage, &self.client, source, watch, &settings).await;
    }

    fn finish_if_idle(&self) {
        if !self.is_running() {
            self.storage.update_scheduler(|s| s.last_end = Some(now_iso()));
            crate::info!("Alle Prüfungen abgeschlossen");
        }
    }

    /// Reiht alle aktiven Geräte (oder ein bestimmtes, dann vorne) bei jeder Quelle ein.
    pub fn run_all(&self, watch_id: Option<&str>) -> bool {
        let watches: Vec<Watch> = self
            .storage
            .list_watches()
            .into_iter()
            .filter(|w| watch_id.map_or(w.enabled, |id| w.id == id))
            .collect();
        if watches.is_empty() {
            return false;
        }
        self.storage.update_scheduler(|s| s.last_start = Some(now_iso()));
        for source in &self.sources {
            for watch in &watches {
                self.workers[source.id()].enqueue(&watch.id, watch_id.is_some());
            }
        }
        crate::info!(
            "Prüfung eingereiht: {}",
            watches.iter().map(|w| if w.name.is_empty() { w.id.as_str() } else { w.name.as_str() }).collect::<Vec<_>>().join(", ")
        );
        true
    }

    pub fn is_running(&self) -> bool {
        self.workers.values().any(|w| w.busy())
    }

    /// Was gerade geprüft wird und was wartet – pro Quelle.
    pub fn status(&self) -> Value {
        let mut out = serde_json::Map::new();
        for source in &self.sources {
            let s = self.workers[source.id()].state.lock().unwrap();
            out.insert(
                source.id().to_string(),
                json!({"label": source.label(), "current": s.current, "queue": s.queue,
                       "progress": activity::progress(source.id())}),
            );
        }
        Value::Object(out)
    }
}

/// Startet die Prüfung zyklisch (Standard: alle 120 Minuten ± Zufallsanteil).
pub struct Scheduler {
    wake: Notify,
}

impl Scheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Scheduler { wake: Notify::new() })
    }

    /// Nach Änderung der Einstellungen den nächsten Termin neu berechnen.
    pub fn reschedule(&self) {
        self.wake.notify_one();
    }

    pub fn next_delay(settings: &Settings) -> f64 {
        let interval = (settings.interval_minutes as f64).max(15.0);
        let jitter = (settings.jitter_minutes as f64).max(0.0).min(interval / 2.0);
        let offset = if jitter > 0.0 { rand::rng().random_range(-jitter..=jitter) } else { 0.0 };
        (interval + offset) * 60.0
    }

    fn set_deadline(storage: &Storage, base: f64) -> f64 {
        let settings = storage.get_settings();
        let deadline = base + Self::next_delay(&settings);
        let next_run = crate::util::iso_from_epoch(deadline);
        storage.update_scheduler(|s| s.next_run = Some(next_run.clone()));
        if settings.scheduler_enabled {
            crate::info!("Nächster automatischer Lauf: {next_run}");
        }
        deadline
    }

    pub fn start(self: &Arc<Self>, engine: Arc<Engine>, storage: Arc<Storage>) {
        let this = self.clone();
        tokio::spawn(async move {
            let epoch = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
            let mut last_run: Option<f64> = None;
            let mut deadline = epoch() + 30.0; // kurz nach dem Start einmal prüfen
            loop {
                let remaining = deadline - epoch();
                if remaining > 0.0 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs_f64(remaining)) => {}
                        _ = this.wake.notified() => {
                            if let Some(last) = last_run {
                                deadline = Self::set_deadline(&storage, last);
                            }
                        }
                    }
                    continue;
                }
                last_run = Some(epoch());
                if storage.get_settings().scheduler_enabled {
                    engine.run_all(None);
                }
                deadline = Self::set_deadline(&storage, last_run.unwrap());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Offer, Stats};
    use crate::shops::OptionDef;
    use crate::storage::SourceConfig;
    use async_trait::async_trait;

    fn watch(target: f64) -> Watch {
        Watch { target_price: target, ..Default::default() }
    }

    fn priced(price: Option<f64>) -> ShopResult {
        ShopResult { price, available: price.is_some(), ..Default::default() }
    }

    #[test]
    fn alarm_only_once_until_price_drops_or_resets() {
        let w = watch(100.0);
        let mut first = priced(Some(95.0));
        assert!(evaluate(&w, &mut first, None));
        let mut same = priced(Some(95.0));
        assert!(!evaluate(&w, &mut same, Some(&first)));
        assert!(same.alerted && same.alert_price == Some(95.0));
        let mut cheaper = priced(Some(90.0));
        assert!(evaluate(&w, &mut cheaper, Some(&same)));
        let mut slightly_higher = priced(Some(97.0));
        assert!(!evaluate(&w, &mut slightly_higher, Some(&cheaper)));
        assert_eq!(slightly_higher.alert_price, Some(90.0));
        let mut over = priced(Some(130.0));
        assert!(!evaluate(&w, &mut over, Some(&cheaper)));
        assert!(!over.alerted && !over.is_deal);
        let mut again = priced(Some(99.0));
        assert!(evaluate(&w, &mut again, Some(&over)));
    }

    #[test]
    fn errors_keep_alarm_state_and_unavailable_is_no_deal() {
        let w = watch(100.0);
        let previous = ShopResult { alerted: true, alert_price: Some(80.0), ..Default::default() };
        let mut error = ShopResult::error("kaputt");
        assert!(!evaluate(&w, &mut error, Some(&previous)));
        assert!(error.alerted && error.alert_price == Some(80.0));
        let mut none = priced(None);
        assert!(!evaluate(&w, &mut none, None));
        let mut no_target = priced(Some(1.0));
        assert!(!evaluate(&watch(0.0), &mut no_target, None));
    }

    #[test]
    fn schedule_respects_minimum_and_jitter() {
        let mut settings = Settings { interval_minutes: 5, jitter_minutes: 0, ..Default::default() };
        assert_eq!(Scheduler::next_delay(&settings), 15.0 * 60.0);
        settings.interval_minutes = 120;
        settings.jitter_minutes = 10;
        for _ in 0..50 {
            let d = Scheduler::next_delay(&settings) / 60.0;
            assert!((110.0..=130.0).contains(&d), "{d}");
        }
        settings.jitter_minutes = 500; // höchstens die Hälfte des Intervalls
        for _ in 0..50 {
            let d = Scheduler::next_delay(&settings) / 60.0;
            assert!((60.0..=180.0).contains(&d), "{d}");
        }
    }

    /// Eine Quelle, die nach Belieben antwortet (nur für Tests).
    struct Fake {
        id: &'static str,
        delay_ms: u64,
        price: Option<f64>,
    }

    #[async_trait]
    impl Source for Fake {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            "Fake"
        }
        fn query_label(&self) -> &'static str {
            ""
        }
        fn query_placeholder(&self) -> &'static str {
            ""
        }
        fn options(&self) -> Vec<OptionDef> {
            vec![]
        }
        async fn check(&self, _w: &Watch, _s: &Settings, _st: StoreState, progress: &mut ScanReporter) -> Result<(ShopResult, StoreState), CheckError> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            let stats = Stats { planned: 1, checked: 1, ..Default::default() };
            progress.report(&stats, &StoreState::default(), None);
            Ok((ShopResult { price: self.price, available: self.price.is_some(), url: "https://x".into(), detail: "detail".into(), ..Default::default() }, StoreState::default()))
        }
        fn filter_offers(&self, offers: Vec<Offer>, _: &Watch, _: &Settings, _: Option<crate::model::Coords>) -> Vec<Offer> {
            offers
        }
    }

    #[tokio::test]
    async fn check_shop_saves_result_history_and_reports_failed_ntfy() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).unwrap());
        let mut w = Watch { name: "Gerät".into(), target_price: 100.0, ..Default::default() };
        w.sources.insert("fake".into(), SourceConfig { enabled: true, query: "x".into(), ..Default::default() });
        let w = storage.add_watch(w);
        let source: Arc<dyn Source> = Arc::new(Fake { id: "fake", delay_ms: 0, price: Some(80.0) });
        // Kein ntfy-Topic konfiguriert: Alarm schlägt fehl, wird aber gemeldet und beim nächsten Lauf wiederholt
        let result = check_shop(&storage, &reqwest::Client::new(), &source, w.clone(), &storage.get_settings()).await;
        assert!(result.is_deal && result.notify_error.is_some() && !result.alerted);
        let saved = storage.get_watch(&w.id).unwrap();
        assert_eq!(saved.results["fake"].price, Some(80.0));
        assert_eq!(saved.history.len(), 1);
        assert!(!saved.results["fake"].checked_at.is_empty());
    }

    #[tokio::test]
    async fn sources_run_independently() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).unwrap());
        let mut w = Watch { name: "Gerät".into(), target_price: 1.0, ..Default::default() };
        for id in ["slow", "quick"] {
            w.sources.insert(id.into(), SourceConfig { enabled: true, query: "x".into(), ..Default::default() });
        }
        let w = storage.add_watch(w);
        let engine = Engine::with_sources(storage.clone(), vec![
            Arc::new(Fake { id: "slow", delay_ms: 600, price: Some(50.0) }),
            Arc::new(Fake { id: "quick", delay_ms: 0, price: Some(60.0) }),
        ]);
        engine.start_workers();
        assert!(engine.run_all(Some(&w.id)));
        assert!(engine.is_running());
        // Die schnelle Quelle ist fertig, während die langsame noch läuft
        tokio::time::sleep(Duration::from_millis(250)).await;
        let saved = storage.get_watch(&w.id).unwrap();
        assert!(saved.results.contains_key("quick") && !saved.results.contains_key("slow"));
        assert_eq!(engine.status()["slow"]["current"], w.id.as_str());
        assert!(engine.is_running());
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(storage.get_watch(&w.id).unwrap().results.contains_key("slow"));
        assert!(!engine.is_running());
        assert!(storage.get_scheduler().last_end.is_some());
        // Unbekanntes oder pausiertes Gerät wird nicht eingereiht
        assert!(!engine.run_all(Some("gibt-es-nicht")));
    }

    #[tokio::test]
    async fn queue_runs_sources_independently_and_deduplicates() {
        let worker = Worker { state: Default::default(), wake: Notify::new() };
        worker.enqueue("a", false);
        worker.enqueue("b", false);
        worker.enqueue("a", false); // doppelt
        worker.enqueue("b", true); // nach vorne
        assert_eq!(worker.state.lock().unwrap().queue, ["b", "a"]);
        assert!(worker.busy());
        worker.state.lock().unwrap().current = Some("b".into());
        worker.enqueue("b", true); // läuft bereits
        assert_eq!(worker.state.lock().unwrap().queue.len(), 2);
    }
}
