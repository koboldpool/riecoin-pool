//! Riecoin block parts the pool builds (Bitcoin Core rules): double SHA-256, the coinbase split for stratum,
//! the coinbase's merkle branch, the header (112 bytes: version 4, parent 32, merkle root 32, time 8, nBits 4,
//! nOffset 32; the PoW hash covers the first 80) and the block id (double SHA-256 of all 112).

use sha2::{Digest, Sha256};

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(data)).into()
}

pub fn compact_size(n: u64, out: &mut Vec<u8>) {
    match n {
        0..=0xfc => out.push(n as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// `CScript() << n` for a block height (BIP34): OP_0, OP_1..OP_16, or a minimal little-endian number push.
pub fn script_height(n: u64) -> Vec<u8> {
    match n {
        0 => vec![0x00],
        1..=16 => vec![0x50 + n as u8],
        _ => {
            let mut b = Vec::new();
            let mut x = n;
            while x > 0 {
                b.push(x as u8);
                x >>= 8;
            }
            if b.last().is_some_and(|l| l & 0x80 != 0) {
                b.push(0);
            }
            let mut s = vec![b.len() as u8];
            s.extend_from_slice(&b);
            s
        }
    }
}

/// Bytes of the extra nonces in the coinbase scriptSig.
pub const EN1_SIZE: usize = 4;
pub const EN2_SIZE: usize = 4;

/// The coinbase (without witness) split around the extra nonces: coinb1 || en1 || en2 || coinb2.
/// scriptSig = height push, a push of the 8 extra-nonce bytes, then a push of the pool tag.
/// Outputs: the whole value to the pool's script, then the witness commitment when the template has one.
/// Sequence and lock time as the node's own miner sets them (0xfffffffe, height - 1).
pub fn coinbase_parts(height: u64, tag: &[u8], value: u64, pool_script: &[u8], witness_commitment: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    assert!(tag.len() <= 64);
    let hs = script_height(height);
    let script_len = hs.len() + 1 + EN1_SIZE + EN2_SIZE + 1 + tag.len();
    let mut c1 = Vec::new();
    c1.extend_from_slice(&2u32.to_le_bytes());
    c1.push(1);
    c1.extend_from_slice(&[0u8; 32]);
    c1.extend_from_slice(&u32::MAX.to_le_bytes());
    compact_size(script_len as u64, &mut c1);
    c1.extend_from_slice(&hs);
    c1.push((EN1_SIZE + EN2_SIZE) as u8);
    let mut c2 = vec![tag.len() as u8];
    c2.extend_from_slice(tag);
    c2.extend_from_slice(&0xffff_fffeu32.to_le_bytes());
    compact_size(1 + witness_commitment.is_some() as u64, &mut c2);
    c2.extend_from_slice(&value.to_le_bytes());
    compact_size(pool_script.len() as u64, &mut c2);
    c2.extend_from_slice(pool_script);
    if let Some(w) = witness_commitment {
        c2.extend_from_slice(&0u64.to_le_bytes());
        compact_size(w.len() as u64, &mut c2);
        c2.extend_from_slice(w);
    }
    c2.extend_from_slice(&(height.saturating_sub(1) as u32).to_le_bytes());
    (c1, c2)
}

/// Bitcoin's merkle root (odd last node paired with itself); leaves are txids in raw byte order.
pub fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return [0; 32];
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        level = level.chunks(2).map(|p| pair(&p[0], &p[1])).collect();
    }
    level[0]
}

fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(a);
    buf[32..].copy_from_slice(b);
    sha256d(&buf)
}

/// The stratum merkle branch of the coinbase (leaf 0); `others` are the other transactions' txids.
pub fn coinbase_branch(others: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut branch = Vec::new();
    // The level as a list whose first entry stands for the coinbase side.
    let mut level: Vec<Option<[u8; 32]>> = std::iter::once(None).chain(others.iter().copied().map(Some)).collect();
    while level.len() > 1 {
        branch.push(level[1].unwrap());
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        let mut next = vec![None];
        for p in level[2..].chunks(2) {
            next.push(Some(pair(&p[0].unwrap(), &p[1].unwrap())));
        }
        level = next;
    }
    branch
}

/// The root from the coinbase and its branch (what the miner computes).
pub fn root_from_branch(coinbase: &[u8], branch: &[[u8; 32]]) -> [u8; 32] {
    branch.iter().fold(sha256d(coinbase), |h, b| pair(&h, b))
}

/// The 112-byte header.
pub fn header(version: u32, prev: &[u8; 32], root: &[u8; 32], time: u64, bits: u32, nonce: &[u8; 32]) -> [u8; 112] {
    let mut h = [0u8; 112];
    h[0..4].copy_from_slice(&version.to_le_bytes());
    h[4..36].copy_from_slice(prev);
    h[36..68].copy_from_slice(root);
    h[68..76].copy_from_slice(&time.to_le_bytes());
    h[76..80].copy_from_slice(&bits.to_le_bytes());
    h[80..112].copy_from_slice(nonce);
    h
}

/// The hash the PoW target is derived from (GetHashForPoW): double SHA-256 of the first 80 bytes.
pub fn pow_hash(header: &[u8; 112]) -> [u8; 32] {
    sha256d(&header[..80])
}

/// The block id (GetHash for PoW version 1 blocks), raw byte order.
pub fn block_id(header: &[u8; 112]) -> [u8; 32] {
    sha256d(header)
}

/// Each 4-byte word reversed (stratum's prevhash, both ways).
pub fn swap_words(b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in (0..32).step_by(4) {
        for k in 0..4 {
            out[i + k] = b[i + 3 - k];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_matches_the_full_tree() {
        for n in 0..14usize {
            let others: Vec<[u8; 32]> = (0..n).map(|i| [i as u8 + 1; 32]).collect();
            let cb = b"coinbase".to_vec();
            let mut leaves = vec![sha256d(&cb)];
            leaves.extend_from_slice(&others);
            assert_eq!(root_from_branch(&cb, &coinbase_branch(&others)), merkle_root(&leaves), "{n} other txs");
        }
    }

    #[test]
    fn heights() {
        assert_eq!(script_height(0), vec![0x00]);
        assert_eq!(script_height(16), vec![0x60]);
        assert_eq!(script_height(17), vec![1, 17]);
        assert_eq!(script_height(128), vec![2, 0x80, 0]);
        assert_eq!(script_height(2_675_756), vec![3, 0x2c, 0xd4, 0x28]);
    }

    #[test]
    fn coinbase_layout() {
        // rieMiner reads the height from coinb1[42..]: 1-byte length then the bytes.
        let (c1, c2) = coinbase_parts(2_675_756, b"/KoboldPool/", 625_000_000, &[0x00, 0x14, 1, 2], Some(&[0x6a, 0x24]));
        assert_eq!(c1[42], 3);
        assert_eq!(&c1[43..46], &[0x2c, 0xd4, 0x28]);
        assert_eq!(c1[41] as usize, 4 + 1 + 8 + 1 + 12);
        assert_eq!(*c1.last().unwrap(), 8);
        assert_eq!(&c2[c2.len() - 4..], &2_675_755u32.to_le_bytes());
    }
}
