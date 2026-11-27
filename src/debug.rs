//! Diagnosemodus: speichert die Antworten der Shops unverändert auf der Festplatte.
//!
//! Eingeschaltet mit `DEBUG_CAPTURE=1`. Die Dateien landen unter `<DATA_DIR>/debug/<shop>/`
//! (Standard `./data/debug`), je Shop werden nur die neuesten [`KEEP_PER_SHOP`] behalten.
//! So sieht man, was der Deal-Alarm selbst bekommt (ohne JavaScript, mit seiner IP) –
//! daraus werden Testdateien für die automatischen Tests.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const KEEP_PER_SHOP: usize = 200;

fn target() -> &'static Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let on = std::env::var("DEBUG_CAPTURE").map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on")).unwrap_or(false);
        on.then(|| PathBuf::from(std::env::var("DATA_DIR").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "./data".into())).join("debug"))
    })
}

pub fn enabled() -> bool {
    target().is_some()
}

/// Dateiname aus Zeit und URL, z. B. `20261127-071500-123_www.euronics.de_search_sSearch_4548….html`.
pub fn file_name(url: &str, status: u16, ext: &str) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let cleaned: String = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let cleaned: String = cleaned.chars().take(120).collect();
    format!("{stamp}_{status}_{cleaned}.{ext}")
}

/// Speichert eine Antwort (nur im Diagnosemodus). Fehler beim Schreiben werden ignoriert.
pub fn capture(shop: &str, url: &str, status: u16, body: &str) {
    let Some(base) = target() else { return };
    let dir = base.join(shop);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ext = if body.trim_start().starts_with('{') || body.trim_start().starts_with('[') { "json" } else { "html" };
    let content = format!("<!-- deal-alarm debug: {url} (HTTP {status}) -->\n{body}");
    let _ = std::fs::write(dir.join(file_name(url, status, ext)), content);
    prune(&dir);
}

fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
    if files.len() <= KEEP_PER_SHOP {
        return;
    }
    files.sort(); // Name beginnt mit Zeitstempel
    for old in &files[..files.len() - KEEP_PER_SHOP] {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_are_safe() {
        let name = super::file_name("https://www.euronics.de/search?sSearch=Sony WH/1000", 200, "html");
        assert!(name.ends_with("_200_www.euronics.de_search_sSearch_Sony_WH_1000.html"), "{name}");
        assert!(!name.contains('/') && !name.contains('?'));
    }
}
