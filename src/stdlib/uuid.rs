//! The `uuid` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::Vm;
use crate::vm::value::Value;

pub fn register(r: &mut Registry) {
    r.source("uuid", ModKind::Std, include_str!("uuid.sand"));
    r.add_builtin("uuid.v4", |vm, _| {
        let mut b = random_bytes(vm);
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(vm.new_str(format_uuid(&b)))
    });
    r.add_builtin("uuid.unix_millis", |_, _| {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        Ok(Value::Int(ms.min(i64::MAX as u128) as i64))
    });
    // Starts each millisecond's counter at a random value in the lower half
    // of its range, leaving room for increments.
    r.add_builtin("uuid.random_counter", |vm, _| Ok(Value::Int(vm.host.rng.below(0x800) as i64)));
    r.add_builtin("uuid.v7_from", |vm, a| {
        let millis = (a[0].as_int() as u64) & 0xffff_ffff_ffff;
        let counter = (a[1].as_int() as u64) & 0xfff;
        let mut b = random_bytes(vm);
        b[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
        b[6] = 0x70 | (counter >> 8) as u8;
        b[7] = counter as u8;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(vm.new_str(format_uuid(&b)))
    });
    r.add_builtin("uuid.parse", |vm, a| {
        let parsed = parse(vm.str(a[0]));
        Ok(match parsed {
            Some(b) => {
                let s = vm.new_str(format_uuid(&b));
                vm.ok(s)
            }
            None => {
                let msg = format!("invalid UUID: {:?}", super::truncate(vm.str(a[0]), 64));
                vm.err_value("parse", &msg)
            }
        })
    });
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
    let mut s = String::with_capacity(36);
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{x:02x}"));
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
