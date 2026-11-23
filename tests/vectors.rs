//! Test vectors for the Stella check and block parts:
//! - real mainnet blocks from our node (block id, merkle root, PoW: a full septuplet, k = 7),
//! - Riecoin Core's genesis blocks (chainparams.cpp: PoW hash and block id) and pow_tests.cpp's refusals,
//! - real rieMiner shares captured with tools/capture_shares.py (TestServer.cpp's jobs), whose k was computed
//!   by that script's separate Python implementation.

use num_bigint::BigUint;
use num_traits::One;
use riecoin_pool::chain::*;
use riecoin_pool::common::{from_display, hex, to_display, unhex};
use riecoin_pool::stella::{self, Bad};
use serde_json::Value;

const MAINNET_MIN: u32 = 600 * 256;
const REGTEST_MIN: u32 = 288 * 256;

fn mainnet_patterns() -> Vec<Vec<u32>> {
    vec![vec![0, 2, 4, 2, 4, 6, 2], vec![0, 2, 6, 4, 2, 4, 2]]
}

fn data(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut v = a.to_vec();
    v.extend_from_slice(b);
    sha256d(&v)
}

#[test]
fn mainnet_blocks() {
    let mut n = 0;
    for line in data("mainnet-blocks.jsonl").lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        let raw = unhex(v["hex"].as_str().unwrap()).unwrap();
        let hdr: [u8; 112] = raw[..112].try_into().unwrap();
        // Block id.
        let mut id = block_id(&hdr);
        id.reverse();
        assert_eq!(hex(&id), v["hash"].as_str().unwrap(), "height {}", v["height"]);
        // Merkle root from the txids, and from the coinbase's stratum branch.
        let txids: Vec<[u8; 32]> = v["txids"].as_array().unwrap().iter().map(|t| from_display(t.as_str().unwrap()).unwrap()).collect();
        assert_eq!(merkle_root(&txids), hdr[36..68]);
        let root = coinbase_branch(&txids[1..]).iter().fold(txids[0], |h, b| pair(&h, b));
        assert_eq!(root, hdr[36..68]);
        // Rebuilt from its fields, the header is the same.
        let le = |a: usize, b: usize| u64::from_le_bytes({
            let mut x = [0u8; 8];
            x[..b - a].copy_from_slice(&hdr[a..b]);
            x
        });
        let nonce: [u8; 32] = hdr[80..].try_into().unwrap();
        let bits = le(76, 80) as u32;
        assert_eq!(
            header(le(0, 4) as u32, &hdr[4..36].try_into().unwrap(), &hdr[36..68].try_into().unwrap(), le(68, 76), bits, &nonce),
            hdr
        );
        // PoW: a full septuplet (k = 7) and a block; a changed primorial offset is not.
        let c = stella::candidate(&pow_hash(&hdr), bits, &nonce, MAINNET_MIN).unwrap();
        assert!(c.n.bits() > 1100, "about difficulty bits: {}", c.n.bits());
        assert_eq!(stella::share_k(&c.n, &mainnet_patterns()), 7);
        assert!(stella::is_block(&c.n, &mainnet_patterns()));
        let mut bad = nonce;
        bad[2] ^= 2;
        let c2 = stella::candidate(&pow_hash(&hdr), bits, &bad, MAINNET_MIN).unwrap();
        assert!(!stella::is_block(&c2.n, &mainnet_patterns()));
        // The header hash changes the target: the same nOffset on another time is no block.
        let mut moved = hdr;
        moved[68] ^= 1;
        let c3 = stella::candidate(&pow_hash(&moved), bits, &nonce, MAINNET_MIN).unwrap();
        assert!(!stella::is_block(&c3.n, &mainnet_patterns()));
        n += 1;
    }
    assert_eq!(n, 5);
}

/// The genesis blocks of testnet and regtest (chainparams.cpp): header layout, PoW hash, block id and PoW.
#[test]
fn genesis_blocks() {
    let merkle = from_display("495297a63256ff66e6bb810adc1660eee7a98eb55dbfeae8e25b1365b8bacca6").unwrap();
    for (nonce, bits, pow, id, patterns) in [
        (
            "00000000000000000000000000000000000000000000002990adb3a701960002",
            512 * 256,
            "d38d558bf81079c5c1662f6645dfa9856bcda0f54c93c5ca3788a59c7cfcc734",
            "753b93f5e3938f69d2b33c8c7572b019b12fa877e78581eebd65d349ca8645da",
            vec![vec![0, 4, 2, 4, 2], vec![0, 2, 4, 2, 4]],
        ),
        (
            "00000000000000000000000000000000000000000000000000000000001a0002",
            288 * 256,
            "e450cfcfbf053cbba2c70088cbe95a5bb4133665126028dd916a553dbf49d94a",
            "08982e71e300f2c7f5b967df5e9b40788942abd4bc62edaeabd27d351f953b68",
            vec![vec![0]],
        ),
    ] {
        let nonce = from_display(nonce).unwrap();
        let hdr = header(536_870_912, &[0; 32], &merkle, 1_707_684_554, bits, &nonce);
        assert_eq!(to_display(&pow_hash(&hdr)), pow);
        assert_eq!(to_display(&block_id(&hdr)), id);
        let c = stella::candidate(&pow_hash(&hdr), bits, &nonce, bits).unwrap();
        assert!(stella::is_block(&c.n, &patterns), "genesis {id} is a constellation");
    }
}

