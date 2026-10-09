//! The `uri` module (RFC 3986).

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm, panic};

pub fn register(r: &mut Registry) {
    r.source("uri", ModKind::Std, include_str!("uri.sand"));
    r.add_builtin("uri.parse", |vm, a| {
        let n = vm.str(a[0]).len();
        vm.reserve(2 * n)?;
        vm.charge_slow_bytes(n)?;
        let parsed = parse(vm.str(a[0]), true);
        result(vm, parsed)
    });
    r.add_builtin("uri.Uri.join", |vm, a| {
        let n = vm.str(a[1]).len() + vm.heap.fields(a[0]).iter().map(|f| vm.str(*f).len()).sum::<usize>();
        vm.reserve(3 * n)?;
        vm.charge_slow_bytes(n)?;
        let base = from_value(vm, a[0]);
        let resolved = parse(vm.str(a[1]), false).map(|r| resolve(&base, r));
        result(vm, resolved)
    });
    r.add_builtin("uri.encode_component", |vm, a| encode_native(vm, a[0], b""));
    r.add_builtin("uri.encode_path", |vm, a| encode_native(vm, a[0], b"/"));
    r.add_builtin("uri.decode_component", |vm, a| {
        vm.reserve(vm.str(a[0]).len())?;
        vm.charge_slow_bytes(vm.str(a[0]).len())?;
        match decode_component(vm.str(a[0])) {
            Ok(s) => {
                let s = vm.new_str(s);
                Ok(vm.ok(s))
            }
            Err(msg) => Ok(vm.err_value("parse", &msg)),
        }
    });
    r.add_builtin("uri.parse_query", |vm, a| {
        let q = vm.str(a[0]);
        vm.reserve(q.len() * 4)?;
        vm.charge_slow_bytes(q.len())?;
        let pairs = parse_query(q);
        vm.charge(pairs.len() as u64)?;
        let mut items = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let k = vm.new_str(k);
            let v = vm.new_str(v);
            items.push(vm.heap.new_tuple(vec![k, v]));
        }
        Ok(vm.heap.new_list(items))
    });
}

/// The components of a URI reference.
#[derive(Debug, Default, Clone, PartialEq)]
struct Parts {
    scheme: Option<String>,
    authority: Option<Authority>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Authority {
    userinfo: Option<String>,
    host: String,
    port: Option<i64>,
}

fn result(vm: &mut Vm, parsed: Result<Parts, String>) -> NResult {
    match parsed {
        Ok(p) => {
            let u = to_value(vm, p)?;
            Ok(vm.ok(u))
        }
        Err(msg) => Ok(vm.err_value("parse", &msg)),
    }
}

fn uri_def(vm: &Vm) -> Result<u32, crate::vm::Ctl> {
    match vm.prog.types.iter().position(|t| t.name == "Uri" && t.module == "uri") {
        Some(d) => Ok(d as u32),
        None => panic("uri.Uri is not loaded"),
    }
}

fn to_value(vm: &mut Vm, p: Parts) -> NResult {
    let def = uri_def(vm)?;
    let opt_str = |vm: &mut Vm, s: Option<String>| {
        let v = s.map(|s| vm.new_str(s));
        vm.option(v)
    };
    let scheme = vm.new_str(p.scheme.unwrap_or_default());
    let (userinfo, host, port) = match p.authority {
        Some(a) => (opt_str(vm, a.userinfo), opt_str(vm, Some(a.host)), vm.option(a.port.map(Value::Int))),
        None => (vm.none(), vm.none(), vm.none()),
    };
    let path = vm.new_str(p.path);
    let query = opt_str(vm, p.query);
    let fragment = opt_str(vm, p.fragment);
    Ok(vm.heap.alloc(Obj::Struct(def, Box::new([scheme, userinfo, host, port, path, query, fragment]))))
}

fn from_value(vm: &Vm, v: Value) -> Parts {
    let f = vm.heap.fields(v);
    let opt_str = |x: Value| vm.as_option(x).map(|s| vm.str(s).to_string());
    let authority = opt_str(f[2]).map(|host| Authority { userinfo: opt_str(f[1]), host, port: vm.as_option(f[3]).map(|p| p.as_int()) });
    Parts {
        scheme: Some(vm.str(f[0]).to_string()),
        authority,
        path: vm.str(f[4]).to_string(),
        query: opt_str(f[5]),
        fragment: opt_str(f[6]),
    }
}

/// Characters that may never appear literally in a URI.
fn is_forbidden(b: u8) -> bool {
    b <= b' ' || b == 0x7f || b"\"<>\\^`{|}".contains(&b)
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delim(b: u8) -> bool {
    b"!$&'()*+,;=".contains(&b)
}

/// Parses a URI reference. With `absolute`, a scheme is required.
fn parse(text: &str, absolute: bool) -> Result<Parts, String> {
    let shown: String = text.chars().take(100).collect();
    let fail = |why: &str| format!("invalid URI `{shown}`: {why}");
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if is_forbidden(b) {
            return Err(fail(&format!("invalid character {:?} at offset {i}", b as char)));
        }
        if b == b'%' && !(i + 2 < bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit()) {
            return Err(fail("invalid percent-encoding"));
        }
    }
    let mut p = Parts::default();
    let mut rest = text;
    if let Some((r, f)) = rest.split_once('#') {
        p.fragment = Some(f.to_string());
        rest = r;
    }
    if let Some((r, q)) = rest.split_once('?') {
        p.query = Some(q.to_string());
        rest = r;
    }
    if let Some(colon) = rest.find(':')
        && !rest[..colon].contains('/')
    {
        let scheme = &rest[..colon];
        let valid = scheme.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        if !valid {
            return Err(fail("invalid scheme"));
        }
        p.scheme = Some(scheme.to_ascii_lowercase());
        rest = &rest[colon + 1..];
    }
    if absolute && p.scheme.is_none() {
        return Err(fail("missing scheme"));
    }
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find('/').unwrap_or(after.len());
        p.authority = Some(parse_authority(&after[..end]).map_err(|why| fail(&why))?);
        rest = &after[end..];
    }
    p.path = rest.to_string();
    Ok(p)
}

