//! Network destinations: host patterns, URL patterns, and the
//! classification of addresses.

use super::path::{Seg, Tok};
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A host pattern: `host`, `*.domain`, `*`, an IP address or a CIDR range,
/// optionally with a port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NetPattern {
    pub kind: HostKind,
    /// `None` matches any port.
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HostKind {
    /// `*`: any host.
    Any,
    /// `*.example.com`: strict subdomains.
    Suffix(String),
    /// An exact host name (lower case).
    Host(String),
    /// An IP address or CIDR range (`10.0.0.0/8`, `fc00::/7`), masked.
    Ip(IpAddr, u8),
}

/// Returns true if `pattern` is a valid network pattern (see [`super::RulePolicy::allow_net`]).
pub fn is_valid_net_pattern(pattern: &str) -> bool {
    NetPattern::parse(pattern).is_some()
}

/// Returns true if `pattern` is a valid listen pattern (see [`super::RulePolicy::allow_listen`]).
pub fn is_valid_listen_pattern(pattern: &str) -> bool {
    ListenPattern::parse(pattern).is_some()
}

/// Returns true if `host` is an unbracketed IP address or a valid hostname.
#[cfg(feature = "os")]
pub(crate) fn is_valid_host(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok() || canonical_hostname(host).is_some()
}

/// Validates `name` as a hostname (at most 253 bytes of `.`-separated labels of
/// 1 to 63 ASCII alphanumeric, `-`, or `_` characters) and returns it in
/// lowercase without a trailing dot.
fn canonical_hostname(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let label_ok = |l: &str| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b));
    (name.len() <= 253 && name.split('.').all(label_ok)).then(|| name.to_ascii_lowercase())
}

impl NetPattern {
    pub(crate) fn parse(pattern: &str) -> Option<NetPattern> {
        // `[v6]`, `[v6/len]` or `[v6]/len`, each optionally with `:port`;
        // otherwise a port may follow a single colon.
        let (body, port): (String, Option<&str>) = if let Some(rest) = pattern.strip_prefix('[') {
            let (inner, after) = rest.split_once(']')?;
            let (len, port) = match after.strip_prefix('/') {
                Some(len_port) => match len_port.split_once(':') {
                    Some((len, port)) => (Some(len), Some(port)),
                    None => (Some(len_port), None),
                },
                None if after.is_empty() => (None, None),
                None => (None, Some(after.strip_prefix(':')?)),
            };
            if !inner.contains(':') {
                return None;
            }
            (len.map_or_else(|| inner.to_string(), |len| format!("{inner}/{len}")), port)
        } else {
            match pattern.split_once(':') {
                Some((body, port)) if !port.contains(':') => (body.to_string(), Some(port)),
                _ => (pattern.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => Some(parse_port(p)?),
            None => None,
        };
        let kind = if body == "*" {
            HostKind::Any
        } else if let Some(suffix) = body.strip_prefix("*.") {
            HostKind::Suffix(canonical_hostname(suffix)?)
        } else if let Some((ip, len)) = body.split_once('/') {
            let ip: IpAddr = ip.parse().ok()?;
            let len: u8 = len.parse().ok()?;
            match ip {
                // Ranges of IPv4-mapped addresses are IPv4 ranges.
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() && len >= 96 => HostKind::Ip(mask(ip.to_canonical(), len - 96), len - 96),
                IpAddr::V4(_) if len <= 32 => HostKind::Ip(mask(ip, len), len),
                IpAddr::V6(_) if len <= 128 => HostKind::Ip(mask(ip, len), len),
                _ => return None,
            }
        } else if let Some(ip) = body_ip(&body) {
            HostKind::Ip(ip, full_len(ip))
        } else {
            HostKind::Host(canonical_hostname(&body)?)
        };
        Some(NetPattern { kind, port })
    }

    /// True if the pattern specifies explicit hosts or addresses without wildcards.
    pub(crate) fn is_explicit(&self) -> bool {
        matches!(self.kind, HostKind::Host(_) | HostKind::Ip(..))
    }

    /// Specificity score for pattern matching.
    pub(crate) fn score(&self) -> u32 {
        match &self.kind {
            HostKind::Host(_) => 3000,
            HostKind::Ip(ip, len) if *len == full_len(*ip) => 3000,
            HostKind::Ip(_, len) => 2000 + *len as u32,
            HostKind::Suffix(s) => 2000 + s.split('.').count() as u32,
            HostKind::Any => 0,
        }
    }
}

impl std::fmt::Display for NetPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v6 = matches!(self.kind, HostKind::Ip(IpAddr::V6(_), _));
        if v6 && self.port.is_some() {
            write!(f, "[")?;
        }
        match &self.kind {
            HostKind::Any => write!(f, "*")?,
            HostKind::Suffix(s) => write!(f, "*.{s}")?,
            HostKind::Host(h) => write!(f, "{h}")?,
            HostKind::Ip(ip, len) if *len == full_len(*ip) => write!(f, "{ip}")?,
            HostKind::Ip(ip, len) => write!(f, "{ip}/{len}")?,
        }
        if v6 && self.port.is_some() {
            write!(f, "]")?;
        }
        match self.port {
            Some(p) => write!(f, ":{p}"),
            None => Ok(()),
        }
    }
}

