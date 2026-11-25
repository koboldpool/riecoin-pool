//! riecoin-pool: Kobold Pool's Riecoin (RIC) pool. Bitcoin stratum v1 as rieMiner speaks it (notify params 9 =
//! PoW version 1 and 10 = accepted constellation patterns; submit [login, job, extraNonce2, ntime, nOffset]), the
//! Stella k-share check in Rust, getblocktemplate/submitblock against riecoind. Same data files as the other
//! Kobold Pool engines (shares.jsonl, blocks.jsonl, stats.json, pool-config.json), so the site and payout.py read them.
//!
//!   -> mining.subscribe ["rieMiner/2507"]        <- [[["mining.notify", id]], extraNonce1 (4 bytes), 4]
//!   -> mining.authorize ["ric1q....worker", "x"] <- true
//!   <- mining.notify [job, prevhash, coinb1, coinb2, [branch], version, nbits, ntime, clean, 1, [[0, 2, 4, 2, 4, 6, 2], ...]]
//!   -> mining.submit [login, job, extraNonce2 (8 hex), ntime (16 hex), nOffset (64 hex, most significant first)]
//!
//! A share is an n with its pattern's first two numbers prime and at least k of them prime (k = pattern length - 2,
//! at least 4: 5 of 7 on mainnet). Every share counts the same (1 unit of work); a full constellation is a block.
//! Expected shares per block are estimated from the share counts (how often a share has more than k primes).

use riecoin_pool::chain::{EN2_SIZE, block_id, coinbase_branch, coinbase_parts, compact_size, header, pow_hash, root_from_branch, swap_words};
use riecoin_pool::common::*;
use riecoin_pool::rpc::Rpc;
use riecoin_pool::stella::{self, Bad};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_LINE: usize = 16_384;
/// Templates (jobs) kept for late submits.
const KEEP_JOBS: usize = 16;
const MAX_SUBMITS_PER_SEC: usize = 25;
const MAX_BAD_IN_A_ROW: u32 = 30;
const HASHRATE_WINDOW: Duration = Duration::from_secs(1800);
/// A fresh template this often even without a new block (new transactions, a newer time).
const REFRESH: Duration = Duration::from_secs(30);
/// The node refuses blocks more than 15 s ahead (MAX_FUTURE_BLOCK_TIME); keep a margin.
const MAX_FUTURE: u64 = 10;
/// Weight, in shares, of the --prime-prob prior in the share-model estimate.
const PRIOR_SHARES: f64 = 200.0;

struct Config {
    network: String,
    listen: String,
    /// Every login on this port mines solo.
    listen_solo: String,
    rpc: String,
    cookie: PathBuf,
    /// The pool wallet's address: every block's coinbase pays it.
    address: String,
    data: PathBuf,
    name: String,
    tag: String,
    /// No work while the node's tip is older than this (seconds; 0 = no limit): still syncing.
    max_tip_age: u64,
    /// No work with fewer peers than this (default 1 on mainnet, 0 elsewhere): never mine a private fork.
    min_peers: usize,
    /// Prior for the chance that a number of a share's pattern (after the first two) is prime.
    prime_prob: f64,
}

fn config() -> Config {
    let mut c = Config {
        network: "mainnet".into(),
        listen: "0.0.0.0:3540".into(),
        listen_solo: "0.0.0.0:3550".into(),
        rpc: String::new(),
        cookie: PathBuf::new(),
        address: String::new(),
        data: "pool-data".into(),
        name: "riecoin-pool".into(),
        tag: "/KoboldPool/".into(),
        max_tip_age: 86_400,
        min_peers: usize::MAX, // set below from the network
        prime_prob: 0.05,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--network" => c.network = v,
            "--listen" => c.listen = v,
            "--listen-solo" => c.listen_solo = v,
            "--rpc" => c.rpc = v,
            "--cookie" => c.cookie = v.into(),
            "--address" => c.address = v,
            "--data" => c.data = v.into(),
            "--name" => c.name = v,
            "--tag" => c.tag = v,
            "--max-tip-age" => c.max_tip_age = v.parse().expect("--max-tip-age seconds"),
            "--min-peers" => c.min_peers = v.parse().expect("--min-peers N"),
            "--prime-prob" => c.prime_prob = v.parse().expect("--prime-prob 0..1"),
            "--help" | "-h" => {
                println!(
                    "riecoin-pool --address <pool wallet address> --cookie <riecoind datadir>/.cookie [--network mainnet|testnet|regtest]\n  \
                     [--rpc 127.0.0.1:28332] [--listen 0.0.0.0:3540] [--listen-solo 0.0.0.0:3550|off] [--data pool-data]\n  \
                     [--name riecoin-pool] [--tag /KoboldPool/] [--max-tip-age 86400] [--min-peers 1 on mainnet, else 0] [--prime-prob 0.05]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown option {other}"),
        }
        i += 2;
    }
    if c.rpc.is_empty() {
        c.rpc = match c.network.as_str() {
            "mainnet" => "127.0.0.1:28332",
            "testnet" => "127.0.0.1:38332",
            "regtest" => "127.0.0.1:18443",
            other => panic!("--network mainnet|testnet|regtest, not {other}"),
        }
        .into();
    }
    if c.min_peers == usize::MAX {
        c.min_peers = if c.network == "mainnet" { 1 } else { 0 };
    }
    assert!(c.tag.len() <= 64, "--tag at most 64 bytes");
    assert!(c.prime_prob > 0.0 && c.prime_prob < 1.0, "--prime-prob between 0 and 1");
    c
}

