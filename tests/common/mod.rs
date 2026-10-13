//! Shared helpers for the acceptance test suite.
#![allow(dead_code)]

use sandlang::{Config, Engine, Error, MemorySink, Outcome, Policy};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh, empty temporary directory, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sandlang-test-{}-{}-{}-{}",
            tag.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect::<String>(),
            std::process::id(),
            n,
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(sandlang::policy::normalize_path(&dir))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct RunResult {
    pub result: Result<Outcome, Error>,
    pub stdout: String,
    pub stderr: String,
}

impl RunResult {
    pub fn ok_stdout(&self) -> &str {
        if let Err(e) = &self.result {
            panic!("run failed: {e}\nstdout:\n{}\nstderr:\n{}", self.stdout, self.stderr);
        }
        &self.stdout
    }

    /// The message of the error the run failed with.
    pub fn failure(&self) -> &str {
        match &self.result {
            Err(Error::Runtime { message, .. }) => message,
            other => panic!("expected a failure, got {other:?}\nstdout:\n{}\nstderr:\n{}", self.stdout, self.stderr),
        }
    }
}

pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Runs `source` as `main.sand` in a fresh temp dir with an allow-all policy.
pub fn run(source: &str) -> RunResult {
    run_with(source, |c| c)
}

/// Like [`run`], configured by `f`. With the `os` feature the script is a
/// file of a temporary directory of the host, otherwise of a memory file
/// system.
pub fn run_with(source: &str, f: impl FnOnce(Config) -> Config) -> RunResult {
    let out = MemorySink::new();
    let err = MemorySink::new();
    let config = Config::new().policy(Policy::allow_all()).stdout(out.clone()).stderr(err.clone());
    #[cfg(feature = "os")]
    let (_dir, main, config) = {
        let dir = TempDir::new("run");
        let main = dir.write("main.sand", source);
        let config = config.cwd(dir.path());
        (dir, main, config)
    };
    #[cfg(not(feature = "os"))]
    let (main, config) = {
        let fs = sandlang::MemFs::new();
        fs.write("/run/main.sand", source).unwrap();
        (PathBuf::from("/run/main.sand"), config.memory_fs(fs).cwd("/run"))
    };
    let mut engine = Engine::new(f(config));
    let result = rt().block_on(engine.run_file(&main));
    RunResult { result, stdout: out.contents(), stderr: err.contents() }
}

/// Asserts that a program prints exactly `expected`.
pub fn expect_output(source: &str, expected: &str) {
    let r = run(source);
    assert_eq!(r.ok_stdout(), expected, "source:\n{source}");
}