pub(crate) fn full_len(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}

/// `ip` with all but its first `len` bits cleared.
pub(crate) fn mask(ip: IpAddr, len: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) & u32::MAX.checked_shl(32 - len as u32).unwrap_or(0))),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & u128::MAX.checked_shl(128 - len as u32).unwrap_or(0))),
    }
}

/// A host or address a request names, as patterns are matched against it.
pub(crate) struct Dest<'a> {
    /// The name without a trailing dot, or the IP literal without brackets.
    pub name: &'a str,
    pub ip: Option<IpAddr>,
    pub port: u16,
}

impl<'a> Dest<'a> {
    pub(crate) fn new(host: &'a str, port: u16) -> Dest<'a> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Dest { name: host.strip_suffix('.').unwrap_or(host), ip: body_ip(host), port }
    }
}

/// Host patterns, indexed so that finding those matching a request takes
/// a few lookups however many there are.
#[derive(Debug)]
pub(crate) struct HostIndex<T> {
    names: FxHashMap<Box<str>, Ported<T>>,
    suffixes: FxHashMap<Box<str>, Ported<T>>,
    ips: FxHashMap<(IpAddr, u8), Ported<T>>,
    /// The prefix lengths of `ips`, IPv4 and IPv6.
    lens: [Vec<u8>; 2],
    any: Ported<T>,
}

/// Values of patterns, with their ports (`None`: any).
type Ported<T> = Vec<(Option<u16>, T)>;

impl<T> Default for HostIndex<T> {
    fn default() -> Self {
        HostIndex {
            names: Default::default(),
            suffixes: Default::default(),
            ips: Default::default(),
            lens: Default::default(),
            any: vec![],
        }
    }
}

impl<T> HostIndex<T> {
    pub(crate) fn insert(&mut self, p: &NetPattern, t: T) {
        let entry = (p.port, t);
        match &p.kind {
            HostKind::Any => self.any.push(entry),
            HostKind::Suffix(s) => self.suffixes.entry(s.as_str().into()).or_default().push(entry),
            HostKind::Host(h) => self.names.entry(h.as_str().into()).or_default().push(entry),
            HostKind::Ip(ip, len) => {
                let lens = &mut self.lens[ip.is_ipv6() as usize];
                if !lens.contains(len) {
                    lens.push(*len);
                }
                self.ips.entry((*ip, *len)).or_default().push(entry);
            }
        }
    }

