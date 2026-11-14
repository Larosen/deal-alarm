//! Deal-Alarm: beobachtet Preise bei mehreren Quellen (expert, Euronics/media@home, Amazon)
//! und schickt per ntfy einen Alarm, sobald der Wunschpreis erreicht ist.

pub mod activity;
pub mod api;
pub mod checker;
pub mod model;
pub mod notifier;
pub mod ratelimit;
pub mod scan;
pub mod shops;
pub mod storage;
pub mod util;
