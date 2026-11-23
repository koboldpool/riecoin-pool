//! Riecoin's proof of work (PoW version 1, "Stella") as the pool needs it, written from the consensus rules in
//! Riecoin Core's pow.cpp (MIT). A block's header hash (first 80 bytes) and nBits give a big target number; the
//! 32-byte nOffset field encodes an offset; n = target + offset must start a prime constellation (one of the
//! accepted patterns, e.g. n, n+2, n+6, n+8, n+12, n+18, n+20). The pool also accepts "k-shares": n and the
//! pattern's second number prime and k of its numbers prime in all (rieMiner's and StellaPool's share rule).
//!
//! nOffset, little-endian: bytes 0..2 = 2 + 32 * difficulty offset (low 5 bits 2 = PoW version 1), 2..14 = primorial
//! offset, 14..30 = primorial factor, 30..32 = primorial number p.
//! offset = P - (target mod P) + factor * P + primorial offset, P = product of the first p primes.

use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::One;
use std::sync::OnceLock;

/// Why an nOffset gives no valid candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// Not PoW version 1 (low 5 bits of nOffset must be 2).
    Version,
    /// nBits below the network's minimum.
    Bits,
    /// The primorial is larger than 2^trailing zeros.
    Primorial,
    /// The offset is 2^trailing zeros or more.
    Offset,
}

/// The first 65,536 primes (2 .. 821,641), as Riecoin Core's primeTable.
pub fn prime_table() -> &'static [u32] {
    static TABLE: OnceLock<Vec<u32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        const LIMIT: usize = 821_641;
        let mut composite = vec![false; LIMIT + 1];
        let mut primes = Vec::with_capacity(65_536);
        for i in 2..=LIMIT {
            if !composite[i] {
                primes.push(i as u32);
                let mut m = i * i;
                while m <= LIMIT {
                    composite[m] = true;
                    m += i;
                }
            }
        }
        primes
    })
}

/// The nBits increase a miner chose in its nOffset (a "difficulty offset"; 0 for the usual value 2), clamped as the node does.
pub fn nbits_offset(nonce: &[u8; 32], nbits: u32) -> u64 {
    let low16 = u16::from_le_bytes([nonce[0], nonce[1]]) as u64;
    if low16 == 2 {
        return 0;
    }
    ((low16 & 0xffe0) << 8).min(u32::MAX as u64 - nbits as u64)
}

/// Trailing zeros of the target (its size is about nBits / 256 bits): (nBits + offset) / 256 + 1 - 265.
pub fn trailing_zeros(nbits: u32, offset: u64, nbits_min: u32) -> Option<u32> {
    if nbits < nbits_min {
        return None;
    }
    let tz = ((nbits as u64 + offset) >> 8) + 1;
    tz.checked_sub(265).map(|t| t as u32)
}

/// The target: ((256 + L) * 2^256 + hash) * 2^tz, where hash is the 80-byte header's double SHA-256 read as a
/// little-endian number and L = round(2^(8 + df/256) - 2^8) for df = nBits mod 256 (the node's integer formula).
pub fn derive_target(hash: &[u8; 32], nbits: u32, tz: u32) -> BigUint {
    let df = (nbits & 255) as u64;
    let l = (10 * df * df * df + 7383 * df * df + 5_840_720 * df + 3_997_440) >> 23;
    let mut t = BigUint::from(256 + l) << 256usize;
    t += BigUint::from_bytes_le(hash);
    t << tz as usize
}

/// A submitted candidate: n and the target size it was mined at.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub n: BigUint,
    pub trailing_zeros: u32,
    pub difficulty_offset: u64,
}

