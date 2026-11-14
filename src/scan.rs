//! Filial-Scanner: fragt den Preis eines Produkts in jeder Filiale einzeln ab.
//!
//! Damit auch Shops mit strengem Rate-Limit vollständig abgedeckt werden, merkt sich der
//! Scanner pro Gerät und Filiale den letzten Preis. Pro Zyklus gilt ein Budget an Anfragen:
//!
//! 1. Die günstigsten bekannten Angebote (bis zur Hälfte des Budgets) werden jedes Mal neu
//!    geprüft, damit ein Deal nicht auf veralteten Daten beruht.
//! 2. Danach kommen nie geprüfte Filialen,
//! 3. danach die am längsten nicht geprüften.
//!
//! So wandert der Scan über mehrere Zyklen durch alle Filialen, während die interessanten
//! jedes Mal aktuell sind. Bricht ein Scan wegen Drosselung ab, bleibt alles bisher
//! Gesammelte erhalten und der nächste Zyklus macht weiter.

use crate::activity::ScanReporter;
use crate::model::{Coords, Offer, Stats, StoreEntry, StoreState};
use crate::ratelimit::RateLimited;
use crate::util::{age_hours, now_iso};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::future::Future;

/// Eine abzufragende Filiale (shop-spezifische Typen implementieren das).
pub trait Scannable: Clone {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn coordinates(&self) -> Option<Coords>;
}

#[derive(Debug)]
pub enum FetchError {
    RateLimited(String),
    Other(String),
}

impl From<RateLimited> for FetchError {
    fn from(e: RateLimited) -> Self {
        FetchError::RateLimited(e.0)
    }
}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        FetchError::Other(e.to_string())
    }
}

/// Reihenfolge der zu prüfenden Filialen für diesen Zyklus (Indizes in `stores`).
pub fn plan<S: Scannable>(stores: &[S], state: &StoreState, budget: Option<usize>) -> Vec<usize> {
    let budget = budget.filter(|b| *b > 0 && *b < stores.len());
    let mut known: Vec<(f64, usize)> = vec![];
    let mut never: Vec<usize> = vec![];
    let mut rest: Vec<(String, usize)> = vec![];
    for (i, store) in stores.iter().enumerate() {
        match state.stores.get(store.id()) {
            None => never.push(i),
            Some(entry) => match &entry.offer {
                Some(offer) => known.push((offer.total_price, i)),
                None => rest.push((entry.checked.clone(), i)),
            },
        }
    }
    known.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Höchstens die Hälfte des Budgets für die günstigsten bekannten Angebote,
    // der Rest rotiert durch ungeprüfte und am längsten nicht geprüfte Filialen
    let hot_count = match budget {
        None => known.len(),
        Some(b) => known.len().min((b / 2).max(1)),
    };
    let hot: Vec<usize> = known[..hot_count].iter().map(|(_, i)| *i).collect();
    for (_, i) in &known[hot_count..] {
        let checked = state.stores[stores[*i].id()].checked.clone();
        rest.push((checked, *i));
    }
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    let mut order: Vec<usize> = hot;
    order.extend(never);
    order.extend(rest.into_iter().map(|(_, i)| i));
    if let Some(b) = budget {
        order.truncate(b);
    }
    order
}

