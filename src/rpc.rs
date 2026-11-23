//! A small JSON-RPC client for riecoind (from bound-pool): HTTP/1.1 POST to 127.0.0.1, Basic auth with the node's
//! cookie (`<datadir>/.cookie`, "__cookie__:<hex>"), re-read on every call because the node makes
//! a new one at each start.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

pub struct Rpc {
    pub addr: String,
    pub cookie: PathBuf,
    pub timeout: Duration,
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

impl Rpc {
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let cookie = std::fs::read_to_string(&self.cookie).map_err(|e| format!("read cookie {}: {e}", self.cookie.display()))?;
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let mut s = TcpStream::connect(&self.addr).map_err(|e| format!("connect {}: {e}", self.addr))?;
        let _ = s.set_read_timeout(Some(self.timeout));
        let _ = s.set_write_timeout(Some(self.timeout));
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            base64(cookie.trim().as_bytes()),
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).map_err(|e| format!("send: {e}"))?;
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&resp);
        let (head, body) = text.split_once("\r\n\r\n").ok_or("bad HTTP reply")?;
        let status = head.split_whitespace().nth(1).unwrap_or("");
        if status == "401" {
            return Err("RPC refused the cookie (401)".into());
        }
        let v: Value = serde_json::from_str(body.trim()).map_err(|e| format!("HTTP {status}: bad JSON: {e}"))?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(format!("{method}: {err}"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_vectors() {
        for (p, e) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64(p.as_bytes()), e);
        }
    }
}
