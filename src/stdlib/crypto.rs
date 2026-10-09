//! The `crypto` module: SHA-256, SHA-1, MD5 and HMAC-SHA256, implemented
//! from their specifications (FIPS 180-4, RFC 1321, RFC 2104).

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::Ctl;
use crate::vm::meter::{Budget, SLOW_BYTES_PER_UNIT};
use crate::vm::value::Value;

pub fn register(r: &mut Registry) {
    r.source("crypto", ModKind::Std, include_str!("crypto.sand"));
    r.add_builtin("crypto.sha256", |vm, a| {
        let d = sha256_parts(&[vm.heap.bytes_of(a[0])], &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.sha1", |vm, a| {
        let d = sha1(vm.heap.bytes_of(a[0]), &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.md5", |vm, a| {
        let d = md5(vm.heap.bytes_of(a[0]), &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.hmac_sha256", |vm, a| {
        let d = hmac_sha256(vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]), &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.constant_time_eq", |vm, a| {
        vm.charge_bytes(vm.heap.bytes_of(a[0]).len())?;
        Ok(Value::Bool(constant_time_eq(vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]))))
    });
}

/// Blocks hashed between charges to the budget (4 KiB).
const BLOCKS_PER_CHARGE: usize = 64;

/// Merkle–Damgård padding shared by SHA-1, SHA-256 and MD5: the message
/// (the concatenation of `parts`), `0x80`, zeros, and the bit length (big-
/// or little-endian) filling 64-byte blocks. Calls `block` for each block,
/// charging `budget` as it goes so that hashing huge inputs can be stopped.
fn md_blocks(parts: &[&[u8]], big_endian: bool, budget: &impl Budget, mut block: impl FnMut(&[u8; 64])) -> Result<(), Ctl> {
    let mut buf = [0u8; 64];
    let mut filled = 0;
    let mut len: u64 = 0;
    for part in parts {
        len = len.wrapping_add(part.len() as u64);
        let mut rest = *part;
        if filled > 0 {
            let n = rest.len().min(64 - filled);
            buf[filled..filled + n].copy_from_slice(&rest[..n]);
            filled += n;
            rest = &rest[n..];
            if filled < 64 {
                continue;
            }
            block(&buf);
        }
        let (blocks, rem) = rest.as_chunks::<64>();
        for chunk in blocks.chunks(BLOCKS_PER_CHARGE) {
            budget.charge((chunk.len() * 64 / SLOW_BYTES_PER_UNIT) as u64)?;
            chunk.iter().for_each(&mut block);
        }
        buf[..rem.len()].copy_from_slice(rem);
        filled = rem.len();
    }
    let mut tail = [0u8; 128];
    tail[..filled].copy_from_slice(&buf[..filled]);
    tail[filled] = 0x80;
    let total = if filled < 56 { 64 } else { 128 };
    let bit_len = len.wrapping_mul(8);
    let len_bytes = if big_endian { bit_len.to_be_bytes() } else { bit_len.to_le_bytes() };
    tail[total - 8..total].copy_from_slice(&len_bytes);
    tail[..total].as_chunks::<64>().0.iter().for_each(block);
    Ok(())
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
    0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
    0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
    0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
    0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 (FIPS 180-4) of the concatenation of `parts`.
pub(crate) fn sha256_parts(parts: &[&[u8]], budget: &impl Budget) -> Result<[u8; 32], Ctl> {
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    md_blocks(parts, true, budget, |block| {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    })?;
    let mut out = [0u8; 32];
    for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = x.to_be_bytes();
    }
    Ok(out)
}

/// SHA-1 (FIPS 180-4).
pub(crate) fn sha1(data: &[u8], budget: &impl Budget) -> Result<[u8; 20], Ctl> {
    let mut h: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    md_blocks(&[data], true, budget, |block| {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    })?;
    let mut out = [0u8; 20];
    for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = x.to_be_bytes();
    }
    Ok(out)
}

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4,
    11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// MD5 (RFC 1321).
pub(crate) fn md5(data: &[u8], budget: &impl Budget) -> Result<[u8; 16], Ctl> {
    // K[i] = floor(abs(sin(i + 1)) * 2^32).
    let k: [u32; 64] = std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32);
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    md_blocks(&[data], false, budget, |block| {
        let mut m = [0u32; 16];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            m[i] = u32::from_le_bytes(*word);
        }
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(MD5_S[i]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    })?;
    let mut out = [0u8; 16];
    for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = x.to_le_bytes();
    }
    Ok(out)
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8], budget: &impl Budget) -> Result<[u8; 32], Ctl> {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256_parts(&[key], budget)?);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad = k.map(|b| b ^ 0x36);
    let opad = k.map(|b| b ^ 0x5c);
    let inner = sha256_parts(&[&ipad, data], budget)?;
    sha256_parts(&[&opad, &inner], budget)
}

/// Equality whose running time does not depend on where the inputs differ.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::meter::Unmetered;

    fn sha256(data: &[u8]) -> [u8; 32] {
        sha256_parts(&[data], &Unmetered).unwrap()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn digests_across_block_boundaries() {
        // NIST two-block message.
        let m = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex(&sha256(m)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        assert_eq!(hex(&sha1(m, &Unmetered).unwrap()), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(
            hex(&md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890", &Unmetered).unwrap()),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        // Splitting the input must not change the digest.
        let data: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        for cut in [0, 1, 55, 63, 64, 65, 200, 300] {
            assert_eq!(sha256_parts(&[&data[..cut], &data[cut..]], &Unmetered).unwrap(), sha256(&data));
        }
    }

    #[test]
    fn hmac_rfc4231() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There", &Unmetered).unwrap()),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let long_key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(&long_key, b"Test Using Larger Than Block-Size Key - Hash Key First", &Unmetered).unwrap()),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