/// A block template, ready for stratum: one job, shared by every session.
struct Template {
    id: String,
    height: u64,
    /// The parent, raw byte order (as in the header).
    prev: [u8; 32],
    version: u32,
    bits: u32,
    curtime: u64,
    /// The median time past of the tip, + 1.
    mintime: u64,
    patterns: Vec<Vec<u32>>,
    coinb1: Vec<u8>,
    coinb2: Vec<u8>,
    branch: Vec<[u8; 32]>,
    txs: Vec<Vec<u8>>,
    created: Instant,
}

impl Template {
    fn kmin(&self) -> u32 {
        stella::min_share_k(self.patterns[0].len())
    }

    fn coinbase(&self, en1: &[u8], en2: &[u8]) -> Vec<u8> {
        let mut cb = self.coinb1.clone();
        cb.extend_from_slice(en1);
        cb.extend_from_slice(en2);
        cb.extend_from_slice(&self.coinb2);
        cb
    }

    fn block(&self, header: &[u8; 112], coinbase: &[u8]) -> Vec<u8> {
        let mut b = header.to_vec();
        compact_size(1 + self.txs.len() as u64, &mut b);
        b.extend_from_slice(coinbase);
        for t in &self.txs {
            b.extend_from_slice(t);
        }
        b
    }

    fn notify(&self, clean: bool) -> Value {
        json!([
            self.id,
            hex(&swap_words(&self.prev)),
            hex(&self.coinb1),
            hex(&self.coinb2),
            self.branch.iter().map(|b| hex(b)).collect::<Vec<_>>(),
            format!("{:08x}", self.version),
            format!("{:08x}", self.bits),
            format!("{:016x}", self.curtime),
            clean,
            1,
            self.patterns
        ])
    }
}

fn parse_template(v: &Value, id: String, tag: &[u8], pool_script: &[u8]) -> Result<Template, String> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).ok_or(format!("template without {k}"));
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).ok_or(format!("template without {k}"));
    if v.get("powversion").and_then(|x| x.as_i64()) != Some(1) {
        return Err(format!("unsupported powversion {:?}", v.get("powversion")));
    }
    let patterns: Vec<Vec<u32>> = serde_json::from_value(v.get("patterns").cloned().unwrap_or_default()).map_err(|e| format!("patterns: {e}"))?;
    if patterns.is_empty() || patterns.iter().any(|p| p.is_empty()) {
        return Err("no constellation patterns".into());
    }
    let height = n("height")?;
    let mut txs = Vec::new();
    let mut txids = Vec::new();
    for t in v.get("transactions").and_then(|x| x.as_array()).ok_or("no transactions")? {
        let data = unhex(t.get("data").and_then(|x| x.as_str()).ok_or("tx without data")?).ok_or("bad tx hex")?;
        // Cross-check the node's wtxid ("hash") against the data; the txid goes into the merkle tree.
        if t.get("hash").and_then(|x| x.as_str()).and_then(from_display) != Some(riecoin_pool::chain::sha256d(&data)) {
            return Err("transaction hash does not match its data".into());
        }
        txids.push(t.get("txid").and_then(|x| x.as_str()).and_then(from_display).ok_or("tx without txid")?);
        txs.push(data);
    }
    let commitment = match v.get("default_witness_commitment").and_then(|x| x.as_str()) {
        Some(h) => Some(unhex(h).ok_or("bad witness commitment")?),
        None => None,
    };
    let (coinb1, coinb2) = coinbase_parts(height, tag, n("coinbasevalue")?, pool_script, commitment.as_deref());
    Ok(Template {
        id,
        height,
        prev: from_display(s("previousblockhash")?).ok_or("bad previousblockhash")?,
        version: n("version")? as u32,
        bits: u32::from_str_radix(s("bits")?, 16).map_err(|_| "bad bits")?,
        curtime: n("curtime")?,
        mintime: n("mintime")?,
        patterns,
        coinb1,
        coinb2,
        branch: coinbase_branch(&txids),
        txs,
        created: Instant::now(),
    })
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    rejected: AtomicU64,
    stale: AtomicU64,
    blocks_submitted: AtomicU64,
    blocks_accepted: AtomicU64,
}

struct Shared {
    cfg: Config,
    rpc: Rpc,
    pool_script: Vec<u8>,
    nbits_min: u32,
    jobs: Mutex<VecDeque<Arc<Template>>>,
    sessions: Mutex<Vec<Arc<Session>>>,
    /// Recent accepted shares for the hash rate: (time, address, worker, work, solo).
    recent: Mutex<VecDeque<(Instant, String, String, f64, bool)>>,
    /// Accepted shares by k since the start (for the share model).
    k_counts: Mutex<HashMap<u32, u64>>,
    /// Login -> valid address of the node's network (validateaddress), cached.
    addr_ok: Mutex<HashMap<String, bool>>,
    next_session: AtomicU64,
    next_job: AtomicU64,
    next_en1: AtomicU32,
    node_up: AtomicBool,
    peers: AtomicUsize,
    refresh_now: AtomicBool,
    /// The parent of the pool's last block that became the tip: jobs on it are stale at once.
    solved_parent: Mutex<Option<[u8; 32]>>,
    /// Fees in basis points (100 = 1 %), from <data>/pool-config.json, re-read every 10 s.
    fee_bp: AtomicU32,
    solo_fee_bp: AtomicU32,
    /// Share work since the pool's last block: effort = round work / block work.
    round_work: Mutex<f64>,
    counters: Counters,
    files: Mutex<(File, File, File, File)>, // shares.jsonl, blocks.jsonl, pool.log, rejects.jsonl
    started: u64,
}