/// Prüft die geplanten Filialen. `fetch(store)` liefert ein Angebot oder `None` (nicht verfügbar).
///
/// `reporter` wird nach jeder Filiale aufgerufen. `best` ist das günstigste Angebot bisher,
/// das `accept` erfüllt (z. B. die Filter des Geräts). Liefert (neuer Zustand, Statistik).
pub async fn run<S, F, Fut>(
    stores: &[S],
    mut state: StoreState,
    fetch: F,
    budget: Option<usize>,
    workers: usize,
    mut reporter: Option<&mut ScanReporter>,
    accept: impl Fn(&Offer) -> bool,
) -> (StoreState, Stats)
where
    S: Scannable,
    F: Fn(S) -> Fut,
    Fut: Future<Output = Result<Option<Offer>, FetchError>>,
{
    let ids: HashSet<&str> = stores.iter().map(|s| s.id()).collect();
    state.stores.retain(|id, _| ids.contains(id.as_str()));
    let queue = plan(stores, &state, budget);
    let mut stats = Stats { planned: queue.len(), ..Default::default() };
    let mut best: Option<Offer> = None;

    let tasks = queue.into_iter().map(|i| {
        let store = stores[i].clone();
        let future = fetch(store.clone());
        async move { (store, future.await) }
    });
    let mut results = stream::iter(tasks).buffered(workers.max(1));
    while let Some((store, result)) = results.next().await {
        match result {
            Err(FetchError::RateLimited(message)) => {
                if !stats.aborted {
                    crate::warn!("Scan wird gedrosselt und abgebrochen: {message} – Fortsetzung im nächsten Lauf");
                }
                stats.aborted = true;
            }
            Err(FetchError::Other(message)) => {
                stats.failed += 1;
                // Die ersten Fehler sichtbar machen, danach nur noch zählen
                if stats.failed <= 3 {
                    crate::warn!("{}: Abfrage fehlgeschlagen ({message})", store.name());
                }
            }
            Ok(offer) => {
                stats.checked += 1;
                state.stores.insert(
                    store.id().to_string(),
                    StoreEntry {
                        name: store.name().to_string(),
                        checked: now_iso(),
                        coordinates: store.coordinates(),
                        offer: offer.clone(),
                    },
                );
                if let Some(offer) = offer {
                    stats.offers += 1;
                    let candidate = Offer {
                        store_name: Some(store.name().to_string()),
                        coordinates: store.coordinates(),
                        ..offer
                    };
                    if best.as_ref().is_none_or(|b| candidate.total_price < b.total_price) && accept(&candidate) {
                        best = Some(candidate);
                    }
                }
            }
        }
        if let Some(reporter) = reporter.as_deref_mut() {
            reporter.report(&stats, &state, best.as_ref());
        }
    }
    stats.total = stores.len();
    stats.known = state.stores.len();
    (state, stats)
}

