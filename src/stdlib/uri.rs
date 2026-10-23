//! The `uri` module (RFC 3986).

use crate::check::ModKind;
use crate::native::{HostError, NativeModule, Val};
use crate::vm::{Ctl, Vm};

/// A parsed URI (RFC 3986). `host` is `Some` if the URI includes an authority
/// component (`//...`). IPv6 host literals omit the enclosing brackets.
#[derive(crate::SandStruct)]
#[sand(module = "uri")]
struct Uri {
    /// Lowercase URI scheme (such as `"https"`).
    scheme: String,
    /// Optional `userinfo` component before `@`.
    userinfo: Option<String>,
    /// Hostname or IP literal (without brackets for IPv6), if an authority is present.
    host: Option<String>,
    /// Explicit port number, if present.
    port: Option<i64>,
    /// URI path component (may be `""`).
    path: String,
    /// Raw query component after `?`, without the leading `?`.
    query: Option<String>,
    /// Fragment component after `#`, without the leading `#`.
    fragment: Option<String>,
}

impl From<Parts> for Uri {
    fn from(p: Parts) -> Uri {
        let (userinfo, host, port) = match p.authority {
            Some(a) => (a.userinfo, Some(a.host), a.port),
            None => (None, None, None),
        };
        Uri { scheme: p.scheme.unwrap_or_default(), userinfo, host, port, path: p.path, query: p.query, fragment: p.fragment }
    }
}

impl From<Uri> for Parts {
    fn from(u: Uri) -> Parts {
        let authority = u.host.map(|host| Authority { userinfo: u.userinfo, host, port: u.port });
        Parts { scheme: Some(u.scheme), authority, path: u.path, query: u.query, fragment: u.fragment }
    }
}

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("uri", ModKind::Std, include_str!("uri.sand"));
    m.bind::<Uri>();
    m.vm_function("parse", |vm: &mut Vm, text: Val<String>| -> Result<Uri, HostError> {
        let n = text.str(&vm.heap).len();
        vm.check_memory(n.saturating_mul(2))?;
        vm.charge_slow_bytes(n)?;
        Ok(parse(text.str(&vm.heap), true).map_err(|msg| HostError::new("parse", msg))?.into())
    });
    m.vm_function("Uri.join", |vm: &mut Vm, base: Uri, reference: Val<String>| -> Result<Uri, HostError> {
        let base = Parts::from(base);
        let n = base.len().saturating_add(reference.str(&vm.heap).len());
        vm.check_memory(n.saturating_mul(3))?;
        vm.charge_slow_bytes(n)?;
        let r = parse(reference.str(&vm.heap), false).map_err(|msg| HostError::new("parse", msg))?;
        Ok(resolve(&base, r).into())
    });
    m.vm_function("encode_component", |vm: &mut Vm, s: Val<String>| encode_native(vm, s, b""));
    m.vm_function("encode_path", |vm: &mut Vm, s: Val<String>| encode_native(vm, s, b"/"));
    m.vm_function("decode_component", |vm: &mut Vm, s: Val<String>| -> Result<String, HostError> {
        let s = s.str(&vm.heap);
        vm.check_memory(s.len())?;
        vm.charge_slow_bytes(s.len())?;
        decode_component(s).map_err(|msg| HostError::new("parse", msg))
    });
    m.vm_function("parse_query", |vm: &mut Vm, q: Val<String>| -> Result<Vec<(String, String)>, Ctl> {
        let q = q.str(&vm.heap);
        vm.check_memory(q.len().saturating_mul(4))?;
        vm.charge_slow_bytes(q.len())?;
        Ok(parse_query(q))
    });
    m
}

/// The components of a URI reference.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Parts {
    pub scheme: Option<String>,
    pub authority: Option<Authority>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Authority {
    pub userinfo: Option<String>,
    pub host: String,
    pub port: Option<i64>,
}

impl Parts {
    /// The length of the text of the components.
    fn len(&self) -> usize {
        let opt = |s: &Option<String>| s.as_ref().map_or(0, String::len);
        let auth = self.authority.as_ref().map_or(0, |a| opt(&a.userinfo) + a.host.len() + 8);
        opt(&self.scheme) + auth + self.path.len() + opt(&self.query) + opt(&self.fragment)
    }
}

impl std::fmt::Display for Parts {
    /// The URI as text (RFC 3986 §5.3 recomposition).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(s) = &self.scheme {
            write!(f, "{s}:")?;
        }
        if let Some(a) = &self.authority {
            f.write_str("//")?;
            if let Some(u) = &a.userinfo {
                write!(f, "{u}@")?;
            }
            if a.host.contains(':') {
                write!(f, "[{}]", a.host)?
            } else {
                f.write_str(&a.host)?
            }
            if let Some(p) = a.port {
                write!(f, ":{p}")?;
            }
        }
        f.write_str(&self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        if let Some(x) = &self.fragment {
            write!(f, "#{x}")?;
        }
        Ok(())
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
pub(crate) fn parse(text: &str, absolute: bool) -> Result<Parts, String> {
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
pub(crate) fn resolve(base: &Parts, r: Parts) -> Parts {
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
pub(crate) fn remove_dot_segments(path: &str) -> String {
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

fn encode_native(vm: &mut Vm, s: Val<String>, keep: &[u8]) -> Result<String, Ctl> {
    let s = s.str(&vm.heap);
    vm.check_memory(s.len().saturating_mul(3))?;
    vm.charge_slow_bytes(s.len())?;
    Ok(encode(s, keep))
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
