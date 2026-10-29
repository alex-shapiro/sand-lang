//! Host patterns, URL patterns, and IP address classification.

use super::path::{Seg, Tok};
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NetPattern {
    pub kind: HostKind,
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HostKind {
    Any,
    Suffix(String),
    Host(String),
    Ip(IpAddr, u8),
}

/// Validates a pattern for [`super::RulePolicy::allow_net`].
pub fn check_net_pattern(pattern: &str) -> Result<(), String> {
    NetPattern::parse(pattern).map(drop)
}

/// Returns true if `pattern` is valid for [`super::RulePolicy::allow_listen`].
pub fn is_valid_listen_pattern(pattern: &str) -> bool {
    ListenPattern::parse(pattern).is_some()
}

#[cfg(feature = "os")]
pub(crate) fn is_valid_host(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok() || canonical_hostname(host).is_some()
}

fn canonical_hostname(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let label_ok = |l: &str| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b));
    (name.len() <= 253 && name.split('.').all(label_ok)).then(|| name.to_ascii_lowercase())
}

impl NetPattern {
    pub(crate) fn parse(pattern: &str) -> Result<NetPattern, String> {
        let invalid =
            || format!("invalid host pattern `{pattern}` (expected a host, host:port, *.domain, *, an IP address or a CIDR range)");
        let parsed = NetPattern::parse_unmasked(pattern).ok_or_else(invalid)?;
        if let HostKind::Ip(ip, len) = parsed.kind
            && mask(ip, len) != ip
        {
            let range = NetPattern { kind: HostKind::Ip(mask(ip, len), len), ..parsed };
            return Err(format!("`{pattern}` has bits set after its prefix length: write `{range}`"));
        }
        Ok(parsed)
    }