fn parse_authority(auth: &str) -> Result<Authority, String> {
    let (userinfo, hostport) = match auth.rfind('@') {
        Some(i) => (Some(&auth[..i]), &auth[i + 1..]),
        None => (None, auth),
    };
    if userinfo.is_some_and(|u| u.contains(['[', ']', '@'])) {
        return Err("invalid userinfo".into());
    }
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let Some(close) = inner.find(']') else { return Err("unterminated IPv6 literal".into()) };
        let host = &inner[..close];
        let valid = !host.is_empty()
            && host.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.' | b'%' | b'v' | b'V') || is_unreserved(b));
        if !valid {
            return Err("invalid IPv6 literal".into());
        }
        let after = &inner[close + 1..];
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return Err("unexpected characters after IPv6 literal".into()),
        }
    } else {
        let (host, port) = match hostport.rfind(':') {
            Some(i) => (&hostport[..i], Some(&hostport[i + 1..])),
            None => (hostport, None),
        };
        if !host.bytes().all(|b| is_unreserved(b) || is_sub_delim(b) || b == b'%' || b >= 0x80) {
            return Err("invalid host".into());
        }
        (host, port)
    };
    let port = match port {
        None | Some("") => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if p.bytes().all(|b| b.is_ascii_digit()) => Some(n as i64),
            _ => return Err("invalid port".into()),
        },
    };
    Ok(Authority { userinfo: userinfo.map(str::to_string), host: host.to_string(), port })
}

/// Resolves reference `r` against `base` (RFC 3986 §5.2.2, strict).
fn resolve(base: &Parts, r: Parts) -> Parts {
    if r.scheme.is_some() {
        return Parts { path: remove_dot_segments(&r.path), ..r };
    }
    let mut t = Parts { scheme: base.scheme.clone(), fragment: r.fragment, ..Parts::default() };
    if r.authority.is_some() {
        t.authority = r.authority;
        t.path = remove_dot_segments(&r.path);
        t.query = r.query;
        return t;
    }
    t.authority = base.authority.clone();
    if r.path.is_empty() {
        t.path = base.path.clone();
        t.query = r.query.or_else(|| base.query.clone());
    } else {
        t.path = if r.path.starts_with('/') { remove_dot_segments(&r.path) } else { remove_dot_segments(&merge(base, &r.path)) };
        t.query = r.query;
    }
    t
}

/// RFC 3986 §5.2.3.
fn merge(base: &Parts, path: &str) -> String {
    if base.authority.is_some() && base.path.is_empty() {
        return format!("/{path}");
    }
    match base.path.rfind('/') {
        Some(i) => format!("{}{path}", &base.path[..=i]),
        None => path.to_string(),
    }
}

/// RFC 3986 §5.2.4.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut out = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(r) = input.strip_prefix("../").or_else(|| input.strip_prefix("./")) {
            input = r;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            let cut = out.rfind('/').unwrap_or(0);
            out.truncate(cut);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |i| i + start);
            out.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    out
}

fn encode_native(vm: &mut Vm, s: Value, keep: &[u8]) -> NResult {
    vm.reserve(vm.str(s).len() * 3)?;
    vm.charge_slow_bytes(vm.str(s).len())?;
    let out = encode(vm.str(s), keep);
    Ok(vm.new_str(out))
}

/// Percent-encodes every byte that is not unreserved or in `keep`.
pub(crate) fn encode(s: &str, keep: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) || keep.contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Strictly decodes `%XX` escapes.
pub(crate) fn decode_component(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            match (b.get(i + 1).copied().and_then(hex_val), b.get(i + 2).copied().and_then(hex_val)) {
                (Some(h), Some(l)) => out.push(h << 4 | l),
                _ => return Err(format!("invalid percent-encoding at offset {i}")),
            }
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "percent-decoded text is not valid UTF-8".to_string())
}

/// Leniently decodes a query key or value: `+` is a space, malformed escapes
/// are kept literally and invalid UTF-8 is replaced.
fn decode_query_part(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                if let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex_val), b.get(i + 2).copied().and_then(hex_val)) {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            (decode_query_part(k), decode_query_part(v))
        })
        .collect()
}
