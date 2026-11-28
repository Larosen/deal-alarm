//! Kleine Helfer: Zeitstempel, Preisformat, Entfernung.

use chrono::{DateTime, Local, SecondsFormat};

pub fn now_iso() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn iso_from_epoch(secs: f64) -> String {
    DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::Secs, false))
        .unwrap_or_default()
}

/// Alter eines ISO-Zeitstempels in Stunden (unendlich bei leerem/ungültigem Wert).
pub fn age_hours(iso: &str) -> f64 {
    match DateTime::parse_from_rfc3339(iso) {
        Ok(then) => (Local::now().fixed_offset() - then).num_milliseconds() as f64 / 3_600_000.0,
        Err(_) => f64::INFINITY,
    }
}

/// 1299.5 -> "1.299,50 €"
pub fn format_price(value: f64) -> String {
    let cents = (value * 100.0).round() as i64;
    let (euros, rest) = (cents.abs() / 100, cents.abs() % 100);
    let digits = euros.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(c);
    }
    format!("{}{grouped},{rest:02} €", if cents < 0 { "-" } else { "" })
}

pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1, lat2, lon2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

/// '1.299,00 €', '1299.00' oder 1299 -> 1299.0
pub fn parse_price(value: &str) -> Option<f64> {
    let text = value.replace('€', "").replace("EUR", "").replace('\u{a0}', "");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let normalized = if text.contains(',') { text.replace('.', "").replace(',', ".") } else { text.to_string() };
    normalized.parse().ok()
}

/// Erster deutscher Preis in einem Text: „1.929,00 € mit 30 Prozent Einsparungen“ -> 1929.0
pub fn first_price(text: &str) -> Option<f64> {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = R.get_or_init(|| regex::Regex::new(r"(\d{1,3}(?:\.\d{3})+|\d+),(\d{2})").unwrap());
    re.captures(text).and_then(|c| parse_price(&format!("{},{}", &c[1], &c[2])))
}

/// Preis aus einem JSON-Wert (Zahl oder Text).
pub fn parse_price_value(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => parse_price(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices() {
        assert_eq!(parse_price("1.299,00 €"), Some(1299.0));
        assert_eq!(parse_price("199,99"), Some(199.99));
        assert_eq!(parse_price("1299.00"), Some(1299.0));
        assert_eq!(parse_price(""), None);
        assert_eq!(first_price("1.929,00 € mit 30 Prozent Einsparungen"), Some(1929.0));
        assert_eq!(first_price("ab 1.699,00 €"), Some(1699.0));
        assert_eq!(first_price("Preis: 89,90 €"), Some(89.9));
        assert_eq!(first_price("kein Preis"), None);
        assert_eq!(parse_price_value(&serde_json::json!(249)), Some(249.0));
        assert_eq!(parse_price_value(&serde_json::Value::Null), None);
    }

    #[test]
    fn formats_prices() {
        assert_eq!(format_price(1299.5), "1.299,50 €");
        assert_eq!(format_price(5.0), "5,00 €");
        assert_eq!(format_price(1234567.891), "1.234.567,89 €");
    }

    #[test]
    fn age_of_timestamps() {
        assert!(age_hours("") .is_infinite());
        assert!(age_hours(&now_iso()) < 0.01);
    }

    #[test]
    fn distance() {
        let hamburg = (53.55, 9.99);
        let berlin = (52.52, 13.40);
        let d = haversine_km(hamburg, berlin);
        assert!((d - 255.0).abs() < 5.0, "{d}");
    }
}