    /// Calls `f(score, explicit, spelled, t)` for each pattern matching
    /// `dest`: higher scores name it more narrowly; `explicit` patterns
    /// name hosts or addresses without wildcards. A name matches name
    /// patterns, an IP literal address patterns, also by another spelling
    /// of its address (see [`ip_spellings`]) where `spelled`.
    pub(crate) fn find(&self, dest: &Dest, mut f: impl FnMut(u32, bool, bool, &T)) {
        let mut each = |list: Option<&Ported<T>>, score: u32, explicit: bool| {
            for (port, t) in list.into_iter().flatten() {
                if port.is_none_or(|p| p == dest.port) {
                    f(score * 2 + port.is_some() as u32, explicit, false, t);
                }
            }
        };
        each(Some(&self.any), 0, false);
        if let Some(ip) = dest.ip {
            self.find_ip(ip, dest.port, |score, spelled, t| f(score, true, spelled, t));
            return;
        }
        let lower;
        let name = if dest.name.bytes().any(|b| b.is_ascii_uppercase()) {
            lower = dest.name.to_ascii_lowercase();
            &lower
        } else {
            dest.name
        };
        if !self.names.is_empty() {
            each(self.names.get(name), 3000, true);
        }
        if !self.suffixes.is_empty() {
            for (i, _) in name.match_indices('.') {
                let suffix = &name[i + 1..];
                each(self.suffixes.get(suffix), 2000 + suffix.split('.').count() as u32, false);
            }
        }
    }

    /// Like [`HostIndex::find`] for an address, with address patterns only.
    pub(crate) fn find_ip(&self, ip: IpAddr, port: u16, mut f: impl FnMut(u32, bool, &T)) {
        if self.ips.is_empty() {
            return;
        }
        let ip = ip.to_canonical();
        let spellings = ip_spellings(ip);
        for (i, ip) in std::iter::once(ip).chain(spellings).enumerate() {
            for &len in &self.lens[ip.is_ipv6() as usize] {
                for (p, t) in self.ips.get(&(mask(ip, len), len)).into_iter().flatten() {
                    if p.is_none_or(|p| p == port) {
                        let pattern = NetPattern { kind: HostKind::Ip(ip, len), port: *p };
                        f(pattern.score() * 2 + p.is_some() as u32, i > 0, t);
                    }
                }
            }
        }
    }
}

/// Other addresses that reach what `ip` reaches, which deny patterns must
/// also catch: the unspecified address (`0.0.0.0`, `::`) connects to the
/// local host (`127.0.0.1`, `::1`), and IPv6 addresses embedding an IPv4
/// address (IPv4-compatible, IPv4-translated, NAT64 `64:ff9b::/96`, 6to4
/// `2002::/16`) reach that address.
fn ip_spellings(ip: IpAddr) -> Vec<IpAddr> {
    if ip.is_unspecified() {
        return vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
    }
    match ip {
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(v4) => std::iter::once(IpAddr::V4(v4)).chain(ip_spellings(IpAddr::V4(v4))).collect(),
            None => vec![],
        },
        IpAddr::V4(_) => vec![],
    }
}

/// The IPv4 address embedded in an IPv6 address of a form that reaches it:
/// IPv4-compatible (`::a.b.c.d`, not `::` or `::1`), IPv4-translated
/// (`::ffff:0:a.b.c.d`), NAT64 (`64:ff9b::a.b.c.d`) or 6to4 (`2002:aabb:ccdd::`).
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let v4 = |hi: u16, lo: u16| Some(Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
    match ip.segments() {
        [0, 0, 0, 0, 0, 0, 0, 0 | 1] => None,
        [0, 0, 0, 0, 0, 0, hi, lo] | [0, 0, 0, 0, 0xffff, 0, hi, lo] | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => v4(hi, lo),
        [0x2002, hi, lo, ..] => v4(hi, lo),
        _ => None,
    }
}

/// Parses an IP literal (IPv6 optionally in brackets); IPv4-mapped IPv6
/// addresses become IPv4.
pub(crate) fn parse_ip(s: &str) -> Option<IpAddr> {
    body_ip(s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(s))
}

fn body_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

pub(crate) fn is_localhost(host: &str) -> bool {
    host.strip_suffix('.').unwrap_or(host).eq_ignore_ascii_case("localhost")
}

