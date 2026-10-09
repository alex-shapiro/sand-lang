//! What the `http` clients (over TCP natively, over `fetch` on the web)
//! share: URLs and header syntax. (Resolving redirects is for the native
//! client: the web's does not follow them.)

use super::stream::IoFailure;
use crate::vm::value::{Obj, Value};
use crate::vm::{NResult, Vm};

// ----------------------------------------------------------------------
// URLs

/// An absolute `http` or `https` URL.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Url {
    pub scheme: String,
    /// Lower-cased host, without brackets for IPv6 literals.
    pub host: String,
    pub port: u16,
    /// Path and query (the request target); never empty.
    pub target: String,
}

pub(crate) fn parse_error(text: &str, why: &str) -> IoFailure {
    IoFailure::new("parse", format!("invalid URL {:?}: {why}", super::truncate(text, 200)))
}

pub(crate) fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// Splits off a leading `scheme:` if `s` starts with one.
pub(crate) fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = s.split_once(':')?;
    let mut chars = scheme.chars();
    let valid =
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

impl Url {
    pub(crate) fn parse(text: &str) -> Result<Url, IoFailure> {
        if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(parse_error(text, "contains whitespace or control characters"));
        }
        let Some((scheme, rest)) = split_scheme(text) else { return Err(parse_error(text, "missing scheme")) };
        let Some(rest) = rest.strip_prefix("//") else { return Err(parse_error(text, "missing `//` after the scheme")) };
        let scheme = scheme.to_ascii_lowercase();
        let Some(default) = default_port(&scheme) else {
            return Err(IoFailure::new(
                "invalid",
                format!("unsupported URL scheme {:?} (expected http or https)", super::truncate(&scheme, 64)),
            ));
        };
        // The browser does TLS on the web.
        if scheme == "https" && !cfg!(any(feature = "tls", web)) {
            return Err(IoFailure::new("invalid", "https is not supported: TLS support is not enabled in this build"));
        }
        let rest = rest.split('#').next().unwrap_or("");
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, target) = rest.split_at(split);
        // Credentials in URLs are not supported; they are ignored.
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
        let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
            let Some((host, after)) = v6.split_once(']') else { return Err(parse_error(text, "unterminated IPv6 address")) };
            (host, after.strip_prefix(':'))
        } else {
            match host_port.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (host_port, None),
            }
        };
        if host.is_empty() {
            return Err(parse_error(text, "missing host"));
        }
        if host.contains(['/', '\\', '[', ']', '@']) {
            return Err(parse_error(text, "invalid host"));
        }
        let port = match port {
            None | Some("") => default,
            Some(p) => p.parse().map_err(|_| parse_error(text, "invalid port"))?,
        };
        let target = if target.starts_with('/') { target.to_string() } else { format!("/{target}") };
        Ok(Url { scheme, host: host.to_ascii_lowercase(), port, target })
    }

    /// `host[:port]` as sent in the `Host` header (the port only if non-default).
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if Some(self.port) == default_port(&self.scheme) { host } else { format!("{host}:{}", self.port) }
    }

    pub(crate) fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }

    #[cfg(feature = "os")]
    /// The path part of the target.
    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }
}

#[cfg(feature = "os")]
/// Removes `.` and `..` segments from an absolute path (RFC 3986 §5.2.4).
pub(crate) fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

#[cfg(feature = "os")]
/// Resolves a `Location` header value against `base`.
pub(crate) fn resolve(base: &str, location: &str) -> Result<String, IoFailure> {
    let location = location.trim();
    if split_scheme(location).is_some() {
        Url::parse(location)?;
        return Ok(location.split('#').next().unwrap_or(location).to_string());
    }
    let base = Url::parse(base)?;
    let location = location.split('#').next().unwrap_or("");
    if location.starts_with("//") {
        return resolve_absolute(&format!("{}:{location}", base.scheme));
    }
    let (path, query) = match location.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (location, None),
    };
    let path = if path.is_empty() {
        base.path().to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        let dir = &base.path()[..base.path().rfind('/').map_or(0, |i| i + 1)];
        format!("{dir}{path}")
    };
    let query = match query {
        Some(q) => format!("?{q}"),
        // An empty reference keeps the base query.
        None if location.is_empty() => base.target[base.path().len()..].to_string(),
        None => String::new(),
    };
    Ok(format!("{}{}{query}", base.origin(), remove_dot_segments(&path)))
}

#[cfg(feature = "os")]
pub(crate) fn resolve_absolute(url: &str) -> Result<String, IoFailure> {
    Url::parse(url)?;
    Ok(url.to_string())
}

// ----------------------------------------------------------------------
// Message syntax

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub(crate) fn valid_header(name: &str, value: &str) -> bool {
    is_token(name) && !value.contains(['\r', '\n', '\0'])
}

/// Converts a `[str: str]` map into validated header pairs.
pub(crate) fn header_pairs(vm: &Vm, map: Value) -> Result<Vec<(String, String)>, String> {
    let Obj::Map(m) = vm.heap.get(map) else { return Ok(vec![]) };
    let mut pairs = Vec::new();
    for (k, v) in m.entries.iter().flatten() {
        let (k, v) = (vm.str(*k), vm.str(*v));
        if !valid_header(k, v) {
            return Err(format!("invalid header {:?}: {:?}", super::truncate(k, 64), super::truncate(v, 64)));
        }
        pairs.push((k.to_string(), v.to_string()));
    }
    Ok(pairs)
}

/// Builds a `[str: str]` map from header pairs.
pub(crate) fn header_map(vm: &mut Vm, headers: Vec<(String, String)>) -> NResult {
    let map = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in headers {
        let k = vm.new_str(k);
        let v = vm.new_str(v);
        vm.map_insert(map, k, v).map_err(crate::vm::meter::ctl_from_message)?;
    }
    Ok(map)
}
