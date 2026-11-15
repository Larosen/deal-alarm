use deal_alarm::api::{self, AppState};
use deal_alarm::checker::{Engine, Scheduler};
use deal_alarm::clientip::{LoginGuard, TrustedProxies};
use deal_alarm::storage::Storage;
use deal_alarm::{info, warn};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// `deal-alarm --healthcheck`: prüft /healthz der laufenden Instanz (für HEALTHCHECK im Dockerfile).
async fn healthcheck() -> i32 {
    let port = env_or("PORT", "8080");
    let url = format!("http://127.0.0.1:{port}/healthz");
    match reqwest::Client::new().get(url).timeout(std::time::Duration::from_secs(4)).send().await {
        Ok(response) if response.status().is_success() => 0,
        _ => 1,
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("deal-alarm {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck().await);
    }
    let data_dir = PathBuf::from(env_or("DATA_DIR", "./data"));
    let storage = match Storage::open(&data_dir) {
        Ok(storage) => Arc::new(storage),
        Err(error) => {
            eprintln!("Zustand kann nicht geladen werden: {error}");
            std::process::exit(1);
        }
    };
    let password = env_or("UI_PASSWORD", "");
    let auth = (!password.is_empty()).then(|| (env_or("UI_USER", "admin"), password));
    let engine = Engine::new(storage.clone());
    engine.start_workers();
    let scheduler = Scheduler::new();
    scheduler.start(engine.clone(), storage.clone());
    let trusted_proxies = match TrustedProxies::parse(&env_or("TRUSTED_PROXIES", "")) {
        Ok(trusted) => trusted,
        Err(error) => {
            eprintln!("TRUSTED_PROXIES ungültig: {error}");
            std::process::exit(1);
        }
    };
    if !trusted_proxies.is_empty() {
        info!("X-Forwarded-For wird ausgewertet für Proxys: {}", trusted_proxies.spec());
    }
    let state = AppState {
        storage,
        engine,
        scheduler,
        client: reqwest::Client::new(),
        auth,
        trusted_proxies: Arc::new(trusted_proxies),
        login_guard: Arc::new(LoginGuard::default()),
    };

    let host = env_or("HOST", "0.0.0.0");
    let port = env_or("PORT", "8080");
    let listener = match tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Port {host}:{port} nicht verfügbar: {error}");
            std::process::exit(1);
        }
    };
    info!("Deal-Alarm {} läuft auf http://{host}:{port}", env!("CARGO_PKG_VERSION"));
    let app = api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(error) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await {
        warn!("Server beendet: {error}");
    }
}