impl Shared {
    fn log(&self, msg: &str) {
        let line = format!("{} {msg}\n", now());
        print!("{line}");
        if let Ok(mut f) = self.files.lock() {
            let _ = f.2.write_all(line.as_bytes());
        }
    }

    fn record(&self, which: usize, v: Value) {
        if let Ok(mut f) = self.files.lock() {
            let file = match which {
                0 => &mut f.0,
                1 => &mut f.1,
                _ => &mut f.3, // REJECTS
            };
            let _ = writeln!(file, "{v}");
        }
    }

    fn current(&self) -> Option<Arc<Template>> {
        self.jobs.lock().unwrap().back().cloned()
    }

    /// The chance p that a pattern number (after the first two) is prime: the shares seen, with the
    /// --prime-prob prior worth PRIOR_SHARES shares.
    fn prime_prob(&self, t: &Template) -> f64 {
        let len = t.patterns[0].len();
        let kmin = t.kmin();
        let p0 = self.cfg.prime_prob;
        let Some((at0, above0)) = prior(p0, len, kmin) else { return p0 };
        let counts = self.k_counts.lock().unwrap();
        let at = *counts.get(&kmin).unwrap_or(&0) as f64;
        let above: f64 = counts.iter().filter(|(k, _)| **k > kmin).map(|(_, c)| *c as f64).sum();
        stella::estimate_p(at + PRIOR_SHARES * at0, above + PRIOR_SHARES * above0, len, kmin).unwrap_or(p0)
    }

    /// Expected shares per block for the current template.
    fn block_work(&self, t: &Template) -> f64 {
        stella::shares_per_block(self.prime_prob(t), t.patterns[0].len(), t.kmin())
    }
}

/// Share fractions (exactly kmin, more than kmin) under the model at p; None when the model has no spread.
fn prior(p: f64, len: usize, kmin: u32) -> Option<(f64, f64)> {
    let m = len.saturating_sub(2) as i32;
    let j0 = kmin.saturating_sub(2) as i32;
    if m <= 0 || j0 >= m {
        return None;
    }
    let b = |j: i32| (0..j).fold(1.0, |a, i| a * (m - i) as f64 / (i + 1) as f64) * p.powi(j) * (1.0 - p).powi(m - j);
    let at = b(j0);
    let above: f64 = (j0 + 1..=m).map(b).sum();
    Some((at / (at + above), above / (at + above)))
}

struct SessState {
    subscribed: bool,
    en1: [u8; 4],
    logins: HashSet<String>,
    /// Logins that mine in solo mode (authorized with a password containing "solo").
    solo: HashSet<String>,
    second: (Instant, usize),
    bad_in_a_row: u32,
    /// Shares seen: (job, extraNonce2, ntime, nOffset).
    seen: HashSet<(String, [u8; 4], u64, [u8; 32])>,
}

struct Session {
    id: u64,
    peer: String,
    solo_port: bool,
    out: Mutex<TcpStream>,
    st: Mutex<SessState>,
    alive: AtomicBool,
}

impl Session {
    fn send(&self, v: &Value) {
        let mut line = v.to_string();
        line.push('\n');
        let ok = self.out.lock().map(|mut s| s.write_all(line.as_bytes()).is_ok()).unwrap_or(false);
        if !ok {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"id": null, "method": method, "params": params}));
    }
}

// ------------------------------------------------------------------------------------- node link