    fn parse_unmasked(pattern: &str) -> Option<NetPattern> {
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
            let len: u8 = len.parse().ok().filter(|_| len.bytes().all(|b| b.is_ascii_digit()))?;
            match ip {
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() && len >= 96 => HostKind::Ip(ip.to_canonical(), len - 96),
                _ if len <= full_len(ip) => HostKind::Ip(ip, len),
                _ => return None,
            }
        } else if let Some(ip) = body_ip(&body) {
            HostKind::Ip(ip, full_len(ip))
        } else {
            HostKind::Host(canonical_hostname(&body)?)
        };
        Some(NetPattern { kind, port })
    }

    /// The specificity of the pattern: its host's, then whether it names a port.
    pub(crate) fn rank(&self) -> u32 {
        self.host_rank() * 2 + self.port.is_some() as u32
    }

    /// The specificity of the pattern's host.
    pub(crate) fn host_rank(&self) -> u32 {
        match &self.kind {
            HostKind::Host(_) => 3000,
            HostKind::Ip(ip, len) if *len == full_len(*ip) => 3000,
            // An IPv4 range is as narrow as the IPv6 range that embeds it in
            // its last 32 bits, so a deny rule for an IPv4 range outranks
            // an IPv6 range that contains its embedded forms.
            HostKind::Ip(IpAddr::V4(_), len) => 2096 + *len as u32,
            HostKind::Ip(_, len) => 2000 + *len as u32,
            // Every name under `localhost` is the loopback host (RFC 6761):
            // such a pattern names one destination, more narrowly than any
            // range or other domain.
            HostKind::Suffix(s) if is_localhost(s) => 2900 + s.split('.').count() as u32,
            HostKind::Suffix(s) => 2000 + s.split('.').count() as u32,
            HostKind::Any => 0,
        }
    }

    pub(crate) fn covers(&self, other: &NetPattern) -> bool {
        self.kind.covers(&other.kind) && self.port.is_none_or(|p| other.port == Some(p))
    }

    pub(crate) fn intersection(&self, other: &NetPattern) -> Option<NetPattern> {
        let port = match (self.port, other.port) {
            (Some(a), Some(b)) if a != b => return None,
            (a, b) => a.or(b),
        };
        let kind = if self.kind.covers(&other.kind) {
            &other.kind
        } else if other.kind.covers(&self.kind) {
            &self.kind
        } else {
            return None;
        };
        Some(NetPattern { kind: kind.clone(), port })
    }

    /// Whether some destination may match both patterns. A pattern that is
    /// not `exact` (of a deny or prompt rule) also matches the alternate
    /// representations of its addresses.
    pub(crate) fn may_meet(&self, other: &NetPattern, exact: bool, other_exact: bool) -> bool {
        let overlap = |a: &NetPattern, b: &NetPattern| a.kind.covers(&b.kind) || b.kind.covers(&a.kind);
        let reaches =
            |strict: &NetPattern, exact: &NetPattern| overlap(strict, exact) || strict.reaching_ranges().iter().any(|r| overlap(r, exact));
        let hosts = match (&self.kind, &other.kind) {
            (HostKind::Ip(..), HostKind::Ip(..)) => match (exact, other_exact) {
                (true, true) => overlap(self, other),
                (false, true) => reaches(self, other),
                (true, false) => reaches(other, self),
                (false, false) => true,
            },
            (HostKind::Any, _) | (_, HostKind::Any) => true,
            (HostKind::Host(h), HostKind::Host(g)) => h == g,
            (HostKind::Suffix(s), HostKind::Host(h)) | (HostKind::Host(h), HostKind::Suffix(s)) => is_under(h, s),
            (HostKind::Suffix(s), HostKind::Suffix(t)) => s == t || is_under(s, t) || is_under(t, s),
            _ => false,
        };
        hosts && (self.port.is_none() || other.port.is_none() || self.port == other.port)
    }

    pub(crate) fn reaching_ranges(&self) -> Vec<NetPattern> {
        let HostKind::Ip(net, len) = self.kind else { return Vec::new() };
        let range = |ip: IpAddr, len: u8| NetPattern { kind: HostKind::Ip(ip, len), port: self.port };
        let v6 = |bits: u128| IpAddr::V6(Ipv6Addr::from(bits));
        let mut ranges = Vec::new();
        let mut v4 = Vec::new();
        if let IpAddr::V4(net) = net {
            v4.push((net, len));
        }
        let local = [Ipv4Addr::UNSPECIFIED.into(), Ipv6Addr::UNSPECIFIED.into(), Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()];
        if local.into_iter().any(|ip| self.kind.matches_ip(ip)) {
            ranges.extend([range(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 32), range(v6(0), 128)]);
            v4.push((Ipv4Addr::UNSPECIFIED, 32));
        }
        for (net, len) in v4 {
            let bits = u32::from(net) as u128;
            ranges.extend(EMBEDDINGS.map(|(prefix, prefix_len, shift)| range(v6(prefix | bits << shift), prefix_len + len)));
        }
        ranges
    }

    pub(crate) fn denies_ip(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        std::iter::once(ip).chain(alternate_ips(ip)).any(|ip| self.kind.matches_ip(ip))
    }
}

impl HostKind {
    fn covers(&self, other: &HostKind) -> bool {
        match (self, other) {
            (HostKind::Any, _) => true,
            (HostKind::Suffix(s), HostKind::Suffix(t)) => s == t || is_under(t, s),
            (HostKind::Suffix(s), HostKind::Host(h)) => is_under(h, s),
            (HostKind::Host(h), HostKind::Host(g)) => h == g,
            (HostKind::Ip(net, len), HostKind::Ip(ip, l)) => len <= l && mask(*ip, *len) == *net,
            _ => false,
        }
    }

    fn matches_ip(&self, ip: IpAddr) -> bool {
        self.covers(&HostKind::Ip(ip, full_len(ip)))
    }
}

pub(crate) struct ProbeTargets {
    label: String,
    port: Option<u16>,
    ranges: Vec<(IpAddr, u8)>,
}