/// n = target + offset for a header hash, nBits and nOffset, with the node's checks (pow.cpp CheckProofOfWorkImpl).
pub fn candidate(hash: &[u8; 32], nbits: u32, nonce: &[u8; 32], nbits_min: u32) -> Result<Candidate, Bad> {
    if nonce[0] & 31 != 2 {
        return Err(Bad::Version);
    }
    if nbits < nbits_min {
        return Err(Bad::Bits);
    }
    let off = nbits_offset(nonce, nbits);
    let tz = trailing_zeros(nbits, off, nbits_min).ok_or(Bad::Bits)?;
    let target = derive_target(hash, nbits, tz);
    let limit = BigUint::one() << tz as usize;
    let count = u16::from_le_bytes([nonce[30], nonce[31]]) as usize;
    let mut primorial = BigUint::one();
    for &p in &prime_table()[..count] {
        primorial *= p;
        if primorial > limit {
            return Err(Bad::Primorial);
        }
    }
    let factor = BigUint::from_bytes_le(&nonce[14..30]);
    let poff = BigUint::from_bytes_le(&nonce[2..14]);
    let offset = &primorial - target.mod_floor(&primorial) + factor * &primorial + poff;
    if offset >= limit {
        return Err(Bad::Offset);
    }
    Ok(Candidate { n: target + offset, trailing_zeros: tz, difficulty_offset: off })
}

/// Fermat test to base 2 (what miners and the share check use).
pub fn fermat(n: &BigUint) -> bool {
    if n < &BigUint::from(4u32) {
        return n == &BigUint::from(2u32) || n == &BigUint::from(3u32);
    }
    if n.is_even() {
        return false;
    }
    BigUint::from(2u32).modpow(&(n - 1u32), n).is_one()
}

/// Miller-Rabin with the first 16 primes as bases (the block check; the node runs its own test anyway).
pub fn strong_probable_prime(n: &BigUint) -> bool {
    if n < &BigUint::from(4u32) {
        return n == &BigUint::from(2u32) || n == &BigUint::from(3u32);
    }
    if n.is_even() {
        return false;
    }
    let m = n - 1u32;
    let s = m.trailing_zeros().unwrap_or(0);
    let d = &m >> s as usize;
    'bases: for &a in &prime_table()[..16] {
        let a = BigUint::from(a);
        if &a >= n {
            continue;
        }
        let mut x = a.modpow(&d, n);
        if x.is_one() || x == m {
            continue;
        }
        for _ in 1..s {
            x = &x * &x % n;
            if x == m {
                continue 'bases;
            }
        }
        return false;
    }
    true
}

/// The numbers of a pattern: n + running sum of its offsets ({0, 2, 4} = n, n+2, n+6).
fn members(n: &BigUint, pattern: &[u32]) -> Vec<BigUint> {
    let mut x = n.clone();
    pattern
        .iter()
        .map(|&o| {
            x += o;
            x.clone()
        })
        .collect()
}

/// How many numbers of `pattern` are (Fermat) primes, or 0 when one of the first two is not.
pub fn primes_in(n: &BigUint, pattern: &[u32]) -> u32 {
    let mut count = 0;
    for (i, m) in members(n, pattern).iter().enumerate() {
        if fermat(m) {
            count += 1;
        } else if i < 2 {
            return 0;
        }
    }
    count
}

/// The share's k: the best count over the accepted patterns.
pub fn share_k(n: &BigUint, patterns: &[Vec<u32>]) -> u32 {
    patterns.iter().map(|p| primes_in(n, p)).max().unwrap_or(0)
}

/// A full constellation of one accepted pattern, every number a strong probable prime: a block.
pub fn is_block(n: &BigUint, patterns: &[Vec<u32>]) -> bool {
    patterns.iter().any(|p| members(n, p).iter().all(strong_probable_prime))
}

/// Smallest k the pool takes as a share: pattern length - 2, at least 4 (rieMiner's and StellaPool's rule), at
/// most the pattern length (regtest's single-number pattern).
pub fn min_share_k(pattern_len: usize) -> u32 {
    (pattern_len.saturating_sub(2).max(4)).min(pattern_len) as u32
}

/// Expected shares (at `kmin`) per block, if each of the pattern's numbers after the first two is prime with
/// probability p, independently: P(at least kmin - 2 of the m others prime) / p^m.
pub fn shares_per_block(p: f64, pattern_len: usize, kmin: u32) -> f64 {
    let m = pattern_len.saturating_sub(2) as i32;
    let j0 = kmin.saturating_sub(2) as i32;
    if m <= 0 {
        return 1.0;
    }
    let mut at_least = 0.0;
    for j in j0..=m {
        at_least += binom(m, j) * p.powi(j) * (1.0 - p).powi(m - j);
    }
    at_least / p.powi(m)
}