/// Polls the node: a new template when the tip changes, after a found block, or every REFRESH.
fn node_loop(sh: Arc<Shared>) {
    let mut last_tip = String::new();
    let mut waiting = false;
    let mut last_peers = Instant::now() - Duration::from_secs(60);
    loop {
        if last_peers.elapsed() >= Duration::from_secs(10) {
            last_peers = Instant::now();
            if let Ok(n) = sh.rpc.call("getconnectioncount", json!([])) {
                sh.peers.store(n.as_u64().unwrap_or(0) as usize, Ordering::Relaxed);
            }
        }
        match sh.rpc.call("getbestblockhash", json!([])) {
            Ok(Value::String(tip)) => {
                if !sh.node_up.swap(true, Ordering::Relaxed) {
                    sh.log(&format!("node RPC {} up", sh.cfg.rpc));
                }
                let stale = sh.current().is_none_or(|t| t.created.elapsed() >= REFRESH);
                let clean = tip != last_tip;
                if clean || stale || sh.refresh_now.swap(false, Ordering::Relaxed) {
                    let id = format!("{:x}", sh.next_job.fetch_add(1, Ordering::Relaxed));
                    match sh
                        .rpc
                        .call("getblocktemplate", json!([{"rules": ["segwit"]}]))
                        .and_then(|v| parse_template(&v, id, sh.cfg.tag.as_bytes(), &sh.pool_script))
                    {
                        Ok(t) => {
                            let age = now().saturating_sub(t.mintime);
                            let peers = sh.peers.load(Ordering::Relaxed);
                            let why = if sh.cfg.max_tip_age > 0 && age > sh.cfg.max_tip_age {
                                Some(format!("the tip is {} h old (still syncing?)", age / 3600))
                            } else if peers < sh.cfg.min_peers {
                                Some(format!("the node has {peers} peers"))
                            } else {
                                None
                            };
                            if let Some(why) = why {
                                if !waiting {
                                    sh.log(&format!("no work for miners: {why}; waiting"));
                                }
                                sh.jobs.lock().unwrap().clear();
                                waiting = true;
                                last_tip.clear();
                            } else {
                                if waiting {
                                    sh.log("node ready: handing out work");
                                }
                                waiting = false;
                                last_tip = tip;
                                install(&sh, t, clean);
                            }
                        }
                        Err(e) => {
                            if !waiting {
                                sh.log(&format!("getblocktemplate failed: {e}"));
                            }
                            sh.jobs.lock().unwrap().clear();
                            waiting = true;
                            last_tip.clear();
                        }
                    }
                }
            }
            Ok(other) => sh.log(&format!("getbestblockhash: unexpected {other}")),
            Err(e) => {
                if sh.node_up.swap(false, Ordering::Relaxed) {
                    sh.log(&format!("node RPC down: {e}"));
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
        thread::sleep(Duration::from_millis(if waiting { 5000 } else { 500 }));
    }
}

fn install(sh: &Arc<Shared>, t: Template, clean: bool) {
    let t = Arc::new(t);
    {
        let mut jobs = sh.jobs.lock().unwrap();
        jobs.push_back(t.clone());
        while jobs.len() > KEEP_JOBS {
            jobs.pop_front();
        }
    }
    if clean {
        sh.log(&format!(
            "new block template: height {} bits {:08x} (difficulty {:.2}) txs {} patterns {:?}",
            t.height,
            t.bits,
            t.bits as f64 / 256.0,
            t.txs.len(),
            t.patterns
        ));
    }
    let sessions: Vec<Arc<Session>> = sh.sessions.lock().unwrap().clone();
    for s in sessions {
        if s.alive.load(Ordering::Relaxed) {
            push_job(&s, &t, clean);
        }
    }
}

fn push_job(sess: &Session, t: &Template, clean: bool) {
    {
        let mut st = sess.st.lock().unwrap();
        if st.logins.is_empty() {
            return;
        }
        if clean {
            st.seen.clear();
        }
    }
    sess.notify("mining.notify", t.notify(clean));
}

// ------------------------------------------------------------------------------------- miners

/// Which file `Shared::record` writes to: one line per refused share, for our monitoring only (the site never reads it).
const REJECTS: usize = 2;

/// A refused submit (any error answer) as one line in rejects.jsonl.
fn log_reject(sh: &Shared, sess: &Session, p: &[Value], reply: &Value) {
    let Some(e) = reply.get("error").filter(|e| !e.is_null()) else { return };
    let who = p.first().and_then(|v| v.as_str()).unwrap_or("");
    let (addr, worker) = who.split_once('.').unwrap_or((who, ""));
    let reason = e.get(1).and_then(|v| v.as_str()).unwrap_or("");
    sh.record(REJECTS, json!({"ts": now(), "addr": addr, "worker": worker, "ip": sess.peer,
        "reason": reason, "stale": reason.contains("stale")}));
}

fn err(id: &Value, code: u32, msg: &str) -> Value {
    json!({"id": id, "result": null, "error": [code, msg, null]})
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"id": id, "result": result, "error": null})
}

/// "<address>[.worker]": (address, worker), if the node knows the address as one of its network.
fn valid_login(sh: &Shared, login: &str) -> Option<(String, String)> {
    let (addr, worker) = login.split_once('.').unwrap_or((login, ""));
    if worker.len() > 32 || !worker.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    if addr.is_empty() || addr.len() > 90 || !addr.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let known = sh.addr_ok.lock().unwrap().get(addr).copied();
    let valid = match known {
        Some(v) => v,
        None => {
            let v = sh
                .rpc
                .call("validateaddress", json!([addr]))
                .ok()?
                .get("isvalid")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let mut cache = sh.addr_ok.lock().unwrap();
            if cache.len() > 10_000 {
                cache.clear();
            }
            cache.insert(addr.to_string(), v);
            v
        }
    };
    valid.then(|| (addr.to_string(), if worker.is_empty() { "default".into() } else { worker.to_string() }))
}

fn serve_miner(sh: Arc<Shared>, stream: TcpStream, solo_port: bool) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(900)));
    let Ok(out) = stream.try_clone() else { return };
    let id = sh.next_session.fetch_add(1, Ordering::Relaxed);
    let en1 = sh.next_en1.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    let sess = Arc::new(Session {
        id,
        peer: peer.clone(),
        solo_port,
        out: Mutex::new(out),
        alive: AtomicBool::new(true),
        st: Mutex::new(SessState {
            subscribed: false,
            en1,
            logins: HashSet::new(),
            solo: HashSet::new(),
            second: (Instant::now(), 0),
            bad_in_a_row: 0,
            seen: HashSet::new(),
        }),
    });
    sh.sessions.lock().unwrap().push(sess.clone());
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match (&mut reader).take(MAX_LINE as u64 + 1).read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) if n > MAX_LINE => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { break };
        let rid = req.get("id").cloned().unwrap_or(Value::Null);
        let p = req.get("params").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let reply = match method {
            "mining.subscribe" => subscribe(&sh, &sess, &rid),
            "mining.authorize" => authorize(&sh, &sess, &rid, &p),
            "mining.submit" => {
                let r = submit(&sh, &sess, &rid, &p);
                log_reject(&sh, &sess, &p, &r);
                r
            }
            "mining.extranonce.subscribe" => ok(&rid, json!(true)),
            _ => err(&rid, 20, "unknown method"),
        };
        sess.send(&reply);
        if !sess.alive.load(Ordering::Relaxed) {
            break;
        }
        if method == "mining.authorize" && reply.get("result") == Some(&Value::Bool(true)) {
            let first = sess.st.lock().unwrap().logins.len() == 1;
            if first {
                if let Some(t) = sh.current() {
                    push_job(&sess, &t, true);
                }
            }
        }
    }
    sess.alive.store(false, Ordering::Relaxed);
    sh.sessions.lock().unwrap().retain(|s| s.id != sess.id);
    let logins = sess.st.lock().unwrap().logins.iter().cloned().collect::<Vec<_>>().join(",");
    sh.log(&format!("session {} {} closed ({})", sess.id, sess.peer, logins));
}

