//! Gemeinsame Hilfsfunktionen für die Shop-Abfragen.

use crate::model::Coords;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT as UA};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0";

/// HTTP-Client mit Browser-Kennung; `accept` setzt den Accept-Header.
pub fn http_client(accept: &str) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(UA, HeaderValue::from_static(USER_AGENT));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE,de;q=0.9,en;q=0.5"));
    headers.insert(ACCEPT, HeaderValue::from_str(accept).unwrap_or_else(|_| HeaderValue::from_static("*/*")));
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP-Client")
}

static PLZ_CACHE: OnceLock<Mutex<HashMap<String, Option<Coords>>>> = OnceLock::new();

/// Geo-Koordinaten für eine deutsche PLZ (`None` bei Fehler; Erfolge werden gemerkt).
pub async fn plz_coordinates(plz: &str) -> Option<Coords> {
    let plz = plz.trim();
    if plz.len() != 5 || !plz.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let cache = PLZ_CACHE.get_or_init(Default::default);
    if let Some(Some(hit)) = cache.lock().unwrap().get(plz) {
        return Some(*hit);
    }
    let found = lookup_plz(plz).await;
    if found.is_some() {
        cache.lock().unwrap().insert(plz.to_string(), found);
    }
    found
}

async fn lookup_plz(plz: &str) -> Option<Coords> {
    let response = reqwest::Client::new()
        .get(format!("https://api.zippopotam.us/de/{plz}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: serde_json::Value = response.json().await.ok()?;
    let place = data["places"].get(0)?;
    Some((place["latitude"].as_str()?.parse().ok()?, place["longitude"].as_str()?.parse().ok()?))
}

#[cfg(test)]
pub fn seed_plz(plz: &str, coords: Coords) {
    PLZ_CACHE.get_or_init(Default::default).lock().unwrap().insert(plz.to_string(), Some(coords));
}

/// Wie [`http_client`], aber mit Cookie-Speicher (Shops, die eine Sitzung erwarten).
pub fn http_client_with_cookies(accept: &str) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(UA, HeaderValue::from_static(USER_AGENT));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE,de;q=0.9,en;q=0.5"));
    headers.insert(ACCEPT, HeaderValue::from_str(accept).unwrap_or_else(|_| HeaderValue::from_static("*/*")));
    reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP-Client")
}

/// Wörter, die bei der Namenssuche nichts über das Gerät aussagen.
const FILLER: &[&str] = &[
    "und", "mit", "für", "fur", "the", "and", "with", "von", "der", "die", "das", "inkl", "neu", "new", "ohne",
    "zoll", "inch", "schwarz", "weiss", "weiß", "silber", "grau", "black", "white", "silver", "edition",
];

/// Zerlegt einen Produktnamen in kleingeschriebene Wörter (Satzzeichen trennen).
pub fn name_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 || t.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

/// Wie gut passt ein Treffer (`candidate`) zum gesuchten Gerätenamen? 0.0 – 1.0.
///
/// Modellbezeichnungen (Wörter aus Buchstaben **und** Ziffern, z. B. „WH-1000XM5“ → „1000xm5“,
/// „GQ65S95HFTXZG“) müssen **alle** im Treffer vorkommen, sonst ist es ein anderes Gerät (S93 statt
/// S94, 55 statt 65 Zoll) und das Ergebnis ist 0. Länderkürzel am Ende zählen nicht: „GQ65S95HFTXZG“
/// passt zu „GQ65S95HFT“. Reine Zahlen (65, 120, 2026) sind nur Pflicht, wenn der Name gar keine
/// Modellbezeichnung enthält (z. B. „AirPods Pro 3“), sonst zählen sie wie normale Wörter.
pub fn name_match(wanted: &str, candidate: &str) -> f64 {
    let cand = name_tokens(candidate);
    let cand_joined: String = cand.concat();
    let wanted: Vec<String> = name_tokens(wanted).into_iter().filter(|t| !FILLER.contains(&t.as_str())).collect();
    if wanted.is_empty() || cand.is_empty() {
        return 0.0;
    }
    let has_digit = |t: &str| t.chars().any(|c| c.is_ascii_digit());
    let mixed = |t: &str| has_digit(t) && t.chars().any(|c| c.is_alphabetic());
    let has_mixed = wanted.iter().any(|t| mixed(t));
    let (models, words): (Vec<&String>, Vec<&String>) =
        wanted.iter().partition(|t| if has_mixed { mixed(t) } else { has_digit(t) });
    let model_found = |m: &str| {
        cand.iter().any(|c| c == m)
            // zusammengeschrieben („WH-1000XM5“ vs. „WH1000XM5“); kurze Angaben wie „3“ nur als eigenes Wort
            || (m.len() >= 4 && cand_joined.contains(m))
            // gleiche Modellnummer mit/ohne Länder- bzw. Farbkürzel („GQ65S95HFTXZG“ ≙ „GQ65S95HFT“)
            || cand.iter().any(|c| c.len() >= 8 && m.len() >= 8 && mixed(c) && (m.starts_with(c.as_str()) || c.starts_with(m)))
    };
    if !models.iter().all(|m| model_found(m)) {
        return 0.0;
    }
    // Zubehör (Hülle für …) ist nicht das Gerät selbst
    let accessory = cand.iter().any(|c| ACCESSORY.contains(&c.as_str())) && !wanted.iter().any(|w| ACCESSORY.contains(&w.as_str()));
    let penalty = if accessory { 0.5 } else { 0.0 };
    if words.is_empty() {
        return 1.0 - penalty;
    }
    let found = words
        .iter()
        .filter(|w| cand.iter().any(|c| c == **w || (w.len() >= 5 && c.starts_with(w.as_str()))) || (w.len() >= 4 && cand_joined.contains(w.as_str())))
        .count();
    let ratio = found as f64 / words.len() as f64;
    // Mit Modellnummer reicht ein Teil der Wörter (Händler formulieren Titel unterschiedlich)
    (if models.is_empty() { ratio } else { 0.5 + ratio / 2.0 }) - penalty
}

/// Modellbezeichnungen in einem Text (Wörter aus Buchstaben und Ziffern, mind. 4 Zeichen).
pub fn model_tokens(text: &str) -> Vec<String> {
    name_tokens(text)
        .into_iter()
        .filter(|t| t.len() >= 4 && t.chars().any(|c| c.is_ascii_digit()) && t.chars().any(|c| c.is_alphabetic()))
        .collect()
}

const ACCESSORY: &[&str] = &[
    "hülle", "huelle", "case", "cover", "tasche", "schutzfolie", "panzerglas", "folie", "ladekabel", "kabel", "adapter",
    "halterung", "ersatz", "ersatzteil", "ohrpolster", "fernbedienung", "wandhalterung", "ständer", "zubehör",
];

/// Mindestwert von [`name_match`], ab dem ein Treffer als dasselbe Gerät gilt.
pub const NAME_MATCH_MIN: f64 = 0.6;

/// Bester passender Treffer: (Index, Wert) oder `None`, wenn keiner gut genug passt.
pub fn best_name_match<'a>(wanted: &str, titles: impl IntoIterator<Item = &'a str>) -> Option<(usize, f64)> {
    titles
        .into_iter()
        .enumerate()
        .map(|(i, t)| (i, name_match(wanted, t)))
        .filter(|(_, s)| *s >= NAME_MATCH_MIN)
        .fold(None, |best: Option<(usize, f64)>, cur| match best {
            Some(b) if b.1 >= cur.1 => Some(b),
            _ => Some(cur),
        })
}

