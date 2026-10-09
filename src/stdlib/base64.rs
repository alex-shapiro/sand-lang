//! The `base64` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::Value;
use crate::vm::{NResult, Vm};

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn register(r: &mut Registry) {
    r.source("base64", ModKind::Std, include_str!("base64.sand"));
    r.add_builtin("base64.encode", |vm, a| encode_native(vm, a[0], false));
    r.add_builtin("base64.encode_url", |vm, a| encode_native(vm, a[0], true));
    r.add_builtin("base64.decode", |vm, a| decode_native(vm, a[0], false));
    r.add_builtin("base64.decode_url", |vm, a| decode_native(vm, a[0], true));
}

fn encode_native(vm: &mut Vm, data: Value, url: bool) -> NResult {
    vm.reserve(vm.heap.bytes_of(data).len() / 3 * 4 + 4)?;
    vm.charge_slow_bytes(vm.heap.bytes_of(data).len())?;
    let s = encode(vm.heap.bytes_of(data), url);
    Ok(vm.new_str(s))
}

fn decode_native(vm: &mut Vm, text: Value, url: bool) -> NResult {
    vm.reserve(vm.str(text).len() / 4 * 3 + 3)?;
    vm.charge_slow_bytes(vm.str(text).len())?;
    match decode(vm.str(text), url) {
        Ok(bytes) => {
            let b = vm.heap.new_bytes(bytes);
            Ok(vm.ok(b))
        }
        Err(msg) => Ok(vm.err_value("parse", &msg)),
    }
}

/// Encodes `data`: the standard alphabet with padding, or the URL-safe
/// alphabet without padding.
pub(crate) fn encode(data: &[u8], url: bool) -> String {
    let alphabet = if url { URL_SAFE } else { STANDARD };
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if !url {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes base64 text. The standard form requires padding; the URL-safe
/// form accepts it optionally. Trailing bits must be zero.
pub(crate) fn decode(text: &str, url: bool) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let unpadded = bytes.len() - bytes.iter().rev().take_while(|&&b| b == b'=').count();
    let padding = bytes.len() - unpadded;
    if padding > 0 || !url {
        // Padding present (or required): the total length must be a multiple
        // of four and the padding must exactly fill the last group.
        if !bytes.len().is_multiple_of(4) || padding > 2 {
            return Err("invalid base64: incorrect length or padding".into());
        }
    }
    if unpadded % 4 == 1 {
        return Err("invalid base64: incorrect length".into());
    }
    let mut out = Vec::with_capacity(unpadded / 4 * 3 + 2);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for (i, &b) in bytes[..unpadded].iter().enumerate() {
        let Some(v) = sextet(b, url) else {
            let c = text[i..].chars().next().unwrap_or('?');
            return Err(format!("invalid base64: unexpected character `{}` at offset {i}", c.escape_debug()));
        };
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1 << nbits) - 1;
        }
    }
    if acc != 0 {
        return Err("invalid base64: non-zero trailing bits".into());
    }
    Ok(out)
}

/// The 6-bit value of an alphabet character.
fn sextet(b: u8, url: bool) -> Option<u8> {
    Some(match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' if !url => 62,
        b'/' if !url => 63,
        b'-' if url => 62,
        b'_' if url => 63,
        _ => return None,
    })
}