fn subscribe(sh: &Shared, sess: &Session, rid: &Value) -> Value {
    if sh.current().is_none() {
        return err(rid, 20, "pool has no work from its node yet");
    }
    let mut st = sess.st.lock().unwrap();
    st.subscribed = true;
    let sid = format!("{:x}", sess.id);
    ok(rid, json!([[["mining.notify", sid]], hex(&st.en1), EN2_SIZE]))
}

fn authorize(sh: &Shared, sess: &Session, rid: &Value, p: &[Value]) -> Value {
    if !sess.st.lock().unwrap().subscribed {
        return err(rid, 25, "not subscribed");
    }
    let login = p.first().and_then(|v| v.as_str()).unwrap_or("");
    // Checked outside the session lock: it may ask the node.
    if valid_login(sh, login).is_none() {
        return err(rid, 24, &format!("login is not a valid Riecoin {} address (with an optional .worker)", sh.cfg.network));
    }
    let mut st = sess.st.lock().unwrap();
    if st.logins.len() >= 8 && !st.logins.contains(login) {
        return err(rid, 24, "too many logins in one session");
    }
    st.logins.insert(login.to_string());
    let solo = sess.solo_port || p.get(1).and_then(|v| v.as_str()).is_some_and(|pw| pw.to_ascii_lowercase().contains("solo"));
    if solo {
        st.solo.insert(login.to_string());
    } else {
        st.solo.remove(login);
    }
    drop(st);
    sh.log(&format!("session {} {} authorized {login}{}", sess.id, sess.peer, if solo { " (solo)" } else { "" }));
    ok(rid, json!(true))
}