/// Whether `ip` is not a public unicast internet address: loopback, private
/// (RFC 1918), shared/CGNAT (100.64/10), link-local, unique-local
/// (`fc00::/7`), site-local, unspecified, multicast, broadcast, documentation,
/// benchmarking and other reserved ranges. IPv6 forms that embed an IPv4
/// address (IPv4-mapped, IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4
/// `2002::/16`) are classified by the embedded address.
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0                                      // "this network", unspecified
        || a == 10                              // RFC 1918
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || a == 127                             // loopback
        || (a == 169 && b == 254)               // link-local (incl. cloud metadata)
        || (a == 172 && (16..32).contains(&b))  // RFC 1918
        || (a == 192 && b == 0 && c == 0)       // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)       // documentation
        || (a == 192 && b == 88 && c == 99)     // 6to4 relay anycast
        || (a == 192 && b == 168)               // RFC 1918
        || (a == 198 && (18..20).contains(&b))  // benchmarking
        || (a == 198 && b == 51 && c == 100)    // documentation
        || (a == 203 && b == 0 && c == 113)     // documentation
        || a >= 224 // multicast, reserved, broadcast
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| is_private_v4(Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_v4(v4);
    }
    match s {
        // Unspecified, loopback and IPv4-compatible addresses.
        [0, 0, 0, 0, 0, 0, _, _] => true,
        // IPv4-translated (RFC 6145): classified by the embedded IPv4 address.
        [0, 0, 0, 0, 0xffff, 0, hi, lo] => embedded(hi, lo),
        // NAT64 well-known prefix: classified by the embedded IPv4 address.
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => embedded(hi, lo),
        // Local-use NAT64, discard-only.
        [0x64, 0xff9b, 1, ..] | [0x100, 0, 0, 0, ..] => true,
        // 6to4: classified by the embedded IPv4 address.
        [0x2002, hi, lo, ..] => embedded(hi, lo),
        // IETF protocol assignments (incl. Teredo) and documentation.
        [0x2001, b, ..] if b < 0x200 => true,
        [0x2001, 0xdb8, ..] => true,
        [0x3fff, b, ..] if b < 0x1000 => true,
        // Unique local, link-local, site-local, multicast.
        [a, ..] => a & 0xfe00 == 0xfc00 || a & 0xffc0 == 0xfe80 || a & 0xffc0 == 0xfec0 || a & 0xff00 == 0xff00,
    }
}

/// Formats `host` and `port` as `host:port`, enclosing IPv6 addresses in brackets.
pub fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

/// Formats `scheme://host[:port]`, omitting the port if it is the default for `scheme`.
pub(crate) fn format_url_origin(scheme: &str, host: &str, port: u16) -> String {
    let default = if scheme == "http" { 80 } else { 443 };
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    if port == default { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{port}") }
}

/// Splits `host`, `host:port`, `[ipv6]`, or `[ipv6]:port` into an unbracketed
/// host and an optional port string. Unbracketed IPv6 addresses are returned
/// without a port.
pub(crate) fn split_port(text: &str) -> Option<(&str, Option<&str>)> {
    match text.strip_prefix('[') {
        Some(rest) => {
            let (ip, after) = rest.split_once(']')?;
            ip.parse::<Ipv6Addr>().ok()?;
            Some((ip, if after.is_empty() { None } else { Some(after.strip_prefix(':')?) }))
        }
        None => Some(match text.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            _ => (text, None),
        }),
    }
}

/// Parses an ASCII decimal port number.
pub(crate) fn parse_port(text: &str) -> Option<u16> {
    if text.bytes().all(|b| b.is_ascii_digit()) { text.parse().ok() } else { None }
}

/// A parsed listen rule pattern (see [`super::RulePolicy::allow_listen`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ListenPattern {
    host: ListenHost,
    port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ListenHost {
    Any,
    Loopback,
    Ip(IpAddr),
}

impl ListenPattern {
    pub(crate) fn parse(text: &str) -> Option<ListenPattern> {
        let (host, port) = split_port(text)?;
        let host = match host {
            "*" => ListenHost::Any,
            _ if is_localhost(host) => ListenHost::Loopback,
            _ => ListenHost::Ip(parse_ip(host)?),
        };
        let port = match port {
            None | Some("*") => None,
            Some(p) => Some(parse_port(p)?),
        };
        Some(ListenPattern { host, port })
    }