/// Suchbegriffe für die Namenssuche eines Geräts: der Gerätename und – falls abweichend – der von
/// einem anderen Shop erkannte Produkttitel (ohne den Shop `exclude`).
pub fn name_terms(watch: &crate::storage::Watch, exclude: &str) -> Vec<String> {
    let mut terms: Vec<String> = vec![];
    let mut push = |t: &str| {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.len() >= 3 && !terms.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
            terms.push(t);
        }
    };
    push(&watch.name);
    for (shop, result) in &watch.results {
        if shop != exclude && result.error.is_none() && !result.title.is_empty() {
            push(&result.title);
        }
    }
    terms.truncate(2);
    terms
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn matches_model_numbers_strictly() {
        assert!(name_match("Sony WH-1000XM5", "Sony WH-1000XM5 Kabellose Kopfhörer, Schwarz") >= NAME_MATCH_MIN);
        assert!(name_match("Sony WH-1000XM5", "Sony WH1000XM5 Bluetooth Kopfhörer") >= NAME_MATCH_MIN);
        assert_eq!(name_match("Sony WH-1000XM5", "Sony WH-1000XM4 Kopfhörer"), 0.0);
        assert_eq!(name_match("Samsung GQ65S94F", "Samsung GQ65S93F OLED"), 0.0);
        assert!(name_match("Apple AirPods Pro 3", "Apple AirPods Pro 3 mit USB-C") >= NAME_MATCH_MIN);
        assert!(name_match("Apple AirPods Pro", "Apple iPhone Hülle") < NAME_MATCH_MIN);
        assert_eq!(name_match("", "irgendwas"), 0.0);
        assert_eq!(name_match("Apple AirPods Pro 3", "Apple AirPods Pro 2 (2023)"), 0.0);
        assert!(name_match("Sony WH-1000XM5", "Hülle für Sony WH-1000XM5") < NAME_MATCH_MIN);
        // Echte Titel (Shop-Bericht 28.11.2026): Länderkürzel, Jahr und Größe dürfen fehlen bzw. anders stehen
        let wanted = "Samsung GQ65S95HFTXZG S95H Vision AI Smart TV (2026) OLED, 65\"";
        assert!(name_match(wanted, "Samsung GQ65S95HFT (65 Zoll)") >= NAME_MATCH_MIN);
        assert!(name_match(wanted, "Samsung GQ65S95HFT 163 cm (65\") OLED Vision AI Smart TV graphitschwarz, 4K Ultra HD") >= NAME_MATCH_MIN);
        assert!(name_match(wanted, "SAMSUNG 65 Zoll OLED 4K S95H Vision AI Smart TV (GQ65S95HFTXZG), Glare Free (2026)") >= NAME_MATCH_MIN);
        assert_eq!(name_match(wanted, "Samsung GQ-S95HFT"), 0.0); // Variantengruppe ohne Größe
        assert_eq!(name_match(wanted, "Samsung GQ55S95HFTXZG S95H"), 0.0); // 55 statt 65 Zoll
        assert_eq!(name_match(wanted, "Samsung GQ65S90H (65 Zoll)"), 0.0);
        assert_eq!(name_match(wanted, "Samsung OLED S95H 4K 65 Zoll (163 cm), UHD Fernseher"), 0.0); // ohne Modellnummer: nicht sicher
    }

    #[test]
    fn picks_best_title() {
        let titles = ["Sony WH-1000XM4", "Sony WH-1000XM5 Schwarz", "Hülle für Sony WH-1000XM5"];
        assert_eq!(best_name_match("Sony WH-1000XM5 Schwarz", titles).map(|x| x.0), Some(1));
        assert_eq!(best_name_match("Sony WH-1000XM5", ["Hülle für Sony WH-1000XM5", "Sony WH-1000XM5"]).map(|x| x.0), Some(1));
        assert!(best_name_match("Bosch Akkuschrauber", titles).is_none());
    }
}