/// mining.submit [login, job, extraNonce2 (8 hex), ntime (hex, up to 16 digits), nOffset (64 hex, most significant first)].
fn submit(sh: &Arc<Shared>, sess: &Session, rid: &Value, p: &[Value]) -> Value {
    let s = |i: usize| p.get(i).and_then(|v| v.as_str()).unwrap_or("");
    let login = s(0).to_string();
    let job_id = s(1).to_string();
    let en2: Option<[u8; 4]> = unhex(s(2)).and_then(|b| b.try_into().ok());
    let ntime = (!s(3).is_empty() && s(3).len() <= 16).then(|| u64::from_str_radix(s(3), 16).ok()).flatten();
    let nonce = from_display(s(4));
    let (Some(en2), Some(ntime), Some(nonce)) = (en2, ntime, nonce) else {
        return err(rid, 20, "bad params: [login, job, extranonce2, ntime, nOffset]");
    };
    let current = sh.current();
    let solved = *sh.solved_parent.lock().unwrap();
    let job = sh.jobs.lock().unwrap().iter().rev().find(|j| j.id == job_id).cloned();
    let (hdr, coinbase, job, solo) = {
        let mut st = sess.st.lock().unwrap();
        if !st.subscribed {
            return err(rid, 25, "not subscribed");
        }
        if !st.logins.contains(&login) {
            return err(rid, 24, "login not authorized");
        }
        let now_i = Instant::now();
        if now_i.duration_since(st.second.0) >= Duration::from_secs(1) {
            st.second = (now_i, 0);
        }
        st.second.1 += 1;
        if st.second.1 > MAX_SUBMITS_PER_SEC {
            sess.alive.store(false, Ordering::Relaxed);
            return err(rid, 20, "too many submits");
        }
        let solo = st.solo.contains(&login);
        let Some(job) = job else { return err(rid, 21, "job not found") };
        if current.as_ref().is_none_or(|c| c.prev != job.prev) || solved == Some(job.prev) {
            sh.counters.stale.fetch_add(1, Ordering::Relaxed);
            return err(rid, 21, "stale job: the chain has a new block");
        }
        if ntime < job.mintime || ntime > now() + MAX_FUTURE {
            return err(rid, 20, "ntime out of range (check the clock)");
        }
        if !st.seen.insert((job_id.clone(), en2, ntime, nonce)) {
            return err(rid, 22, "duplicate share");
        }
        let coinbase = job.coinbase(&st.en1, &en2);
        let root = root_from_branch(&coinbase, &job.branch);
        (header(job.version, &job.prev, &root, ntime, job.bits, &nonce), coinbase, job, solo)
    };
    // The prime tests run outside the session lock.
    let hash = pow_hash(&hdr);
    let kmin = job.kmin();
    let checked = stella::candidate(&hash, job.bits, &nonce, sh.nbits_min).map(|c| (stella::share_k(&c.n, &job.patterns), c));
    let (k, cand) = match checked {
        Ok((k, c)) if k >= kmin => (k, c),
        other => {
            sh.counters.rejected.fetch_add(1, Ordering::Relaxed);
            let mut st = sess.st.lock().unwrap();
            st.bad_in_a_row += 1;
            if st.bad_in_a_row > MAX_BAD_IN_A_ROW {
                sess.alive.store(false, Ordering::Relaxed);
            }
            let why = match other {
                Err(Bad::Version) => "nOffset is not PoW version 1".to_string(),
                Err(Bad::Bits) => "nBits below the minimum".to_string(),
                Err(Bad::Primorial) => "primorial too large".to_string(),
                Err(Bad::Offset) => "offset too large".to_string(),
                Ok((k, _)) => format!("{k}-share, {kmin}-shares or better expected"),
            };
            return err(rid, 23, &why);
        }
    };
    sess.st.lock().unwrap().bad_in_a_row = 0;
    let block = k as usize >= job.patterns.iter().map(|p| p.len()).min().unwrap_or(usize::MAX) && stella::is_block(&cand.n, &job.patterns);
    let raw = block.then(|| job.block(&hdr, &coinbase));
    credit(sh, &login, solo, &job, &hdr, k, raw);
    ok(rid, json!(true))
}

/// Counts an accepted share (shares.jsonl, hash rate, effort) and, for a block, sends `raw` to the node and
/// records the found/result events.
fn credit(sh: &Arc<Shared>, login: &str, solo: bool, job: &Template, hdr: &[u8; 112], k: u32, raw: Option<Vec<u8>>) {
    let work = 1.0;
    let block = raw.is_some();
    let block_work = sh.block_work(job);
    let (addr, worker) = valid_login(sh, login).unwrap_or_default();
    sh.counters.accepted.fetch_add(1, Ordering::Relaxed);
    *sh.k_counts.lock().unwrap().entry(k).or_default() += 1;
    let mode = if solo { "solo" } else { "pplns" };
    sh.record(0, json!({"ts": now(), "addr": addr, "worker": worker, "work": work, "height": job.height,
                        "block": block, "mode": mode, "k": k}));
    sh.recent.lock().unwrap().push_back((Instant::now(), addr.clone(), worker.clone(), work, solo));
    if k > job.kmin() {
        sh.log(&format!("{k}-share from {login} at height {}", job.height));
    }
    let effort = {
        let mut rw = sh.round_work.lock().unwrap();
        *rw += work;
        let e = *rw / block_work;
        if block {
            *rw = 0.0;
        }
        e
    };
    let Some(raw) = raw else { return };
    let mut id = block_id(hdr);
    id.reverse();
    let id = hex(&id);
    sh.counters.blocks_submitted.fetch_add(1, Ordering::Relaxed);
    sh.log(&format!("BLOCK found by {login} at height {} id {id}", job.height));
    let mut prev = job.prev;
    prev.reverse();
    let mut pow = pow_hash(hdr);
    pow.reverse();
    sh.record(1, json!({"ts": now(), "event": "found", "height": job.height, "id": id, "parent": hex(&prev),
                        "finder": login, "sent": true, "block_work": block_work, "mode": mode,
                        "effort": effort, "hash": hex(&pow),
                        "fee_bp": if solo { sh.solo_fee_bp.load(Ordering::Relaxed) } else { sh.fee_bp.load(Ordering::Relaxed) }}));
    let (accepted, reason) = match sh.rpc.call("submitblock", json!([hex(&raw)])) {
        Ok(Value::Null) => (true, "accepted".to_string()),
        // Valid but not on the best chain (another block at this height came first), or already known.
        Ok(Value::String(r)) if r == "inconclusive" || r == "duplicate" => (true, r),
        Ok(Value::String(r)) => (false, r),
        Ok(other) => (false, other.to_string()),
        Err(e) => (false, e),
    };
    let tip = accepted && sh.rpc.call("getbestblockhash", json!([])).ok().and_then(|v| v.as_str().map(|s| s == id)).unwrap_or(false);
    if accepted {
        sh.counters.blocks_accepted.fetch_add(1, Ordering::Relaxed);
    }
    if tip {
        *sh.solved_parent.lock().unwrap() = Some(job.prev);
    }
    sh.log(&format!(
        "node {} block {id} at height {} ({reason}{})",
        if accepted { "ACCEPTED" } else { "refused" },
        job.height,
        if accepted && !tip { ", not the tip" } else { "" }
    ));
    sh.record(1, json!({"ts": now(), "event": "result", "height": job.height, "id": id, "finder": login,
                       "accepted": accepted, "tip": tip, "code": if accepted { 0 } else { 1 }, "reason": reason}));
    sh.refresh_now.store(true, Ordering::Relaxed);
}

