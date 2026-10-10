//! The `crypto` module: SHA-256, SHA-1, MD5 and HMAC-SHA256 from the
//! RustCrypto crates, charged as they go.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::Ctl;
use crate::vm::meter::{Budget, SLOW_BYTES_PER_UNIT};
use crate::vm::value::Value;
use hmac::{KeyInit, Mac};
use sha2::Digest;

pub fn register(r: &mut Registry) {
    r.source("crypto", ModKind::Std, include_str!("crypto.sand"));
    r.add_builtin("crypto.sha256", |vm, a| {
        let d = sha256_parts(&[vm.heap.bytes_of(a[0])], &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.sha1", |vm, a| {
        let d = digest::<sha1::Sha1>(&[vm.heap.bytes_of(a[0])], &vm.meter)?;
        Ok(vm.heap.new_bytes(d.to_vec()))
    });
    r.add_builtin("crypto.md5", |vm, a| {
        let d = digest::<md5::Md5>(&[vm.heap.bytes_of(a[0])], &vm.meter)?;
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

/// Bytes hashed between charges to the budget (4 KiB).
const CHUNK: usize = 4096;

/// Feeds the concatenation of `parts` to `update`, charging `budget` as it
/// goes so that hashing huge inputs can be stopped.
fn feed(parts: &[&[u8]], budget: &impl Budget, mut update: impl FnMut(&[u8])) -> Result<(), Ctl> {
    for part in parts {
        for chunk in part.chunks(CHUNK) {
            budget.charge((chunk.len() / SLOW_BYTES_PER_UNIT) as u64)?;
            update(chunk);
        }
    }
    Ok(())
}

/// The digest of the concatenation of `parts`.
fn digest<D: Digest>(parts: &[&[u8]], budget: &impl Budget) -> Result<sha2::digest::Output<D>, Ctl> {
    let mut h = D::new();
    feed(parts, budget, |chunk| h.update(chunk))?;
    Ok(h.finalize())
}

/// SHA-256 (FIPS 180-4) of the concatenation of `parts`.
pub(crate) fn sha256_parts(parts: &[&[u8]], budget: &impl Budget) -> Result<[u8; 32], Ctl> {
    Ok(digest::<sha2::Sha256>(parts, budget)?.into())
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8], budget: &impl Budget) -> Result<[u8; 32], Ctl> {
    // A key longer than a block is hashed first.
    budget.charge((key.len() / SLOW_BYTES_PER_UNIT) as u64)?;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).map_err(|_| Ctl::panic("internal error: HMAC key"))?;
    feed(&[data], budget, |chunk| mac.update(chunk))?;
    Ok(mac.finalize().into_bytes().into())
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
        assert_eq!(hex(&digest::<sha1::Sha1>(&[m], &Unmetered).unwrap()), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(
            hex(&digest::<md5::Md5>(&[b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"], &Unmetered)
                .unwrap()),
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
