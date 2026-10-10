//! What the `http` clients (over TCP natively, over `fetch` on the web)
//! share: URLs (built on the `uri` module's parser), redirect resolution
//! and header syntax.

use super::stream::IoFailure;
use super::uri;
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, NResult, Vm};

// ----------------------------------------------------------------------
// URLs

/// An absolute `http` or `https` URL (parsed as a URI by `uri::parse`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Url {
    pub scheme: String,
    /// Lower-cased host, without brackets for IPv6 literals.
    pub host: String,
    pub port: u16,
    /// Path and query (the request target); never empty.
    pub target: String,
}

pub(crate) fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

impl Url {
    pub(crate) fn parse(text: &str) -> Result<Url, IoFailure> {
        let parts = uri::parse(text, true).map_err(|e| IoFailure::new("parse", e))?;
        Url::from_parts(text, parts)
    }

    fn from_parts(text: &str, parts: uri::Parts) -> Result<Url, IoFailure> {
        let fail = |why: &str| IoFailure::new("parse", format!("invalid URL {:?}: {why}", super::truncate(text, 200)));
        let scheme = parts.scheme.unwrap_or_default();
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
        // Credentials in URLs are not supported; they are ignored.
        let Some(authority) = parts.authority.filter(|a| !a.host.is_empty()) else { return Err(fail("missing host")) };
        let port = match authority.port {
            Some(p) => u16::try_from(p).map_err(|_| fail("invalid port"))?,
            None => default,
        };
        let path = if parts.path.is_empty() { "/" } else { &parts.path };
        let target = match parts.query {
            Some(q) => format!("{path}?{q}"),
            None => path.to_string(),
        };
        Ok(Url { scheme, host: authority.host.to_ascii_lowercase(), port, target })
    }

    /// `host[:port]` as sent in the `Host` header (the port only if non-default).
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if Some(self.port) == default_port(&self.scheme) { host } else { format!("{host}:{}", self.port) }
    }

    pub(crate) fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }
}

/// Resolves a `Location` header value against `base` (RFC 3986 §5.2,
/// `uri::resolve`), without the fragment.
pub(crate) fn resolve(base: &str, location: &str) -> Result<String, IoFailure> {
    let parse = |text: &str, absolute: bool| uri::parse(text, absolute).map_err(|e| IoFailure::new("parse", e));
    let mut target = uri::resolve(&parse(base, true)?, parse(location.trim(), false)?);
    target.fragment = None;
    let text = target.to_string();
    Url::from_parts(&text, target)?;
    Ok(text)
}

/// The natives `http.resolve` and `http.same_origin`, used by every client.
pub(crate) fn register(r: &mut crate::engine::Registry) {
    r.add_builtin("http.resolve", |vm, a| {
        vm.charge_slow_bytes(vm.str(a[0]).len().saturating_add(vm.str(a[1]).len()))?;
        let resolved = resolve(vm.str(a[0]), vm.str(a[1]));
        Ok(match resolved {
            Ok(url) => {
                vm.reserve(url.len())?;
                let s = vm.new_str(url);
                vm.ok(s)
            }
            Err(e) => vm.err_value(e.kind, &e.message),
        })
    });
    r.add_builtin("http.same_origin", |vm, a| {
        vm.charge_slow_bytes(vm.str(a[0]).len().saturating_add(vm.str(a[1]).len()))?;
        let same = match (Url::parse(vm.str(a[0])), Url::parse(vm.str(a[1]))) {
            (Ok(x), Ok(y)) => x.origin() == y.origin(),
            _ => false,
        };
        Ok(crate::vm::value::Value::Bool(same))
    });
}

// ----------------------------------------------------------------------
// Message syntax

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub(crate) fn valid_header(name: &str, value: &str) -> bool {
    is_token(name) && !value.contains(['\r', '\n', '\0'])
}

/// Converts a `[str: str]` map into validated header pairs; `Ok(Err(..))`
/// describes an invalid header.
pub(crate) fn header_pairs(vm: &Vm, map: Value) -> Result<Result<Vec<(String, String)>, String>, Ctl> {
    let Obj::Map(m) = vm.heap.get(map) else { return Ok(Ok(vec![])) };
    vm.charge(m.live as u64)?;
    let mut pairs = Vec::with_capacity(m.live);
    let mut size = 0usize;
    for (k, v) in m.entries.iter().flatten() {
        let (k, v) = (vm.str(*k), vm.str(*v));
        size = size.saturating_add(k.len() + v.len() + 64);
        vm.charge_bytes(k.len() + v.len())?;
        vm.reserve(size)?;
        if !valid_header(k, v) {
            return Ok(Err(format!("invalid header {:?}: {:?}", super::truncate(k, 64), super::truncate(v, 64))));
        }
        pairs.push((k.to_string(), v.to_string()));
    }
    Ok(Ok(pairs))
}

/// Builds a `[str: str]` map from header pairs.
pub(crate) fn header_map(vm: &mut Vm, headers: Vec<(String, String)>) -> NResult {
    let size = headers.iter().map(|(k, v)| k.len() + v.len() + 128).fold(0usize, usize::saturating_add);
    vm.reserve(size)?;
    vm.charge(headers.len() as u64)?;
    let map = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in headers {
        let k = vm.new_str(k);
        let v = vm.new_str(v);
        vm.map_insert(map, k, v)?;
    }
    Ok(map)
}