impl ProbeTargets {
    pub(crate) fn new<'a>(patterns: impl Iterator<Item = &'a NetPattern>) -> ProbeTargets {
        let (mut labels, mut ports, mut ranges) = (HashSet::new(), HashSet::new(), Vec::new());
        for p in patterns {
            match &p.kind {
                HostKind::Host(name) | HostKind::Suffix(name) => labels.extend(name.split('.')),
                HostKind::Ip(ip, len) => ranges.push((*ip, *len)),
                HostKind::Any => {}
            }
            ports.extend(p.port);
        }
        let label = (0..).map(|n| format!("x{n}")).find(|l| !labels.contains(l.as_str())).expect("an unused label");
        let port = [443, 80, 8443, 8080].into_iter().chain(0..=u16::MAX).find(|p| !ports.contains(p));
        ProbeTargets { label, port, ranges }
    }

    pub(crate) fn dest(&self, pattern: &NetPattern, denied: &NetPattern) -> Option<(String, u16)> {
        let name = match &pattern.kind {
            HostKind::Any => self.label.clone(),
            HostKind::Suffix(domain) => format!("{}.{domain}", self.label),
            HostKind::Host(name) => name.clone(),
            HostKind::Ip(ip, len) => ProbeTargets::address(*ip, *len, &self.ranges, &|ip| denied.denies_ip(ip))?.1.to_string(),
        };
        Some((name, pattern.port.or(self.port)?))
    }

    fn address(net: IpAddr, len: u8, ranges: &[(IpAddr, u8)], ok: &dyn Fn(IpAddr) -> bool) -> Option<(u8, IpAddr)> {
        let inside: Vec<(IpAddr, u8)> =
            ranges.iter().copied().filter(|&(ip, l)| l > len && ip.is_ipv4() == net.is_ipv4() && mask(ip, len) == net).collect();
        let at = |offset: u128| match net {
            IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) + offset as u32)),
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) + offset)),
        };
        let host_bits = (full_len(net) - len) as u32;
        if inside.is_empty() {
            let last = u128::MAX.checked_shr(128 - host_bits).unwrap_or(0);
            let quality = |ip: IpAddr| 2 * alternate_ips(ip).is_empty() as u8 + !is_private_address(ip) as u8;
            let candidates = [0, 1, 2, last / 2 + 1, last].into_iter().filter(|&o| o <= last).map(at).filter(|&ip| ok(ip));
            return candidates.map(|ip| (quality(ip), ip)).max_by_key(|c| c.0);
        }
        let halves = [net, at(1 << (host_bits - 1))];
        let found = halves.into_iter().filter(|half| !inside.contains(&(*half, len + 1)));
        found.filter_map(|half| ProbeTargets::address(half, len + 1, &inside, ok)).max_by_key(|c| c.0)
    }
}

pub(crate) fn domains_above(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('.').map(|(i, _)| &name[i + 1..])
}

pub(crate) fn enclosing_ranges(ip: IpAddr, len: u8) -> impl Iterator<Item = (IpAddr, u8)> {
    (0..=len).map(move |l| (mask(ip, l), l))
}

fn is_under(name: &str, domain: &str) -> bool {
    name.len() > domain.len() && name.ends_with(domain) && name.as_bytes()[name.len() - domain.len() - 1] == b'.'
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

fn full_len(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}

fn mask(ip: IpAddr, len: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) & u32::MAX.checked_shl(32 - len as u32).unwrap_or(0))),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & u128::MAX.checked_shl(128 - len as u32).unwrap_or(0))),
    }
}

pub(crate) struct Dest<'a> {
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

#[derive(Debug)]
pub(crate) struct HostIndex<T> {
    names: FxHashMap<Box<str>, Ported<T>>,
    suffixes: FxHashMap<Box<str>, Ported<T>>,
    ips: FxHashMap<(IpAddr, u8), Ported<T>>,
    lens: [Vec<u8>; 2],
    any: Ported<T>,
}

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

    /// Calls `f(is_alternate_ip, value)` for each pattern matching `dest`.
    pub(crate) fn find(&self, dest: &Dest, mut f: impl FnMut(bool, &T)) {
        let mut each = |list: Option<&Ported<T>>| {
            for (port, t) in list.into_iter().flatten() {
                if port.is_none_or(|p| p == dest.port) {
                    f(false, t);
                }
            }
        };
        each(Some(&self.any));
        if let Some(ip) = dest.ip {
            self.find_ip(ip, dest.port, f);
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
            each(self.names.get(name));
        }
        if !self.suffixes.is_empty() {
            for domain in domains_above(name) {
                each(self.suffixes.get(domain));
            }
        }
    }

    pub(crate) fn find_ip(&self, ip: IpAddr, port: u16, mut f: impl FnMut(bool, &T)) {
        if self.ips.is_empty() {
            return;
        }
        let ip = ip.to_canonical();
        let alternates = alternate_ips(ip);
        for (i, ip) in std::iter::once(ip).chain(alternates).enumerate() {
            for &len in &self.lens[ip.is_ipv6() as usize] {
                for (p, t) in self.ips.get(&(mask(ip, len), len)).into_iter().flatten() {
                    if p.is_none_or(|p| p == port) {
                        f(i > 0, t);
                    }
                }
            }
        }
    }
}

