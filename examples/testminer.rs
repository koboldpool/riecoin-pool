//! A slow single-thread test miner for riecoin-pool, for regtest (pattern [0]: every prime is a block) and other
//! easy test jobs. Speaks the pool's stratum like rieMiner; searches n = target + 1 + primorial offset with no
//! primorial, Fermat-tests every odd candidate and submits each share of the job's minimum k.
//!
//!   testminer <host:port> <login> [password] [shares to send, default 10] [pause between shares, ms, default 1000]

use riecoin_pool::chain::{header, pow_hash, root_from_branch, swap_words};
use riecoin_pool::common::{hex, to_display, unhex};
use riecoin_pool::stella;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

struct Job {
    id: String,
    prev: [u8; 32],
    coinb1: Vec<u8>,
    coinb2: Vec<u8>,
    branch: Vec<[u8; 32]>,
    version: u32,
    bits: u32,
    ntime: u64,
    patterns: Vec<Vec<u32>>,
}

fn job_of(p: &[Value]) -> Job {
    let s = |i: usize| p[i].as_str().unwrap().to_string();
    Job {
        id: s(0),
        prev: swap_words(&unhex(&s(1)).unwrap().try_into().unwrap()),
        coinb1: unhex(&s(2)).unwrap(),
        coinb2: unhex(&s(3)).unwrap(),
        branch: p[4].as_array().unwrap().iter().map(|b| unhex(b.as_str().unwrap()).unwrap().try_into().unwrap()).collect(),
        version: u32::from_str_radix(&s(5), 16).unwrap(),
        bits: u32::from_str_radix(&s(6), 16).unwrap(),
        ntime: u64::from_str_radix(&s(7), 16).unwrap(),
        patterns: serde_json::from_value(p[10].clone()).unwrap(),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let addr = a.get(1).expect("testminer <host:port> <login> [password] [shares] [pause ms]");
    let login = a.get(2).expect("login").clone();
    let pass = a.get(3).cloned().unwrap_or_else(|| "x".into());
    let wanted: u64 = a.get(4).map(|x| x.parse().unwrap()).unwrap_or(10);
    let pause: u64 = a.get(5).map(|x| x.parse().unwrap()).unwrap_or(1000);
    let stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut out = stream.try_clone().unwrap();
    let mut rd = BufReader::new(stream);
    let mut send = |v: Value| {
        let mut l = v.to_string();
        l.push('\n');
        out.write_all(l.as_bytes()).unwrap();
    };
    send(json!({"id": 1, "method": "mining.subscribe", "params": ["testminer/0.1"]}));
    send(json!({"id": 2, "method": "mining.authorize", "params": [login, pass]}));
    let mut en1 = Vec::new();
    let mut job: Option<Job> = None;
    let (mut sent, mut accepted, mut rejected, mut en2_counter) = (0u64, 0u64, 0u64, 0u32);
    let mut poff: u64 = 0;
    let mut next_id = 10;
    let mut line = String::new();
    loop {
        // Read whatever the pool sent.
        loop {
            line.clear();
            match rd.read_line(&mut line) {
                Ok(0) => {
                    println!("pool closed the connection; sent {sent}, accepted {accepted}, rejected {rejected}");
                    std::process::exit(1)
                }
                Ok(_) => {
                    let v: Value = serde_json::from_str(&line).unwrap();
                    if v["method"] == "mining.notify" {
                        let j = job_of(v["params"].as_array().unwrap());
                        if v["params"][8] == true || job.as_ref().is_none_or(|o| o.prev != j.prev) {
                            poff = 0;
                        }
                        job = Some(j);
                    } else if v["id"] == 1 {
                        en1 = unhex(v["result"][1].as_str().unwrap()).unwrap();
                    } else if v["id"].as_u64().is_some_and(|i| i >= 10) {
                        if v["result"] == true {
                            accepted += 1;
                        } else {
                            rejected += 1;
                            println!("rejected: {}", v["error"]);
                        }
                        if accepted + rejected >= wanted {
                            println!("done: sent {sent}, accepted {accepted}, rejected {rejected}");
                            return;
                        }
                    } else if v["id"] == 2 && v["result"] != true {
                        panic!("authorize refused: {}", v["error"]);
                    }
                }
                Err(_) => break, // timeout: nothing more for now
            }
        }
        if sent >= wanted {
            continue;
        }
        let Some(j) = &job else { continue };
        en2_counter += 1;
        let en2 = en2_counter.to_le_bytes();
        let mut cb = j.coinb1.clone();
        cb.extend_from_slice(&en1);
        cb.extend_from_slice(&en2);
        cb.extend_from_slice(&j.coinb2);
        let mut nonce = [0u8; 32];
        nonce[0] = 2;
        let hdr0 = header(j.version, &j.prev, &root_from_branch(&cb, &j.branch), j.ntime, j.bits, &nonce);
        let hash = pow_hash(&hdr0);
        let kmin = stella::min_share_k(j.patterns[0].len());
        // n = target + 1 + poff: target is even, so even poff gives odd n.
        for _ in 0..20_000 {
            nonce[2..10].copy_from_slice(&poff.to_le_bytes());
            poff += 2;
            let Ok(c) = stella::candidate(&hash, j.bits, &nonce, 0) else { break };
            if stella::share_k(&c.n, &j.patterns) >= kmin {
                send(json!({"id": next_id, "method": "mining.submit",
                            "params": [login, j.id, hex(&en2), format!("{:016x}", j.ntime), to_display(&nonce)]}));
                next_id += 1;
                sent += 1;
                std::thread::sleep(Duration::from_millis(pause));
                break;
            }
        }
    }
}
