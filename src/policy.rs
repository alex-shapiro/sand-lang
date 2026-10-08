//! Sandbox policy: capabilities, decisions, rule-based policies and prompting.

use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// A request to perform an effect outside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    FsRead(PathBuf),
    FsWrite(PathBuf),
    /// Outgoing connection.
    Net {
        host: String,
        port: u16,
    },
    /// Listening socket address (`host:port`).
    NetListen(String),
    /// Environment variable (`"*"` for all).
    Env(String),
    Stdin,
    Stdout,
    Stderr,
    /// Loading a script module.
    Import(PathBuf),
    /// Defined by host native modules.
    Custom {
        name: String,
        detail: String,
    },
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Capability::FsRead(p) => write!(f, "read {}", p.display()),
            Capability::FsWrite(p) => write!(f, "write {}", p.display()),
            Capability::Net { host, port } => write!(f, "connect to {host}:{port}"),
            Capability::NetListen(a) => write!(f, "listen on {a}"),
            Capability::Env(n) => write!(f, "read environment variable {n}"),
            Capability::Stdin => write!(f, "read standard input"),
            Capability::Stdout => write!(f, "write standard output"),
            Capability::Stderr => write!(f, "write standard error"),
            Capability::Import(p) => write!(f, "import {}", p.display()),
            Capability::Custom { name, detail } => write!(f, "{name} {detail}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Prompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

pub type PromptFuture = Pin<Box<dyn Future<Output = PromptAnswer> + Send>>;

/// Asks the user about a capability. While a prompt is pending the engine
/// runs no script code.
pub trait Prompter: Send + Sync {
    fn prompt(&self, cap: Capability) -> PromptFuture;
}

impl<F, Fut> Prompter for F
where
    F: Fn(Capability) -> Fut + Send + Sync,
    Fut: Future<Output = PromptAnswer> + Send + 'static,
{
    fn prompt(&self, cap: Capability) -> PromptFuture {
        Box::pin(self(cap))
    }
}

/// A policy deciding capability requests.
#[derive(Clone, Default)]
pub enum Policy {
    AllowAll,
    #[default]
    DenyAll,
    Rules(Arc<RulePolicy>),
    Custom(Arc<dyn Fn(&Capability) -> Decision + Send + Sync>),
}

impl fmt::Debug for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Policy::AllowAll => write!(f, "AllowAll"),
            Policy::DenyAll => write!(f, "DenyAll"),
            Policy::Rules(r) => write!(f, "Rules({r:?})"),
            Policy::Custom(_) => write!(f, "Custom"),
        }
    }
}

impl Policy {
    /// Everything allowed: only for trusted scripts.
    pub fn allow_all() -> Policy {
        Policy::AllowAll
    }
    /// Everything denied except standard output and error.
    pub fn deny_all() -> Policy {
        Policy::DenyAll
    }
    pub fn rules(r: RulePolicy) -> Policy {
        Policy::Rules(Arc::new(r))
    }
    pub fn custom(f: impl Fn(&Capability) -> Decision + Send + Sync + 'static) -> Policy {
        Policy::Custom(Arc::new(f))
    }

    pub fn check(&self, cap: &Capability) -> Decision {
        match self {
            Policy::AllowAll => Decision::Allow,
            Policy::DenyAll => match cap {
                Capability::Stdout | Capability::Stderr | Capability::Import(_) => Decision::Allow,
                _ => Decision::Deny,
            },
            Policy::Rules(r) => r.check(cap),
            Policy::Custom(f) => f(cap),
        }
    }
}

/// A rule-based policy. Deny rules take precedence over allow rules; requests
/// matching no rule get the fallback decision.
#[derive(Clone, Debug)]
pub struct RulePolicy {
    read_allow: Vec<PathBuf>,
    read_deny: Vec<PathBuf>,
    write_allow: Vec<PathBuf>,
    write_deny: Vec<PathBuf>,
    net_allow: Vec<String>,
    net_deny: Vec<String>,
    listen_allow: Vec<String>,
    env_allow: Vec<String>,
    import_allow: Option<Vec<PathBuf>>,
    stdin: bool,
    stdout: bool,
    stderr: bool,
    custom_allow: Vec<(String, Option<String>)>,
    fallback: Decision,
}

impl Default for RulePolicy {
    fn default() -> Self {
        RulePolicy::new()
    }
}

/// Normalises a path lexically (resolving `.` and `..`) and canonicalises the
/// longest existing prefix (resolving symlinks).
pub fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    // Canonicalise the existing part so symlinks cannot escape.
    let mut existing = out.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut r = c;
            for part in rest.iter().rev() {
                r.push(part);
            }
            return r;
        }
        match existing.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                if !existing.pop() {
                    return out;
                }
            }
            None => return out,
        }
    }
}

fn under(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

fn host_matches(pattern: &str, host: &str, port: u16) -> bool {
    let (ph, pp) = match pattern.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() && !h.is_empty() && !h.ends_with(':') => (h, Some(p.parse::<u16>().unwrap())),
        _ => (pattern, None),
    };
    if let Some(pp) = pp
        && pp != port
    {
        return false;
    }
    let ph = ph.trim_start_matches('[').trim_end_matches(']');
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if ph == "*" {
        return true;
    }
    if let Some(suffix) = ph.strip_prefix("*.") {
        return host.len() > suffix.len() + 1 && host.ends_with(suffix) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.';
    }
    ph.eq_ignore_ascii_case(host)
}

