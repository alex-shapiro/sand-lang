//! The `uuid` module.

use crate::check::ModKind;
use crate::native::{HostError, NativeModule};
use crate::vm::{Ctl, Vm};

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("uuid", ModKind::Std, include_str!("uuid.sand"));
    m.vm_function("v4", |vm: &mut Vm| -> Result<_, Ctl> {
        let mut b = random_bytes(vm);
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(format_uuid(&b))
    });
    m.vm_function("unix_millis", |vm: &mut Vm| -> Result<_, Ctl> {
        let ms = vm.host.clock.wall().duration_since(crate::platform::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        Ok(ms.min(i64::MAX as u128) as i64)
    });
    // Starts each millisecond's counter at a random value in the lower half
    // of its range, leaving room for increments.
    m.vm_function("random_counter", |vm: &mut Vm| vm.host.rng.below(0x800) as i64);
    m.vm_function("v7_from", |vm: &mut Vm, millis: i64, counter: i64| -> Result<_, Ctl> {
        let millis = (millis as u64) & 0xffff_ffff_ffff;
        let counter = (counter as u64) & 0xfff;
        let mut b = random_bytes(vm);
        b[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
        b[6] = 0x70 | (counter >> 8) as u8;
        b[7] = counter as u8;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(format_uuid(&b))
    });
    m.function("parse", |s: &str| match parse(s) {
        Some(b) => Ok(format_uuid(&b)),
        None => Err(HostError::new("parse", format!("invalid UUID: {:?}", super::truncate(s, 64)))),
    });
    m
}

fn random_bytes(vm: &mut Vm) -> [u8; 16] {
    let hi = vm.host.rng.next_u64().to_be_bytes();
    let lo = vm.host.rng.next_u64().to_be_bytes();
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&hi);
    b[8..].copy_from_slice(&lo);
    b
}

/// The lower-case hyphenated form `8-4-4-4-12`.
fn format_uuid(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(36);
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 15) as usize] as char);
    }
    s
}

/// Parses the hyphenated, simple, braced and URN forms.
fn parse(s: &str) -> Option<[u8; 16]> {
    let s = match (s.get(..9), s.get(9..)) {
        (Some(p), Some(rest)) if p.eq_ignore_ascii_case("urn:uuid:") => rest,
        _ => s,
    };
    let s = s.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(s);
    let hex: Vec<u8> = match s.len() {
        32 => s.bytes().collect(),
        36 => {
            let b = s.as_bytes();
            if [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
                return None;
            }
            b.iter().copied().filter(|&c| c != b'-').collect()
        }
        _ => return None,
    };
    if hex.len() != 32 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = digit(hex[2 * i]) << 4 | digit(hex[2 * i + 1]);
    }
    Some(out)
}
