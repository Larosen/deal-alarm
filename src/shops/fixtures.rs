//! Prüft echte, gespeicherte Shop-Seiten aus `fixtures/` gegen die Auswertung (siehe fixtures/README.md).

use serde_json::Value;
use std::path::{Path, PathBuf};

/// (Seite, Erwartung) für alle Paare in `fixtures/<kind>/`.
fn cases(kind: &str) -> Vec<(PathBuf, String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(kind);
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut out = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let page = std::fs::read_to_string(&path).unwrap();
        let expect = std::fs::read_to_string(path.with_extension("json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        out.push((path, page, expect));
    }
    out
}

fn close(a: f64, b: &Value) -> bool {
    b.as_f64().is_none_or(|b| (a - b).abs() < 0.005)
}

#[test]
fn product_pages() {
    for (path, page, expect) in cases("product") {
        let p = super::euronics::parse_product_page(&page, "https://example.invalid/")
            .unwrap_or_else(|| panic!("{}: keine Produktdaten gefunden", path.display()));
        if let Some(t) = expect["title"].as_str() {
            assert_eq!(p.title, t, "{}", path.display());
        }
        if let Some(e) = expect["ean"].as_str() {
            assert!(super::euronics::same_ean(&p.ean, e), "{}: EAN {} statt {e}", path.display(), p.ean);
        }
        assert!(close(p.price.unwrap_or(-1.0), &expect["price"]), "{}: Preis {:?}", path.display(), p.price);
    }
}

#[test]
fn amazon_offer_lists() {
    for (path, page, expect) in cases("amazon-offers") {
        let offers = super::amazon::parse_offers(&page);
        assert!(!offers.is_empty(), "{}: keine Angebote erkannt", path.display());
        if let Some(n) = expect["offers"].as_u64() {
            assert_eq!(offers.len() as u64, n, "{}", path.display());
        }
        let cheapest = offers.iter().map(|o| o.price).fold(f64::INFINITY, f64::min);
        assert!(close(cheapest, &expect["cheapest"]), "{}: günstigstes {cheapest}", path.display());
    }
}

#[test]
fn amazon_search_pages() {
    for (path, page, expect) in cases("amazon-search") {
        let results = super::amazon::parse_search(&page);
        assert!(!results.is_empty(), "{}: keine Treffer erkannt", path.display());
        if let Some(a) = expect["first_asin"].as_str() {
            assert_eq!(results.iter().find(|r| !r.sponsored).map(|r| r.asin.as_str()), Some(a), "{}", path.display());
        }
        if let Some(n) = expect["results"].as_u64() {
            assert_eq!(results.len() as u64, n, "{}", path.display());
        }
    }
}

#[test]
fn idealo_search_pages() {
    for (path, page, expect) in cases("idealo-search") {
        let links = super::idealo::search_links(&page, "https://www.idealo.de");
        assert!(!links.is_empty(), "{}: keine Treffer erkannt", path.display());
        if let Some(id) = expect["first_id"].as_str() {
            assert_eq!(super::idealo::product_id(&links[0].0).as_deref(), Some(id), "{}", path.display());
        }
        if let Some(n) = expect["results"].as_u64() {
            assert_eq!(links.len() as u64, n, "{}", path.display());
        }
    }
}
