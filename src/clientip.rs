//! Echte Besucher-IP hinter einem Reverse-Proxy (z. B. Pangolin, Traefik, Nginx Proxy Manager).
//!
//! Proxy-Header wie `X-Forwarded-For` kann jeder Client selbst mitschicken. Sie werden deshalb nur
//! ausgewertet, wenn die Verbindung von einem vertrauenswürdigen Proxy kommt (`TRUSTED_PROXIES`).
//! In der Kette wird dann von rechts gelesen: Der erste Eintrag, der kein vertrauenswürdiger Proxy
//! ist, ist der Besucher. So lässt sich die Adresse nicht durch einen selbst gesetzten Header fälschen.

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    fn parse(text: &str) -> Option<Net> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr = normalize(addr.trim().parse().ok()?);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Net { addr, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, normalize(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// IPv4-Adressen in IPv6-Schreibweise (`::ffff:1.2.3.4`) als IPv4 behandeln
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Private Netze, Loopback und Link-Local – passt für Proxys im selben Docker-/Heimnetz
const PRIVATE: &[&str] = &[
    "127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16",
    "100.64.0.0/10", "::1/128", "fc00::/7", "fe80::/10",
];

#[derive(Clone, Debug, Default)]
pub struct TrustedProxies {
    nets: Vec<Net>,
    spec: String,
}

impl TrustedProxies {
    /// Kommagetrennte Liste aus IPs, Netzen (CIDR) und dem Stichwort `private`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut nets = Vec::new();
        for token in spec.split([',', ' ', ';']).map(str::trim).filter(|t| !t.is_empty()) {
            if token.eq_ignore_ascii_case("private") {
                nets.extend(PRIVATE.iter().filter_map(|n| Net::parse(n)));
            } else {
                nets.push(Net::parse(token).ok_or_else(|| format!("„{token}“ ist keine gültige IP-Adresse bzw. kein Netz (CIDR)"))?);
            }
        }
        Ok(TrustedProxies { nets, spec: spec.trim().to_string() })
    }

    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(ip))
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    pub fn spec(&self) -> &str {
        &self.spec
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientIp {
    /// Adresse des Besuchers (hinter einem vertrauenswürdigen Proxy aus dessen Headern)
    pub ip: IpAddr,
    /// Adresse, von der die Verbindung direkt kommt (z. B. der Proxy)
    pub peer: IpAddr,
    /// Die Adresse stammt aus Proxy-Headern
    pub via_proxy: bool,
    /// Proxy-Header waren da, wurden aber ignoriert, weil `peer` nicht vertrauenswürdig ist
    pub header_ignored: bool,
}

/// Eine Adresse aus einem Header-Eintrag: `1.2.3.4`, `1.2.3.4:5678`, `[2001:db8::1]:80`, `"[::1]"`
fn parse_addr(text: &str) -> Option<IpAddr> {
    let text = text.trim().trim_matches('"');
    if let Some(rest) = text.strip_prefix('[') {
        return rest.split(']').next()?.parse().ok().map(normalize);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(normalize(ip));
    }
    // IPv4 mit Port
    let (host, port) = text.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

fn header_values<'a>(headers: &'a HeaderMap, name: &str) -> impl Iterator<Item = &'a str> {
    headers.get_all(name).into_iter().filter_map(|v| v.to_str().ok())
}

/// Adressen aus den Proxy-Headern, von links (Besucher) nach rechts (letzter Proxy).
/// Ungültige Einträge (z. B. `unknown`) machen die Kette ab dort unbrauchbar.
fn forwarded_chain(headers: &HeaderMap) -> Option<Vec<Option<IpAddr>>> {
    let xff: Vec<Option<IpAddr>> = header_values(headers, "x-forwarded-for")
        .flat_map(|v| v.split(','))
        .filter(|e| !e.trim().is_empty())
        .map(parse_addr)
        .collect();
    if !xff.is_empty() {
        return Some(xff);
    }
    // RFC 7239: Forwarded: for=192.0.2.60;proto=https, for="[2001:db8::1]:4711"
    let forwarded: Vec<Option<IpAddr>> = header_values(headers, "forwarded")
        .flat_map(|v| v.split(','))
        .filter_map(|element| {
            element.split(';').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                key.trim().eq_ignore_ascii_case("for").then(|| parse_addr(value))
            })
        })
        .collect();
    if !forwarded.is_empty() {
        return Some(forwarded);
    }
    header_values(headers, "x-real-ip").next().map(|v| vec![parse_addr(v)])
}

pub fn resolve(peer: IpAddr, headers: &HeaderMap, trusted: &TrustedProxies) -> ClientIp {
    let peer = normalize(peer);
    let chain = forwarded_chain(headers);
    if !trusted.is_trusted(peer) {
        return ClientIp { ip: peer, peer, via_proxy: false, header_ignored: chain.is_some() };
    }
    let Some(chain) = chain else {
        return ClientIp { ip: peer, peer, via_proxy: false, header_ignored: false };
    };
    // Von rechts lesen und vertrauenswürdige Proxys überspringen
    let mut client = peer;
    for entry in chain.iter().rev() {
        match entry {
            Some(ip) => {
                client = *ip;
                if !trusted.is_trusted(*ip) {
                    break;
                }
            }
            // Unlesbarer Eintrag: nicht weiter nach links gehen, letzte sichere Adresse behalten
            None => break,
        }
    }
    ClientIp { ip: client, peer, via_proxy: client != peer, header_ignored: false }
}

pub fn unspecified() -> IpAddr {
    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
}

/// Sperrt eine IP nach zu vielen fehlgeschlagenen Anmeldungen vorübergehend.
pub struct LoginGuard {
    attempts: Mutex<HashMap<IpAddr, (u32, Instant, Option<Instant>)>>,
    max_failures: u32,
    window: Duration,
    lockout: Duration,
}

