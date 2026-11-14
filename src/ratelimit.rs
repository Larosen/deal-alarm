//! Adaptives Rate-Limit pro Shop.
//!
//! Jeder Shop bekommt einen Limiter, der Anfragen gleichmäßig verteilt. Meldet der Shop
//! eine Drosselung (429, 503 oder ein WAF-403), wird das Tempo halbiert und eine Pause
//! eingelegt (Retry-After wird beachtet). Nach einer Serie erfolgreicher Anfragen steigt das
//! Tempo langsam wieder bis zum Maximum. Häufen sich Drosselungen, bricht der Scan ab und
//! setzt im nächsten Zyklus an derselben Stelle fort.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const THROTTLE_STATUS: &[u16] = &[429, 503];

/// Der Shop drosselt zu stark – Scan abbrechen und später fortsetzen.
#[derive(Debug, Clone)]
pub struct RateLimited(pub String);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

struct Inner {
    max_rate: f64,
    rate: f64,
    next_slot: Instant,
    pause_until: Instant,
    streak: u32,
    strikes: u32,
    requests: u64,
    throttled: u64,
}

pub struct AdaptiveLimiter {
    name: String,
    min_rate: f64,
    max_strikes: u32,
    max_pause: f64,
    inner: Mutex<Inner>,
}

#[derive(Serialize, Debug)]
pub struct Snapshot {
    pub rate_per_min: f64,
    pub max_rate_per_min: f64,
    pub requests: u64,
    pub throttled: u64,
    pub paused: bool,
}

impl AdaptiveLimiter {
    pub fn new(name: &str, per_minute: f64, min_per_minute: f64) -> Self {
        let now = Instant::now();
        let rate = per_minute.max(min_per_minute);
        AdaptiveLimiter {
            name: name.to_string(),
            min_rate: min_per_minute,
            max_strikes: 5,
            max_pause: 300.0,
            inner: Mutex::new(Inner {
                max_rate: rate, rate, next_slot: now, pause_until: now, streak: 0, strikes: 0, requests: 0, throttled: 0,
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn configure(&self, per_minute: f64) {
        let per_minute = per_minute.max(self.min_rate);
        let mut s = self.inner.lock().unwrap();
        if s.max_rate != per_minute {
            s.max_rate = per_minute;
            s.rate = per_minute;
        }
    }

    pub fn start_scan(&self) {
        self.inner.lock().unwrap().strikes = 0;
    }

    /// Wartet bis zum nächsten freien Slot.
    pub async fn wait(&self) -> Result<(), RateLimited> {
        let delay = {
            let mut s = self.inner.lock().unwrap();
            if s.strikes >= self.max_strikes {
                return Err(RateLimited(format!("{} drosselt die Anfragen", self.name)));
            }
            let now = Instant::now();
            let start = now.max(s.next_slot).max(s.pause_until);
            s.next_slot = start + Duration::from_secs_f64(60.0 / s.rate);
            start.saturating_duration_since(now)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    pub fn ok(&self) {
        let mut s = self.inner.lock().unwrap();
        s.requests += 1;
        s.strikes = 0;
        s.streak += 1;
        if s.streak >= 25 && s.rate < s.max_rate {
            s.rate = (s.rate * 1.15).min(s.max_rate);
            s.streak = 0;
        }
    }

    pub fn throttled(&self, retry_after: Option<f64>) {
        let mut s = self.inner.lock().unwrap();
        s.requests += 1;
        s.throttled += 1;
        s.strikes += 1;
        s.streak = 0;
        s.rate = (s.rate / 2.0).max(self.min_rate);
        let pause = retry_after.filter(|p| *p > 0.0).unwrap_or(15.0 * 2f64.powi(s.strikes as i32 - 1));
        s.pause_until = Instant::now() + Duration::from_secs_f64(pause.min(self.max_pause));
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.inner.lock().unwrap();
        Snapshot {
            rate_per_min: (s.rate * 10.0).round() / 10.0,
            max_rate_per_min: s.max_rate,
            requests: s.requests,
            throttled: s.throttled,
            paused: s.pause_until > Instant::now(),
        }
    }

    #[cfg(test)]
    pub fn rate(&self) -> f64 {
        self.inner.lock().unwrap().rate
    }

    #[cfg(test)]
    pub fn strikes(&self) -> u32 {
        self.inner.lock().unwrap().strikes
    }
}

fn retry_after(response: &reqwest::Response) -> Option<f64> {
    response.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()
}

/// GET unter Kontrolle des Limiters. Liefert die Antwort oder `RateLimited`.
pub async fn get(
    client: &reqwest::Client,
    limiter: &AdaptiveLimiter,
    url: &str,
    query: &[(&str, &str)],
    throttle_status: &[u16],
    attempts: usize,
    timeout: Duration,
) -> Result<reqwest::Response, RateLimited> {
    for _ in 0..attempts {
        limiter.wait().await?;
        let response = match client.get(url).query(query).timeout(timeout).send().await {
            Ok(r) => r,
            Err(_) => {
                limiter.throttled(None);
                continue;
            }
        };
        if throttle_status.contains(&response.status().as_u16()) {
            limiter.throttled(retry_after(&response));
            continue;
        }
        limiter.ok();
        return Ok(response);
    }
    Err(RateLimited(format!("{}: keine Antwort nach {attempts} Versuchen", limiter.name)))
}

static LIMITERS: OnceLock<Mutex<HashMap<String, Arc<AdaptiveLimiter>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<AdaptiveLimiter>>> {
    LIMITERS.get_or_init(Default::default)
}

pub fn get_limiter(name: &str, per_minute: f64, min_per_minute: f64) -> Arc<AdaptiveLimiter> {
    let mut map = registry().lock().unwrap();
    match map.get(name) {
        Some(limiter) => {
            limiter.configure(per_minute);
            limiter.clone()
        }
        None => {
            let limiter = Arc::new(AdaptiveLimiter::new(name, per_minute, min_per_minute));
            map.insert(name.to_string(), limiter.clone());
            limiter
        }
    }
}

pub fn snapshots() -> HashMap<String, Snapshot> {
    registry().lock().unwrap().iter().map(|(n, l)| (n.clone(), l.snapshot())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_and_recovers() {
        let limiter = AdaptiveLimiter::new("t", 60.0, 2.0);
        limiter.throttled(None);
        assert_eq!(limiter.rate(), 30.0);
        limiter.throttled(Some(1.0));
        assert_eq!(limiter.rate(), 15.0);
        for _ in 0..25 {
            limiter.ok();
        }
        assert!(limiter.rate() > 15.0 && limiter.rate() <= 60.0);
        assert_eq!(limiter.strikes(), 0);
        for _ in 0..40 {
            limiter.throttled(None);
        }
        assert_eq!(limiter.rate(), 2.0);
    }

    #[tokio::test]
    async fn aborts_after_repeated_throttling() {
        let limiter = AdaptiveLimiter::new("t", 6000.0, 1.0);
        for _ in 0..5 {
            limiter.throttled(Some(0.001));
        }
        assert!(limiter.wait().await.is_err());
        limiter.start_scan();
        // Pause (1 ms) abwarten, danach geht es wieder
        assert!(limiter.wait().await.is_ok());
    }

    #[tokio::test]
    async fn spaces_requests_evenly() {
        let limiter = AdaptiveLimiter::new("t", 1200.0, 1.0); // 50 ms Abstand
        let start = Instant::now();
        for _ in 0..4 {
            limiter.wait().await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(140));
    }
}