/// pow_tests.cpp (regtest): too easy nBits, offset too large, zero nBits; and the offset arithmetic itself.
#[test]
fn core_pow_tests() {
    let zero = [0u8; 32];
    let o = |s: &str| from_display(&format!("{s:0>64}")).unwrap();
    assert_eq!(stella::candidate(&zero, 287 * 256, &o("220002"), REGTEST_MIN).unwrap_err(), Bad::Bits);
    assert_eq!(stella::candidate(&zero, 288 * 256, &o("0100003a0002"), REGTEST_MIN).unwrap_err(), Bad::Offset);
    assert_eq!(stella::candidate(&zero, 0, &o("010002"), REGTEST_MIN).unwrap_err(), Bad::Bits);
    // With nBits 288: target 2^288 (hash 0, df 0), offset 0x22 + 1 = 35 ("2^287 + 35" in the test is for 287).
    let c = stella::candidate(&zero, 288 * 256, &o("220002"), REGTEST_MIN).unwrap();
    assert_eq!(c.trailing_zeros, 24);
    assert_eq!(c.n, (BigUint::one() << 288usize) + 35u32);
    // offset = 1 + primorial offset (primorial 1): 0x100003a gives 16777275 >= 2^24 (refused above), 0xfffffe the largest.
    assert_eq!(stella::candidate(&zero, 288 * 256, &o("fffffe0002"), REGTEST_MIN).unwrap().n, (BigUint::one() << 288usize) + 0xffffffu32);
    assert_eq!(stella::candidate(&zero, 288 * 256, &o("ffffff0002"), REGTEST_MIN).unwrap_err(), Bad::Offset);
    // Version bits: odd (legacy) and anything but 2 in the low 5 bits.
    assert_eq!(stella::candidate(&zero, 288 * 256, &o("220001"), REGTEST_MIN).unwrap_err(), Bad::Version);
    assert_eq!(stella::candidate(&zero, 288 * 256, &o("220004"), REGTEST_MIN).unwrap_err(), Bad::Version);
    // A difficulty offset (low 16 bits 2 + 32 d) adds d * 8192 to nBits: 32 more trailing zeros per step.
    let c = stella::candidate(&zero, 288 * 256, &o("220022"), REGTEST_MIN).unwrap();
    assert_eq!(c.trailing_zeros, 24 + 32);
    // A primorial larger than 2^tz: the first 10 primes multiply to 6469693230 > 2^24.
    assert_eq!(stella::candidate(&zero, 288 * 256, &o(&format!("000a{}02", "0".repeat(58))), REGTEST_MIN).unwrap_err(), Bad::Primorial);
}

/// Real rieMiner submissions for TestServer.cpp-style jobs (quintuplets at difficulty 768, septuplets at 540).
#[test]
fn riminer_shares() {
    let v: Value = serde_json::from_str(&data("riminer-shares.json")).unwrap();
    let mut by_k = std::collections::BTreeMap::new();
    for s in v.as_array().unwrap() {
        let notify = s["notify"].as_array().unwrap();
        let submit = s["submit"].as_array().unwrap();
        let st = |a: &Value| a.as_str().unwrap().to_string();
        let mut coinbase = unhex(&st(&notify[2])).unwrap();
        coinbase.extend(unhex(&st(&s["extranonce1"])).unwrap());
        coinbase.extend(unhex(&st(&submit[2])).unwrap());
        coinbase.extend(unhex(&st(&notify[3])).unwrap());
        let branch: Vec<[u8; 32]> = notify[4].as_array().unwrap().iter().map(|b| unhex(b.as_str().unwrap()).unwrap().try_into().unwrap()).collect();
        let prev = swap_words(&unhex(&st(&notify[1])).unwrap().try_into().unwrap());
        let version = u32::from_str_radix(&st(&notify[5]), 16).unwrap();
        let bits = u32::from_str_radix(&st(&notify[6]), 16).unwrap();
        let ntime = u64::from_str_radix(&st(&submit[3]), 16).unwrap();
        assert_eq!(submit[3].as_str().unwrap().len(), 16, "rieMiner sends ntime as 16 hex digits");
        let nonce = from_display(&st(&submit[4])).unwrap();
        let patterns: Vec<Vec<u32>> = serde_json::from_value(notify[10].clone()).unwrap();
        let hdr = header(version, &prev, &root_from_branch(&coinbase, &branch), ntime, bits, &nonce);
        let c = stella::candidate(&pow_hash(&hdr), bits, &nonce, REGTEST_MIN).unwrap();
        let k = stella::share_k(&c.n, &patterns);
        assert_eq!(k as i64, s["k"].as_i64().unwrap());
        assert!(k >= stella::min_share_k(patterns[0].len()));
        assert_eq!(stella::is_block(&c.n, &patterns), k as usize == patterns[0].len());
        *by_k.entry((patterns[0].len(), k)).or_insert(0) += 1;
    }
    // (pattern length, k) -> shares: a spread of k, including full constellations.
    assert!(by_k.contains_key(&(5, 4)) && by_k.contains_key(&(7, 5)) && by_k.contains_key(&(7, 6)));
    assert!(by_k.contains_key(&(5, 5)) || by_k.contains_key(&(7, 7)));
}