// ------------------------------------------------------------------------------------- stats

/// Reads the fees from <data>/pool-config.json: {"fee_percent": 0.0, "solo_fee_percent": 0.0}.
/// A missing file is created with 0 % for both. A broken file or out-of-range value keeps the
/// current fees and is logged, so a typo never changes the fee by accident.
fn load_fees(sh: &Shared) {
    let path = sh.cfg.data.join("pool-config.json");
    if !path.exists() {
        let _ = std::fs::write(&path, "{\n  \"fee_percent\": 0.0,\n  \"solo_fee_percent\": 0.0\n}\n");
    }
    let parsed = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let Some(cfg) = parsed else {
        sh.log("pool-config.json does not parse; fees unchanged");
        return;
    };
    for (key, slot) in [("fee_percent", &sh.fee_bp), ("solo_fee_percent", &sh.solo_fee_bp)] {
        let Some(pct) = cfg.get(key).and_then(|v| v.as_f64()) else { continue };
        if !(0.0..=10.0).contains(&pct) {
            sh.log(&format!("pool-config.json: {key} {pct} outside 0..10 %; unchanged"));
            continue;
        }
        let bp = (pct * 100.0).round() as u32;
        let old = slot.swap(bp, Ordering::Relaxed);
        if old != bp {
            sh.log(&format!("{key} is now {:.2} % (was {:.2} %)", bp as f64 / 100.0, old as f64 / 100.0));
        }
    }
}

fn stats_loop(sh: Arc<Shared>) {
    loop {
        thread::sleep(Duration::from_secs(10));
        load_fees(&sh);
        let mut by_addr: HashMap<String, (f64, HashMap<String, f64>, bool)> = HashMap::new();
        let total = {
            let mut recent = sh.recent.lock().unwrap();
            while recent.front().is_some_and(|r| r.0.elapsed() > HASHRATE_WINDOW) {
                recent.pop_front();
            }
            let mut total = 0.0;
            for (_, a, w, work, solo) in recent.iter() {
                total += work;
                let e = by_addr.entry(a.clone()).or_default();
                e.0 += work;
                *e.1.entry(w.clone()).or_default() += work;
                e.2 |= *solo;
            }
            total
        };
        // Until the window has filled, divide by the time the pool has run, not the whole window.
        let secs = HASHRATE_WINDOW.as_secs_f64().min((now() - sh.started).max(10) as f64);
        let t = sh.current();
        let miners: Vec<Value> = by_addr
            .iter()
            .map(|(a, (w, ws, solo))| {
                json!({"addr": a, "hashrate": w / secs, "solo": solo,
                       "workers": ws.iter().map(|(n, x)| json!({"name": n, "hashrate": x / secs})).collect::<Vec<_>>()})
            })
            .collect();
        let peers = sh.peers.load(Ordering::Relaxed);
        let node_up = sh.node_up.load(Ordering::Relaxed);
        let block_work = t.as_ref().map(|t| sh.block_work(t));
        let mut k_counts: Vec<(u32, u64)> = sh.k_counts.lock().unwrap().iter().map(|(k, c)| (*k, *c)).collect();
        k_counts.sort();
        let st = json!({
            "updated": now(), "started": sh.started, "name": sh.cfg.name, "network": sh.cfg.network,
            "node_online": node_up, "peer_online": node_up && peers > 0, "peers": peers,
            "sessions": sh.sessions.lock().unwrap().len(),
            "hashrate": total / secs, "unit": "shares/s",
            "fee_percent": sh.fee_bp.load(Ordering::Relaxed) as f64 / 100.0,
            "solo_fee_percent": sh.solo_fee_bp.load(Ordering::Relaxed) as f64 / 100.0,
            "height": t.as_ref().map(|t| t.height), "bits": t.as_ref().map(|t| format!("{:08x}", t.bits)),
            "difficulty": t.as_ref().map(|t| t.bits as f64 / 256.0),
            "patterns": t.as_ref().map(|t| t.patterns.clone()),
            "share_k": t.as_ref().map(|t| t.kmin()),
            "prime_prob": t.as_ref().map(|t| sh.prime_prob(t)),
            "k_counts": k_counts.iter().map(|(k, c)| (k.to_string(), json!(c))).collect::<serde_json::Map<_, _>>(),
            "network_work_per_block": block_work,
            "round_effort": block_work.map(|w| *sh.round_work.lock().unwrap() / w),
            "accepted": sh.counters.accepted.load(Ordering::Relaxed),
            "rejected": sh.counters.rejected.load(Ordering::Relaxed),
            "stale": sh.counters.stale.load(Ordering::Relaxed),
            "blocks_submitted": sh.counters.blocks_submitted.load(Ordering::Relaxed),
            "blocks_accepted": sh.counters.blocks_accepted.load(Ordering::Relaxed),
            "miners": miners,
            "job_age_s": t.as_ref().map(|t| t.created.elapsed().as_secs()),
        });
        let tmp = sh.cfg.data.join("stats.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&st).unwrap()).is_ok() {
            let _ = std::fs::rename(&tmp, sh.cfg.data.join("stats.json"));
        }
    }
}