fn listen_matches(pattern: &str, addr: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let (ph, pp) = pattern.rsplit_once(':').unwrap_or((pattern, "0"));
    let (ah, ap) = addr.rsplit_once(':').unwrap_or((addr, "0"));
    let host_ok = ph == "*"
        || ph.trim_matches(['[', ']']) == ah.trim_matches(['[', ']'])
        || (ph == "localhost" && (ah == "127.0.0.1" || ah == "::1"));
    host_ok && (pp == "0" || pp == "*" || pp == ap)
}

impl RulePolicy {
    /// Standard output and error are allowed; script imports are allowed
    /// (they are always confined to the module root); everything else falls
    /// back to `Deny`.
    pub fn new() -> RulePolicy {
        RulePolicy {
            read_allow: vec![],
            read_deny: vec![],
            write_allow: vec![],
            write_deny: vec![],
            net_allow: vec![],
            net_deny: vec![],
            listen_allow: vec![],
            env_allow: vec![],
            import_allow: None,
            stdin: false,
            stdout: true,
            stderr: true,
            custom_allow: vec![],
            fallback: Decision::Deny,
        }
    }
    pub fn allow_read(mut self, p: impl AsRef<Path>) -> Self {
        self.read_allow.push(normalize_path(p.as_ref()));
        self
    }
    pub fn deny_read(mut self, p: impl AsRef<Path>) -> Self {
        self.read_deny.push(normalize_path(p.as_ref()));
        self
    }
    /// Allows writing (and reading) under `p`.
    pub fn allow_write(mut self, p: impl AsRef<Path>) -> Self {
        let n = normalize_path(p.as_ref());
        self.write_allow.push(n.clone());
        self.read_allow.push(n);
        self
    }
    pub fn deny_write(mut self, p: impl AsRef<Path>) -> Self {
        self.write_deny.push(normalize_path(p.as_ref()));
        self
    }
    /// `host`, `host:port`, `*.example.com` or `*`.
    pub fn allow_net(mut self, pattern: &str) -> Self {
        self.net_allow.push(pattern.to_string());
        self
    }
    pub fn deny_net(mut self, pattern: &str) -> Self {
        self.net_deny.push(pattern.to_string());
        self
    }
    /// `host:port`; port `0` allows any port.
    pub fn allow_listen(mut self, addr: &str) -> Self {
        self.listen_allow.push(addr.to_string());
        self
    }
    pub fn allow_env(mut self, name: &str) -> Self {
        self.env_allow.push(name.to_string());
        self
    }
    pub fn allow_stdin(mut self) -> Self {
        self.stdin = true;
        self
    }
    pub fn deny_stdout(mut self) -> Self {
        self.stdout = false;
        self
    }
    pub fn deny_stderr(mut self) -> Self {
        self.stderr = false;
        self
    }
    /// Restricts imports to the given directories.
    pub fn allow_import(mut self, p: impl AsRef<Path>) -> Self {
        self.import_allow.get_or_insert_with(Vec::new).push(normalize_path(p.as_ref()));
        self
    }
    /// Allows a custom capability (`detail = None` allows any detail).
    pub fn allow_custom(mut self, name: &str, detail: Option<&str>) -> Self {
        self.custom_allow.push((name.to_string(), detail.map(String::from)));
        self
    }
    pub fn fallback(mut self, d: Decision) -> Self {
        self.fallback = d;
        self
    }

    pub fn check(&self, cap: &Capability) -> Decision {
        let allow_or = |b: bool| if b { Decision::Allow } else { self.fallback };
        match cap {
            Capability::Stdout => {
                if self.stdout {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Stderr => {
                if self.stderr {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Stdin => allow_or(self.stdin),
            Capability::FsRead(p) => {
                let p = normalize_path(p);
                if under(&p, &self.read_deny) { Decision::Deny } else { allow_or(under(&p, &self.read_allow)) }
            }
            Capability::FsWrite(p) => {
                let p = normalize_path(p);
                if under(&p, &self.write_deny) || under(&p, &self.read_deny) {
                    Decision::Deny
                } else {
                    allow_or(under(&p, &self.write_allow))
                }
            }
            Capability::Net { host, port } => {
                if self.net_deny.iter().any(|pat| host_matches(pat, host, *port)) {
                    Decision::Deny
                } else {
                    allow_or(self.net_allow.iter().any(|pat| host_matches(pat, host, *port)))
                }
            }
            Capability::NetListen(addr) => allow_or(self.listen_allow.iter().any(|pat| listen_matches(pat, addr))),
            Capability::Env(name) => allow_or(self.env_allow.iter().any(|n| n == name || n == "*")),
            Capability::Import(p) => match &self.import_allow {
                None => Decision::Allow,
                Some(roots) => allow_or(under(&normalize_path(p), roots)),
            },
            Capability::Custom { name, detail } => {
                allow_or(self.custom_allow.iter().any(|(n, d)| n == name && d.as_ref().is_none_or(|d| d == detail)))
            }
        }
    }
}