// ---------------------------------------------------------------------------
// A tiny HTTP/1.1 test server (runs on its own thread with its own runtime).

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap()
    }
    pub fn query(&self) -> &str {
        self.target.split_once('?').map(|(_, q)| q).unwrap_or("")
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Send the body with chunked transfer encoding.
    pub chunked: bool,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> HttpResponse {
        HttpResponse { status, headers: vec![], body: body.into(), chunked: false }
    }
    pub fn header(mut self, k: &str, v: &str) -> HttpResponse {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

pub struct TestServer {
    pub addr: std::net::SocketAddr,
    pub requests: std::sync::Arc<std::sync::Mutex<Vec<HttpRequest>>>,
    /// The connections accepted so far.
    pub connections: std::sync::Arc<AtomicUsize>,
}

type Handler = std::sync::Arc<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>;

pub fn http_server<F>(handler: F) -> TestServer
where
    F: Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static,
{
    serve(std::sync::Arc::new(handler), None)
}

/// Like [`http_server`], over TLS with the `localhost` certificate from
/// `tests/tls` (trust `tls_test_root()` to accept it).
#[cfg(feature = "tls")]
pub fn https_server<F>(handler: F) -> TestServer
where
    F: Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static,
{
    serve(std::sync::Arc::new(handler), Some(tls_acceptor()))
}

/// Without TLS support there is no acceptor.
#[cfg(not(feature = "tls"))]
#[derive(Clone)]
enum TlsAcceptor {}
#[cfg(feature = "tls")]
use tokio_rustls::TlsAcceptor;

/// The test root CA (DER) that issued the `localhost` certificate.
pub fn tls_test_root() -> Vec<u8> {
    include_bytes!("../tls/ca.der").to_vec()
}

/// A TLS acceptor presenting the `localhost` test certificate.
#[cfg(feature = "tls")]
pub fn tls_acceptor() -> TlsAcceptor {
    use tokio_rustls::rustls::{self, pki_types};
    let cert = pki_types::CertificateDer::from(include_bytes!("../tls/localhost.der").to_vec());
    let key = pki_types::PrivateKeyDer::Pkcs8(pki_types::PrivatePkcs8KeyDer::from(include_bytes!("../tls/localhost.key.der").to_vec()));
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    TlsAcceptor::from(std::sync::Arc::new(config))
}

fn serve(handler: Handler, tls: Option<TlsAcceptor>) -> TestServer {
    use std::sync::{Arc, Mutex};
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reqs = requests.clone();
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = rt();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                let reqs = reqs.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    match tls {
                        None => serve_connection(sock, handler, reqs).await,
                        #[cfg(feature = "tls")]
                        Some(acceptor) => {
                            if let Ok(stream) = acceptor.accept(sock).await {
                                serve_connection(stream, handler, reqs).await
                            }
                        }
                        #[cfg(not(feature = "tls"))]
                        Some(never) => match never {},
                    }
                });
            }
        });
    });
    let addr = rx.recv().unwrap();
    TestServer { addr, requests, connections }
}

/// Answers the requests arriving on one connection.
async fn serve_connection<S>(mut sock: S, handler: Handler, reqs: std::sync::Arc<std::sync::Mutex<Vec<HttpRequest>>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::new();
    loop {
        // Read the head.
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break Some(p);
            }
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let Some(head_end) = head_end else { return };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        buf.drain(..head_end + 4);
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or("").split_whitespace();
        let method = first.next().unwrap_or("").to_string();
        let target = first.next().unwrap_or("").to_string();
        let headers: Vec<(String, String)> =
            lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
        let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
        // Reads until `buf` holds `n` bytes.
        macro_rules! fill {
            ($n:expr) => {
                let n = $n;
                while buf.len() < n {
                    let mut chunk = [0u8; 4096];
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
            };
        }
        let body: Vec<u8> = if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            let mut body = Vec::new();
            loop {
                let line_end = loop {
                    if let Some(p) = buf.windows(2).position(|w| w == b"\r\n") {
                        break p;
                    }
                    fill!(buf.len() + 1);
                };
                let size = usize::from_str_radix(String::from_utf8_lossy(&buf[..line_end]).trim(), 16).unwrap();
                buf.drain(..line_end + 2);
                fill!(size + 2);
                body.extend(buf.drain(..size));
                buf.drain(..2);
                if size == 0 {
                    break body;
                }
            }
        } else {
            let len = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            fill!(len);
            buf.drain(..len).collect()
        };
        let req = HttpRequest { method, target, headers, body };
        reqs.lock().unwrap().push(req.clone());
        // Run the handler off the (single-threaded) runtime so slow handlers overlap.
        let resp = {
            let (handler, req) = (handler.clone(), req.clone());
            tokio::task::spawn_blocking(move || handler(&req)).await.unwrap()
        };
        let mut out = format!("HTTP/1.1 {} X\r\n", resp.status).into_bytes();
        for (k, v) in &resp.headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        if resp.chunked {
            out.extend_from_slice(b"transfer-encoding: chunked\r\n\r\n");
            for piece in resp.body.chunks(7) {
                out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                out.extend_from_slice(piece);
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"0\r\n\r\n");
        } else {
            out.extend_from_slice(format!("content-length: {}\r\n\r\n", resp.body.len()).as_bytes());
            if req.method != "HEAD" {
                out.extend_from_slice(&resp.body);
            }
        }
        if sock.write_all(&out).await.is_err() || sock.flush().await.is_err() {
            return;
        }
        if resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")) {
            return;
        }
    }
}