    /// Returns the specificity score if `self` matches `addr`.
    pub(crate) fn matches(&self, addr: SocketAddr) -> Option<u32> {
        let host_ok = match self.host {
            ListenHost::Any => true,
            ListenHost::Loopback => addr.ip().to_canonical().is_loopback(),
            ListenHost::Ip(ip) => addr.ip().to_canonical() == ip,
        };
        let port_ok = self.port.is_none_or(|p| p == addr.port());
        (host_ok && port_ok).then_some(2 * (self.host != ListenHost::Any) as u32 + self.port.is_some() as u32)
    }
}

/// A URL pattern of an `http` rule: `[scheme://]host[:port][/path]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UrlPattern {
    pub scheme: Option<&'static str>,
    pub host: NetPattern,
    pub path: Vec<Seg>,
}

impl UrlPattern {
    /// Parses a pattern from its text. Without a port, a pattern with a
    /// scheme matches the scheme's default port and one without matches
    /// any port. Path names are percent-decoded.
    pub(crate) fn parse(text: &[Tok]) -> Result<UrlPattern, String> {
        let flat: String = text.iter().map(|t| t.0).collect();
        let (scheme, rest) = match flat.find("://").filter(|&i| !flat[..i].contains('/')) {
            Some(i) => match flat[..i].to_ascii_lowercase().as_str() {
                "http" => (Some("http"), &text[7..]),
                "https" => (Some("https"), &text[8..]),
                _ => return Err(format!("unsupported scheme `{}` (expected http or https)", &flat[..i])),
            },
            None => (None, text),
        };
        let end = rest.iter().position(|t| t.0 == '/').unwrap_or(rest.len());
        let authority: String = rest[..end].iter().map(|t| t.0).collect();
        let mut host = NetPattern::parse(&authority)
            .filter(|p| !matches!(p.kind, HostKind::Ip(ip, len) if len != full_len(ip)))
            .ok_or_else(|| format!("invalid host `{authority}` (expected a host, host:port, *.domain, * or an IP address)"))?;
        if host.port.is_none() {
            host.port = scheme.map(|s| if s == "http" { 80 } else { 443 });
        }
        let mut path = Vec::new();
        for seg in rest.get(end + 1..).unwrap_or_default().split(|t| t.0 == '/' && !t.1) {
            if seg.is_empty() {
                continue;
            }
            if seg.iter().any(|t| t.0 == '?' && !t.1) {
                return Err("URL patterns match paths, not queries (remove the `?...`)".into());
            }
            let seg = Seg::parse(seg)?.map(|s| percent_decode(s).into_owned());
            if matches!(&seg, Seg::Lit(s) if &**s == "." || &**s == "..") {
                return Err("URL patterns cannot contain `.` or `..` segments".into());
            }
            path.push(seg);
        }
        while path.last() == Some(&Seg::GlobStar) {
            path.pop();
        }
        Ok(UrlPattern { scheme, host, path })
    }
}

/// `s` with `%XX` escapes decoded, if they all are valid and decode to
/// UTF-8.
pub(crate) fn percent_decode(s: &str) -> Cow<'_, str> {
    if !s.contains('%') {
        return s.into();
    }
    let hex = |b: Option<&u8>| b.and_then(|b| (*b as char).to_digit(16));
    let (b, mut out, mut i) = (s.as_bytes(), Vec::with_capacity(s.len()), 0);
    while i < b.len() {
        if b[i] == b'%' {
            let (Some(h), Some(l)) = (hex(b.get(i + 1)), hex(b.get(i + 2))) else { return s.into() };
            out.push((h << 4 | l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_or(s.into(), Cow::Owned)
}

/// The names of a URL path as rules match them: split at `/`,
/// percent-decoded, with empty names dropped and `.` and `..` resolved.
/// Also returns, if a name decodes to text containing `/` or `\` (which a
/// server may take as separators), the names split there too.
pub(crate) fn url_segments(path: &str) -> (Vec<String>, Option<Vec<String>>) {
    let decoded: Vec<Cow<str>> = path.split('/').map(percent_decode).collect();
    let main = resolve_dots(decoded.iter().map(|n| &**n));
    let alt = decoded.iter().any(|n| n.contains(['/', '\\'])).then(|| resolve_dots(decoded.iter().flat_map(|n| n.split(['/', '\\']))));
    (main, alt)
}

fn resolve_dots<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        match name {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            n => out.push(n.to_string()),
        }
    }
    out
}