/// The round's work so far, rebuilt from the share log so a restart does not reset the round effort: the work of
/// every share after the last block share (the same rule the live count uses).
fn round_work_from_log(path: &std::path::Path) -> f64 {
    let Ok(f) = File::open(path) else { return 0.0 };
    let mut work = 0.0;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        work += v.get("work").and_then(Value::as_f64).unwrap_or(0.0);
        if v.get("block").and_then(Value::as_bool) == Some(true) {
            work = 0.0;
        }
    }
    work
}

fn main() {
    let cfg = config();
    assert!(!cfg.cookie.as_os_str().is_empty(), "--cookie <riecoind datadir>/.cookie is required");
    std::fs::create_dir_all(&cfg.data).expect("create the data directory");
    let open = |n: &str| OpenOptions::new().create(true).append(true).open(cfg.data.join(n)).expect("open log file");
    let files = (open("shares.jsonl"), open("blocks.jsonl"), open("pool.log"), open("rejects.jsonl"));
    let rpc = Rpc { addr: cfg.rpc.clone(), cookie: cfg.cookie.clone(), timeout: Duration::from_secs(30) };
    // The pool address must be valid on the node's network; its script goes into every coinbase.
    let pool_script = loop {
        match rpc.call("validateaddress", json!([cfg.address])) {
            Ok(v) if v.get("isvalid").and_then(|x| x.as_bool()) == Some(true) => {
                break unhex(v.get("scriptPubKey").and_then(|x| x.as_str()).unwrap_or("")).expect("scriptPubKey");
            }
            Ok(_) => panic!("--address {} is not a valid address on the node's network", cfg.address),
            Err(e) => {
                eprintln!("waiting for the node to check the pool address: {e}");
                thread::sleep(Duration::from_secs(10));
            }
        }
    };
    let nbits_min = match cfg.network.as_str() {
        "mainnet" => 600 * 256,
        "testnet" => 512 * 256,
        _ => 288 * 256,
    };
    let seed = u32::from_le_bytes(riecoin_pool::chain::sha256d(&now().to_le_bytes())[..4].try_into().unwrap()) ^ std::process::id();
    let sh = Arc::new(Shared {
        rpc,
        pool_script,
        nbits_min,
        jobs: Mutex::new(VecDeque::new()),
        sessions: Mutex::new(Vec::new()),
        recent: Mutex::new(VecDeque::new()),
        k_counts: Mutex::new(HashMap::new()),
        addr_ok: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
        next_job: AtomicU64::new(1),
        next_en1: AtomicU32::new(seed),
        node_up: AtomicBool::new(false),
        peers: AtomicUsize::new(0),
        refresh_now: AtomicBool::new(false),
        solved_parent: Mutex::new(None),
        fee_bp: AtomicU32::new(0),
        solo_fee_bp: AtomicU32::new(0),
        round_work: Mutex::new(round_work_from_log(&cfg.data.join("shares.jsonl"))),
        counters: Counters::default(),
        files: Mutex::new(files),
        started: now(),
        cfg,
    });
    load_fees(&sh);
    sh.log(&format!(
        "{} {} starting: network {}, node RPC {}, coinbase pays {}",
        sh.cfg.name,
        env!("CARGO_PKG_VERSION"),
        sh.cfg.network,
        sh.cfg.rpc,
        sh.cfg.address
    ));
    {
        let s = sh.clone();
        thread::spawn(move || node_loop(s));
    }
    {
        let s = sh.clone();
        thread::spawn(move || stats_loop(s));
    }
    let accept = |addr: String, solo: bool, sh: Arc<Shared>| {
        let listener = TcpListener::bind(&addr).unwrap_or_else(|e| panic!("listen {addr}: {e}"));
        sh.log(&format!("stratum listening on {addr} ({})", if solo { "solo" } else { "PPLNS" }));
        for conn in listener.incoming() {
            match conn {
                Ok(s) => {
                    let sh2 = sh.clone();
                    thread::spawn(move || serve_miner(sh2, s, solo));
                }
                Err(e) => sh.log(&format!("accept failed: {e}")),
            }
        }
    };
    if sh.cfg.listen_solo != "off" {
        let (addr, sh2) = (sh.cfg.listen_solo.clone(), sh.clone());
        thread::spawn(move || accept(addr, true, sh2));
    }
    accept(sh.cfg.listen.clone(), false, sh.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_work_restarts_after_the_last_block() {
        let path = std::env::temp_dir().join(format!("riecoin-round-work-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "{\"work\":1.0,\"block\":false,\"k\":5}\n",
                "{\"work\":1.0,\"block\":true,\"k\":7}\n",
                "{\"work\":1.0,\"block\":false,\"k\":5}\n",
                "not json\n",
                "{\"work\":1.0,\"block\":false,\"k\":6}\n",
            ),
        )
        .unwrap();
        let work = round_work_from_log(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(work, 2.0);
        assert_eq!(round_work_from_log(&path), 0.0);
    }
}