pub enum LoginCheck {
    Allowed,
    Locked(Duration),
}

impl Default for LoginGuard {
    fn default() -> Self {
        LoginGuard::new(10, Duration::from_secs(15 * 60), Duration::from_secs(15 * 60))
    }
}

impl LoginGuard {
    pub fn new(max_failures: u32, window: Duration, lockout: Duration) -> Self {
        LoginGuard { attempts: Mutex::new(HashMap::new()), max_failures, window, lockout }
    }

    pub fn check(&self, ip: IpAddr) -> LoginCheck {
        let mut attempts = self.attempts.lock().unwrap();
        if let Some((_, _, Some(until))) = attempts.get(&ip) {
            let now = Instant::now();
            if *until > now {
                return LoginCheck::Locked(*until - now);
            }
            attempts.remove(&ip);
        }
        LoginCheck::Allowed
    }

    /// Fehlversuch zählen. Liefert true, wenn die IP dadurch gesperrt wurde.
    pub fn failed(&self, ip: IpAddr) -> bool {
        let mut attempts = self.attempts.lock().unwrap();
        let now = Instant::now();
        // Alte Einträge aufräumen, damit die Liste nicht wächst
        attempts.retain(|_, (_, first, locked)| locked.is_some_and(|u| u > now) || now - *first < self.window);
        let entry = attempts.entry(ip).or_insert((0, now, None));
        entry.0 += 1;
        if entry.0 >= self.max_failures && entry.2.is_none() {
            entry.2 = Some(now + self.lockout);
            return true;
        }
        false
    }

    pub fn succeeded(&self, ip: IpAddr) {
        self.attempts.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn parses_ips_networks_and_private_keyword() {
        let t = TrustedProxies::parse("172.18.0.0/16, 10.0.0.5 2001:db8::/32").unwrap();
        assert!(t.is_trusted(ip("172.18.3.4")) && !t.is_trusted(ip("172.19.0.1")));
        assert!(t.is_trusted(ip("10.0.0.5")) && !t.is_trusted(ip("10.0.0.6")));
        assert!(t.is_trusted(ip("2001:db8::abcd")) && t.is_trusted(ip("::ffff:10.0.0.5")));
        let p = TrustedProxies::parse("private").unwrap();
        assert!(p.is_trusted(ip("192.168.1.10")) && p.is_trusted(ip("172.17.0.2")) && !p.is_trusted(ip("8.8.8.8")));
        assert!(TrustedProxies::parse("").unwrap().is_empty());
        assert!(TrustedProxies::parse("nonsense").is_err() && TrustedProxies::parse("10.0.0.0/40").is_err());
    }

    #[test]
    fn headers_are_ignored_from_untrusted_peers() {
        let trusted = TrustedProxies::parse("172.18.0.0/16").unwrap();
        let spoofed = headers(&[("x-forwarded-for", "1.2.3.4")]);
        let c = resolve(ip("203.0.113.9"), &spoofed, &trusted);
        assert_eq!(c.ip, ip("203.0.113.9"));
        assert!(!c.via_proxy && c.header_ignored);
        // ohne konfigurierte Proxys werden Header nie ausgewertet
        let c = resolve(ip("172.18.0.2"), &spoofed, &TrustedProxies::default());
        assert_eq!(c.ip, ip("172.18.0.2"));
    }

    #[test]
    fn reads_chain_from_the_right_and_resists_spoofing() {
        let trusted = TrustedProxies::parse("172.18.0.0/16, 10.1.1.1").unwrap();
        // Besucher schickt selbst einen gefälschten Eintrag mit; Pangolin hängt die echte IP an
        let h = headers(&[("x-forwarded-for", "6.6.6.6, 198.51.100.7, 10.1.1.1")]);
        let c = resolve(ip("172.18.0.5"), &h, &trusted);
        assert_eq!(c.ip, ip("198.51.100.7"));
        assert!(c.via_proxy);
        // mehrere Header-Zeilen und Ports
        let h = headers(&[("x-forwarded-for", "198.51.100.7:51234"), ("x-forwarded-for", "10.1.1.1")]);
        assert_eq!(resolve(ip("172.18.0.5"), &h, &trusted).ip, ip("198.51.100.7"));
    }

    #[test]
    fn supports_forwarded_and_x_real_ip() {
        let trusted = TrustedProxies::parse("private").unwrap();
        let h = headers(&[("forwarded", "for=\"[2001:db8::7]:4711\";proto=https")]);
        assert_eq!(resolve(ip("172.18.0.5"), &h, &trusted).ip, ip("2001:db8::7"));
        let h = headers(&[("x-real-ip", "198.51.100.8")]);
        assert_eq!(resolve(ip("::ffff:172.18.0.5"), &h, &trusted).ip, ip("198.51.100.8"));
        // unlesbarer Eintrag: nicht weiter nach links lesen
        let h = headers(&[("x-forwarded-for", "198.51.100.9, unknown")]);
        assert_eq!(resolve(ip("172.18.0.5"), &h, &trusted).ip, ip("172.18.0.5"));
    }

    #[test]
    fn login_guard_locks_after_repeated_failures() {
        let guard = LoginGuard::new(3, Duration::from_secs(60), Duration::from_secs(60));
        let a = ip("198.51.100.7");
        assert!(!guard.failed(a) && !guard.failed(a));
        assert!(guard.failed(a));
        assert!(matches!(guard.check(a), LoginCheck::Locked(_)));
        assert!(matches!(guard.check(ip("198.51.100.8")), LoginCheck::Allowed));
        guard.succeeded(a);
        assert!(matches!(guard.check(a), LoginCheck::Allowed));
    }
}
