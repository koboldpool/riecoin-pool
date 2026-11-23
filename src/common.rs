//! Small helpers (hex, time), from bound-pool's common.rs.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    unhex(s)?.try_into().ok()
}

/// A hash from its display hex (most significant byte first, as the node shows it) into raw byte order.
pub fn from_display(s: &str) -> Option<[u8; 32]> {
    let mut b = unhex32(s)?;
    b.reverse();
    Some(b)
}

/// The inverse of from_display.
pub fn to_display(b: &[u8; 32]) -> String {
    let mut r = *b;
    r.reverse();
    hex(&r)
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