fn alternate_ips(ip: IpAddr) -> Vec<IpAddr> {
    if ip.is_unspecified() {
        let other = if ip.is_ipv4() { IpAddr::V6(Ipv6Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::UNSPECIFIED) };
        return vec![other, IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
    }
    match ip {
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(v4) => std::iter::once(IpAddr::V4(v4)).chain(alternate_ips(IpAddr::V4(v4))).collect(),
            None => vec![],
        },
        IpAddr::V4(_) => vec![],
    }
}

/// NAT64, 6to4, IPv4-translated, and IPv4-compatible IPv6 prefixes: prefix bits, prefix length, and IPv4 bit shift.
const EMBEDDINGS: [(u128, u8, u32); 4] = [(0x64_ff9b << 96, 96, 0), (0x2002 << 112, 16, 80), (0xffff << 48, 96, 0), (0, 96, 0)];

fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let bits = u128::from(ip);
    let (_, _, shift) = EMBEDDINGS.iter().find(|&&(prefix, len, _)| bits > 1 && mask(ip.into(), len) == Ipv6Addr::from(prefix))?;
    Some(Ipv4Addr::from((bits >> shift) as u32))
}

pub(crate) fn parse_ip(s: &str) -> Option<IpAddr> {
    body_ip(s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(s))
}

fn body_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

/// Whether `host` is `localhost` or a name under it, which RFC 6761 reserves
/// for the loopback host.
pub(crate) fn is_localhost(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host).as_bytes();
    let tail = b".localhost";
    host.eq_ignore_ascii_case(&tail[1..]) || (host.len() > tail.len() && host[host.len() - tail.len()..].eq_ignore_ascii_case(tail))
}

/// The internal address ranges: every IP address outside them (and outside
/// them in its IPv4 form, if it embeds an IPv4 address) is a public unicast
/// internet address.
const INTERNAL_RANGES: [&str; 24] = [
    "0.0.0.0/8",       // unspecified ("this network")
    "10.0.0.0/8",      // RFC 1918
    "100.64.0.0/10",   // shared address space (CGNAT)
    "127.0.0.0/8",     // loopback
    "169.254.0.0/16",  // link-local
    "172.16.0.0/12",   // RFC 1918
    "192.0.0.0/24",    // IETF protocol assignments
    "192.0.2.0/24",    // documentation
    "192.88.99.0/24",  // 6to4 relay anycast
    "192.168.0.0/16",  // RFC 1918
    "198.18.0.0/15",   // benchmarking
    "198.51.100.0/24", // documentation
    "203.0.113.0/24",  // documentation
    "224.0.0.0/3",     // multicast, reserved, and broadcast
    "::/96",           // unspecified, loopback, and IPv4-compatible
    "64:ff9b:1::/48",  // local-use NAT64
    "100::/64",        // discard-only
    "2001::/23",       // IETF protocol assignments
    "2001:db8::/32",   // documentation
    "3fff::/20",       // documentation
    "fc00::/7",        // unique-local
    "fe80::/10",       // link-local
    "fec0::/10",       // site-local
    "ff00::/8",        // multicast
];

/// [`INTERNAL_RANGES`] as patterns.
fn internal_ranges() -> &'static [NetPattern] {
    static PATTERNS: std::sync::LazyLock<Vec<NetPattern>> =
        std::sync::LazyLock::new(|| INTERNAL_RANGES.iter().map(|p| NetPattern::parse(p).expect("a valid range")).collect());
    &PATTERNS
}