/// Alle Angebote, die nicht älter als `max_age_hours` sind, günstigste zuerst.
pub fn fresh_offers(state: &StoreState, max_age_hours: f64) -> Vec<Offer> {
    let mut offers: Vec<Offer> = state
        .stores
        .iter()
        .filter_map(|(id, entry)| {
            let offer = entry.offer.as_ref()?;
            (age_hours(&entry.checked) <= max_age_hours).then(|| Offer {
                store_id: Some(id.clone()),
                store_name: Some(entry.name.clone()),
                checked: Some(entry.checked.clone()),
                coordinates: entry.coordinates,
                ..offer.clone()
            })
        })
        .collect();
    offers.sort_by(|a, b| a.total_price.total_cmp(&b.total_price));
    offers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct Shop(String);
    impl Scannable for Shop {
        fn id(&self) -> &str {
            &self.0
        }
        fn name(&self) -> &str {
            &self.0
        }
        fn coordinates(&self) -> Option<Coords> {
            None
        }
    }

    fn shops(n: usize) -> Vec<Shop> {
        (0..n).map(|i| Shop(format!("s{i}"))).collect()
    }

    fn entry(price: Option<f64>, checked: &str) -> StoreEntry {
        StoreEntry { name: "x".into(), checked: checked.into(), coordinates: None, offer: price.map(Offer::simple) }
    }

    fn names(stores: &[Shop], plan: &[usize]) -> Vec<String> {
        plan.iter().map(|i| stores[*i].0.clone()).collect()
    }

    #[test]
    fn plan_prioritises_known_offers_then_unchecked_then_oldest() {
        let stores = shops(6);
        let mut state = StoreState::default();
        state.stores.insert("s0".into(), entry(Some(300.0), "2026-01-01T00:00:00+00:00"));
        state.stores.insert("s1".into(), entry(Some(100.0), "2026-01-02T00:00:00+00:00"));
        state.stores.insert("s2".into(), entry(None, "2026-01-01T00:00:00+00:00"));
        state.stores.insert("s3".into(), entry(None, "2025-12-01T00:00:00+00:00"));
        // s4, s5 nie geprüft
        assert_eq!(names(&stores, &plan(&stores, &state, None)), ["s1", "s0", "s4", "s5", "s3", "s2"]);
        // Budget 4: höchstens 2 heiße Angebote, dann ungeprüfte
        assert_eq!(names(&stores, &plan(&stores, &state, Some(4))), ["s1", "s0", "s4", "s5"]);
        // Budget 2: nur 1 heißes Angebot, dann ein ungeprüfter
        assert_eq!(names(&stores, &plan(&stores, &state, Some(2))), ["s1", "s4"]);
    }

    #[tokio::test]
    async fn rotates_through_all_stores_with_budget() {
        let stores = shops(10);
        let mut state = StoreState::default();
        let seen = Arc::new(std::sync::Mutex::new(HashSet::new()));
        for _ in 0..4 {
            let seen = seen.clone();
            let (new_state, stats) = run(
                &stores,
                state,
                move |s: Shop| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().insert(s.0.clone());
                        Ok(None)
                    }
                },
                Some(3),
                2,
                None,
                |_| true,
            )
            .await;
            assert_eq!(stats.planned, 3);
            state = new_state;
        }
        assert_eq!(seen.lock().unwrap().len(), 10);
        assert_eq!(state.stores.len(), 10);
    }

    #[tokio::test]
    async fn keeps_partial_results_when_rate_limited() {
        let stores = shops(5);
        let counter = Arc::new(AtomicUsize::new(0));
        let (state, stats) = run(
            &stores,
            StoreState::default(),
            move |s: Shop| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n >= 2 {
                        Err(FetchError::RateLimited("gedrosselt".into()))
                    } else {
                        Ok(Some(Offer::simple(10.0 + s.0.len() as f64)))
                    }
                }
            },
            None,
            1,
            None,
            |_| true,
        )
        .await;
        assert!(stats.aborted);
        assert_eq!(stats.checked, 2);
        assert_eq!(state.stores.len(), 2);
    }

    #[tokio::test]
    async fn drops_stores_that_no_longer_exist_and_counts_failures() {
        let stores = shops(2);
        let mut state = StoreState::default();
        state.stores.insert("gone".into(), entry(Some(1.0), "2026-01-01T00:00:00+00:00"));
        let (state, stats) = run(
            &stores,
            state,
            |s: Shop| async move {
                if s.0 == "s1" {
                    Err(FetchError::Other("kaputt".into()))
                } else {
                    Ok(Some(Offer::simple(50.0)))
                }
            },
            None,
            2,
            None,
            |_| true,
        )
        .await;
        assert!(!state.stores.contains_key("gone"));
        assert_eq!((stats.checked, stats.failed, stats.offers, stats.total, stats.known), (1, 1, 1, 2, 1));
    }

    #[tokio::test]
    async fn reports_progress_with_filtered_best() {
        let stores = shops(3);
        let mut reporter = ScanReporter::new("scan-test", "T", "Gerät", None).with_cadence(1, 3600.0);
        let prices = [("s0", 200.0), ("s1", 100.0), ("s2", 150.0)];
        let (_, _) = run(
            &stores,
            StoreState::default(),
            move |s: Shop| async move { Ok(prices.iter().find(|p| p.0 == s.0).map(|p| Offer::simple(p.1))) },
            None,
            1,
            Some(&mut reporter),
            |o| o.total_price > 120.0, // der Filter schließt s1 aus
        )
        .await;
        let progress = crate::activity::progress("scan-test").unwrap();
        assert_eq!(progress["best_price"], 150.0);
        assert_eq!(progress["done"], 3);
    }

    #[test]
    fn fresh_offers_skip_stale_entries() {
        let mut state = StoreState::default();
        state.stores.insert("a".into(), entry(Some(20.0), &now_iso()));
        state.stores.insert("b".into(), entry(Some(10.0), "2020-01-01T00:00:00+00:00"));
        state.stores.insert("c".into(), entry(None, &now_iso()));
        let offers = fresh_offers(&state, 24.0);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].store_id.as_deref(), Some("a"));
        assert_eq!(fresh_offers(&state, f64::INFINITY).len(), 2);
    }
}