fn binom(n: i32, k: i32) -> f64 {
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// p from the shares seen: `above` shares with k > kmin against `at` shares with exactly kmin. Under the same
/// model the ratio above/at grows with p; solved by bisection.
pub fn estimate_p(at: f64, above: f64, pattern_len: usize, kmin: u32) -> Option<f64> {
    let m = pattern_len.saturating_sub(2) as i32;
    let j0 = kmin.saturating_sub(2) as i32;
    if m <= 0 || j0 >= m || at <= 0.0 {
        return None;
    }
    let want = above / at;
    let ratio = |p: f64| {
        let exact = binom(m, j0) * p.powi(j0) * (1.0 - p).powi(m - j0);
        let more: f64 = (j0 + 1..=m).map(|j| binom(m, j) * p.powi(j) * (1.0 - p).powi(m - j)).sum();
        more / exact
    };
    let (mut lo, mut hi) = (1e-6, 0.999);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if ratio(mid) < want {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_table_like_the_node() {
        let t = prime_table();
        assert_eq!(t.len(), 65_536);
        assert_eq!((t[0], t[32_767], t[65_535]), (2, 386_093, 821_641));
    }

    #[test]
    fn primality() {
        for p in [2u32, 3, 5, 7, 13, 65_537, 821_641] {
            assert!(fermat(&BigUint::from(p)) && strong_probable_prime(&BigUint::from(p)), "{p}");
        }
        for c in [1u32, 4, 9, 15, 65_535] {
            assert!(!fermat(&BigUint::from(c)) && !strong_probable_prime(&BigUint::from(c)), "{c}");
        }
        // 341 = 11 * 31 fools Fermat base 2, not Miller-Rabin.
        assert!(fermat(&BigUint::from(341u32)) && !strong_probable_prime(&BigUint::from(341u32)));
        // 2^127 - 1 is prime, 3 (2^127 - 1) is not; the Fermat number 2^128 + 1 is composite but fools base 2.
        let m127 = (BigUint::one() << 127usize) - 1u32;
        assert!(fermat(&m127) && strong_probable_prime(&m127));
        assert!(!fermat(&(&m127 * 3u32)) && !strong_probable_prime(&(&m127 * 3u32)));
        let f7 = (BigUint::one() << 128usize) + 1u32;
        assert!(fermat(&f7) && !strong_probable_prime(&f7));
    }

    #[test]
    fn share_rules() {
        // 97, 101, 103, 107, 109, 113 (97 + {0,4,2,4,2,4}): a prime sextuplet.
        let n = BigUint::from(97u32);
        assert_eq!(primes_in(&n, &[0, 4, 2, 4, 2, 4]), 6);
        assert!(is_block(&n, &[vec![0, 4, 2, 4, 2, 4]]));
        // 7: 7, 11, 13, 17, 19, 23 also; shift the pattern so the second number fails: 0.
        assert_eq!(primes_in(&BigUint::from(7u32), &[0, 2, 4]), 0); // 9 is not prime
        assert_eq!(primes_in(&BigUint::from(11u32), &[0, 2, 2, 4]), 3); // 11, 13, 15 no, 19
        assert_eq!(min_share_k(7), 5);
        assert_eq!(min_share_k(5), 4);
        assert_eq!(min_share_k(1), 1);
    }

    #[test]
    fn share_model() {
        // The estimate inverts the model.
        for p in [0.03f64, 0.1, 0.25] {
            let m = 5;
            let at = binom(m, 3) * p.powi(3) * (1.0 - p).powi(2);
            let above = binom(m, 4) * p.powi(4) * (1.0 - p) + p.powi(5);
            let e = estimate_p(at, above, 7, 5).unwrap();
            assert!((e - p).abs() < 1e-9, "{p} {e}");
        }
        // Single-number pattern (regtest): every share is a block.
        assert_eq!(shares_per_block(0.5, 1, 1), 1.0);
    }
}