/// Returns true if `ip` is not a public unicast internet address.
///
/// Non-public addresses include loopback, private, shared/CGNAT, link-local,
/// unique-local, site-local, unspecified, multicast, broadcast, documentation,
/// benchmarking, and reserved ranges. IPv6 addresses that embed an IPv4
/// address are classified by the embedded address.
pub fn is_private_address(ip: IpAddr) -> bool {
    internal_ranges().iter().any(|p| p.denies_ip(ip))
}

/// Formats `host` and `port` as `host:port`, enclosing IPv6 addresses in brackets.
pub fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

pub(crate) fn format_url_origin(scheme: &str, host: &str, port: u16) -> String {
    let default = if scheme == "http" { 80 } else { 443 };
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    if port == default { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{port}") }
}

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

pub(crate) fn parse_port(text: &str) -> Option<u16> {
    if text.bytes().all(|b| b.is_ascii_digit()) { text.parse().ok() } else { None }
}

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

impl std::fmt::Display for ListenPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.host, self.port) {
            (ListenHost::Any, _) => write!(f, "*")?,
            (ListenHost::Loopback, _) => write!(f, "localhost")?,
            (ListenHost::Ip(ip @ IpAddr::V6(_)), Some(_)) => write!(f, "[{ip}]")?,
            (ListenHost::Ip(ip), _) => write!(f, "{ip}")?,
        }
        match self.port {
            Some(p) => write!(f, ":{p}"),
            None => Ok(()),
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UrlPattern {
    pub host: NetPattern,
    pub path: Vec<Seg>,
}

impl UrlPattern {
    pub(crate) fn parse(text: &[Tok]) -> Result<UrlPattern, String> {
        let flat: String = text.iter().map(|t| t.0).collect();
        if let Some(i) = flat.find("://").filter(|&i| !flat[..i].contains('/')) {
            let rest = &flat[i + 3..];
            return Err(match flat[..i].to_ascii_lowercase().as_str() {
                "http" => format!(
                    "URL patterns take no scheme: write `plaintext {rest}` for `{flat}` (rules match HTTPS unless they say `plaintext`)"
                ),
                "https" => format!("URL patterns take no scheme: write `{rest}` (rules match HTTPS unless they say `plaintext`)"),
                _ => format!("URL patterns take no scheme (remove `{}://`)", &flat[..i]),
            });
        }
        let rest = text;
        let text = |toks: &[Tok]| toks.iter().map(|t| t.0).collect::<String>();
        let slash_from = |from: usize| rest[from..].iter().position(|t| t.0 == '/').map_or(rest.len(), |i| from + i);
        // Ignore `/` inside bracketed IPv6 ranges.
        let mut end =
            slash_from(if rest.first().is_some_and(|t| t.0 == '[') { rest.iter().position(|t| t.0 == ']').unwrap_or(0) } else { 0 });
        // Treat a numeric segment immediately after a bare IP address as a
        // CIDR prefix length so that CIDR ranges in `http` rules are rejected
        // instead of parsed as URL paths.
        let head = text(&rest[..end]);
        let mut address = None;
        if let Ok(NetPattern { kind: HostKind::Ip(ip, len), port: None }) = NetPattern::parse(&head)
            && len == full_len(ip)
            && end < rest.len()
        {
            let next = slash_from(end + 1);
            let after = text(&rest[end + 1..next]);
            let prefix = after.split_once(':').map_or(after.as_str(), |(prefix, _)| prefix);
            let max = if head.contains(':') { 128 } else { 32 };
            if prefix.bytes().all(|b| b.is_ascii_digit()) && prefix.parse::<u8>().is_ok_and(|p| p <= max) {
                end = next;
                address = Some((ip, after));
            }
        }
        let authority = text(&rest[..end]);
        if let Some(NetPattern { kind: HostKind::Ip(ip, len), port }) = NetPattern::parse_unmasked(&authority)
            && len < full_len(ip)
        {
            let range = NetPattern { kind: HostKind::Ip(mask(ip, len), len), port };
            let mut message =
                format!("`{authority}` is a range, and http rules match single addresses: write a net rule for the range (`net {range}`)");
            if let Some((ip, name)) = address {
                let full = if ip.is_ipv6() { format!("[{ip}]/128") } else { format!("{ip}/32") };
                message += &format!(", or `{full}/{name}` for the path `/{name}` on `{ip}`");
            }
            return Err(message);
        }
        let host = NetPattern::parse(&authority)?;
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
        Ok(UrlPattern { host, path })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn url(text: &str) -> Result<UrlPattern, String> {
        UrlPattern::parse(&text.chars().map(|c| (c, false)).collect::<Vec<_>>())
    }

    #[test]
    fn localhost_names_are_the_names_under_localhost() {
        for name in ["localhost", "LOCALHOST", "localhost.", "app.localhost", "a.b.LocalHost.", "x-1.localhost"] {
            assert!(is_localhost(name), "{name}");
        }
        for name in ["localhost.com", "notlocalhost", "local", "app.localhost.com", "xlocalhost", ""] {
            assert!(!is_localhost(name), "{name}");
        }
        // A pattern of names under `localhost` ranks between every range and an exact name.
        let rank = |p: &str| NetPattern::parse(p).unwrap().rank();
        assert!(rank("*.localhost") > rank("[::]/127") && rank("*.localhost") > rank("10.0.0.0/31"));
        assert!(rank("*.a.localhost") > rank("*.localhost") && rank("*.a.localhost") < rank("a.localhost"));
        assert!(rank("*.example.com") < rank("10.0.0.0/8"));
    }

    /// A direct classification, to compare with the internal ranges.
    fn is_private_oracle(ip: IpAddr) -> bool {
        fn v4(ip: Ipv4Addr) -> bool {
            let [a, b, c, _] = ip.octets();
            a == 0
                || a == 10
                || (a == 100 && (64..128).contains(&b))
                || a == 127
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (18..20).contains(&b))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224
        }
        let ip6 = match ip {
            IpAddr::V4(ip) => return v4(ip),
            IpAddr::V6(ip) => ip,
        };
        if let Some(ip) = ip6.to_ipv4_mapped() {
            return v4(ip);
        }
        let embedded = |hi: u16, lo: u16| v4(Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
        match ip6.segments() {
            [0, 0, 0, 0, 0, 0, _, _] => true,
            [0, 0, 0, 0, 0xffff, 0, hi, lo] | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => embedded(hi, lo),
            [0x64, 0xff9b, 1, ..] | [0x100, 0, 0, 0, ..] => true,
            [0x2002, hi, lo, ..] => embedded(hi, lo),
            [0x2001, b, ..] if b < 0x200 => true,
            [0x2001, 0xdb8, ..] => true,
            [0x3fff, b, ..] if b < 0x1000 => true,
            [a, ..] => a & 0xfe00 == 0xfc00 || a & 0xffc0 == 0xfe80 || a & 0xffc0 == 0xfec0 || a & 0xff00 == 0xff00,
        }
    }

    /// Addresses near the edges of the internal ranges and of the forms that
    /// embed IPv4 addresses.
    fn edge_address() -> impl Strategy<Value = IpAddr> {
        let edges: Vec<IpAddr> = INTERNAL_RANGES
            .iter()
            .chain(&["64:ff9b::/96", "2002::/16", "::ffff:0:0/96", "::ffff:0:0:0/96"])
            .flat_map(|r| {
                let NetPattern { kind: HostKind::Ip(ip, len), .. } = NetPattern::parse(r).unwrap() else { unreachable!() };
                let host = |offset: i128| -> IpAddr {
                    match ip {
                        IpAddr::V4(v4) => Ipv4Addr::from((u32::from(v4) as i128 + offset) as u32).into(),
                        IpAddr::V6(v6) => Ipv6Addr::from((u128::from(v6) as i128).wrapping_add(offset) as u128).into(),
                    }
                };
                let size = 1i128.checked_shl((full_len(ip) - len) as u32).unwrap_or(i128::MAX);
                [host(-1), host(0), host(1), host(size - 1), host(size)]
            })
            .collect();
        prop::sample::select(edges)
    }

    proptest! {
        #[test]
        fn internal_ranges_classify_addresses_as_before(
            v4 in any::<u32>(),
            v6 in any::<u128>(),
            embed in 0..EMBEDDINGS.len(),
            edge in edge_address(),
        ) {
            let (prefix, _, shift) = EMBEDDINGS[embed];
            for ip in [
                IpAddr::V4(v4.into()),
                IpAddr::V6(v6.into()),
                IpAddr::V6(Ipv6Addr::from(prefix | (v4 as u128) << shift)),
                IpAddr::V6(Ipv4Addr::from(v4).to_ipv6_mapped()),
                edge,
            ] {
                prop_assert_eq!(is_private_address(ip), is_private_oracle(ip), "{}", ip);
            }
        }

        #[test]
        fn embedded_addresses_are_in_reaching_ranges(form in 0..EMBEDDINGS.len(), v4 in any::<u32>(), noise in any::<u128>()) {
            let (prefix, len, shift) = EMBEDDINGS[form];
            let free = u128::MAX.checked_shr(len as u32 + 32).unwrap_or(0);
            let ip = Ipv6Addr::from(prefix | (v4 as u128) << shift | noise & free);
            let embedded = embedded_ipv4(ip);
            prop_assert_eq!(embedded, (u128::from(ip) > 1).then(|| Ipv4Addr::from(v4)));
            let ranges = NetPattern { kind: HostKind::Ip(Ipv4Addr::from(v4).into(), 32), port: None }.reaching_ranges();
            let reached = ranges.iter().any(|r| r.kind.matches_ip(ip.into()));
            prop_assert!(reached, "{} in none of {:?}", ip, ranges);
        }

        #[test]
        fn url_hosts_are_single_address_net_patterns(
            host in prop::sample::select(vec![
                "*", "*:443", "*.example.com", "a.com:8080", "8.8.8.8", "8.8.8.8:80", "10.0.0.1", "::1", "[::1]", "[::1]:80",
                "10.0.0.0/8", "10.0.0.0/8:443", "[fc00::/7]", "[fc00::]/7", "[fc00::]/7:443", "[fc00::/7]:443", "fc00::/7",
                "::ffff:10.0.0.0/104", "10.0.0.1/8", "[fc00::1/7]", "[::1]:x", "a..b", "10.0.0.1/32", "[::1]/128", "::ffff:10.0.0.1/128",
            ]),
            scheme in prop::sample::select(vec!["", "https://", "http://"]),
            path in prop::collection::vec(prop::sample::select(vec!["a", "8", "24", "128", "2024", "8:1", "*", "**"]), 0..3),
        ) {
            let path: String = path.iter().map(|n| format!("/{n}")).collect();
            let parsed = url(&format!("{scheme}{host}{path}"));
            if !scheme.is_empty() {
                let error = parsed.unwrap_err();
                prop_assert!(error.starts_with("URL patterns take no scheme"), "{}", error);
                return Ok(());
            }
            let bare_address = !host.contains('/') && matches!(NetPattern::parse(host), Ok(NetPattern { kind: HostKind::Ip(ip, len), port: None }) if len == full_len(ip));
            let first = path.split('/').nth(1).map(|n| n.split(':').next().unwrap_or(n));
            let max = if host.contains(':') { 128 } else { 32 };
            let (host, path) = match first.and_then(|n| n.parse::<u8>().ok()) {
                Some(len) if bare_address && len <= max => {
                    let (len, rest) = path[1..].split_once('/').map_or((&path[1..], ""), |(len, rest)| (len, rest));
                    (format!("{host}/{len}"), format!("/{}", rest.trim_start_matches('/')))
                }
                _ => (host.to_string(), path),
            };
            if let Some(NetPattern { kind: HostKind::Ip(ip, len), .. }) = NetPattern::parse_unmasked(&host)
                && len < full_len(ip)
            {
                let error = parsed.unwrap_err();
                prop_assert!(error.starts_with(&format!("`{host}` is a range, and http rules match single addresses")), "{}", error);
                return Ok(());
            }
            match NetPattern::parse(&host) {
                Ok(expected) => {
                    let parsed = parsed.unwrap();
                    prop_assert_eq!(&parsed.host, &expected);
                    prop_assert_eq!(parsed.path, url(&format!("example.com{path}")).unwrap().path);
                }
                Err(e) => prop_assert_eq!(parsed.unwrap_err(), e),
            }
        }
    }
}
