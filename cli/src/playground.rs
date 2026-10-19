//! `sand playground`: serves the web playground (`playground/`) on the
//! local host, so it opens in a browser (which will not run its workers
//! from `file://` pages). A minimal static file server: GET and HEAD of
//! files under the directory, one request per connection, nothing else.
//!
//! Every response isolates the page from other origins (COOP and COEP), so
//! that it may share memory with its workers: a `SharedArrayBuffer` through
//! which it stops a busy engine without terminating the worker. Requests
//! scripts make with `fetch` are CORS requests, which isolation allows.

use crate::runtime;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const DEFAULT_PORT: u16 = 8000;

/// The longest request head read.
const MAX_HEAD: usize = 16 << 10;

/// How long a connection may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The playground's engine, built by `playground/build.sh`.
const ENGINE: &str = "pkg/sandlang_wasm_bg.wasm";

pub(crate) fn cmd_playground(args: &[String]) -> Result<u8, String> {
    let mut dir = None;
    let mut port = DEFAULT_PORT;
    for a in args {
        if let Some(p) = a.strip_prefix("--port=") {
            port = p.parse().map_err(|_| format!("invalid port `{p}`"))?;
        } else if a.starts_with("--") {
            return Err(format!("unknown option `{a}` (`sand playground [DIR] [--port=PORT]`)"));
        } else if dir.is_none() {
            dir = Some(PathBuf::from(a));
        } else {
            return Err(format!("unexpected argument `{a}`"));
        }
    }
    let root = dir.unwrap_or_else(default_dir);
    if !root.join("index.html").is_file() {
        return Err(format!(
            "{} is not the playground's directory (it has no index.html); give its path: sand playground DIR",
            root.display()
        ));
    }
    if !root.join(ENGINE).is_file() {
        return Err(format!(
            "the playground's engine is not built ({} is missing): run playground/build.sh first",
            root.join(ENGINE).display()
        ));
    }
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    runtime()?.block_on(serve(root, port))
}

/// `playground/` of the source tree this binary was built from, if it is
/// still there, else `./playground`.
fn default_dir() -> PathBuf {
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../playground");
    if built.join("index.html").is_file() { built } else { PathBuf::from("playground") }
}

async fn serve(root: PathBuf, port: u16) -> Result<u8, String> {
    // The local host only: the playground is for whoever runs this.
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("port {port} is in use; choose another with --port=PORT")
        } else {
            format!("cannot listen on 127.0.0.1:{port}: {e}")
        }
    })?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    println!("Serving the Sand playground at http://{addr}/ (from {}). Ctrl-C stops.", root.display());
    let root = std::sync::Arc::new(root);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((conn, _)) = accepted else { continue };
                let root = root.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(REQUEST_TIMEOUT, respond(conn, &root)).await;
                });
            }
            Ok(()) = tokio::signal::ctrl_c() => return Ok(0),
        }
    }
}

/// Reads one request from `conn` and answers it.
async fn respond(mut conn: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = conn.read(&mut buf).await?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head[..head.iter().position(|&b| b == b'\r').unwrap_or(0)]).into_owned();
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let response = match method {
        "GET" | "HEAD" => match resolve(root, target) {
            Some(path) => match tokio::task::spawn_blocking(move || std::fs::read(&path).map(|data| (path, data))).await {
                Ok(Ok((path, data))) => Response::ok(content_type(&path), data),
                _ => Response::status(404, "Not Found"),
            },
            None => Response::status(404, "Not Found"),
        },
        _ => Response::status(405, "Method Not Allowed"),
    };
    conn.write_all(&response.head()).await?;
    if method != "HEAD" {
        conn.write_all(&response.body).await?;
    }
    conn.shutdown().await
}

/// The file under `root` that the request target names: `/` and
/// directories are their `index.html`. `None` for paths with `..`, and for
/// anything outside `root` (through a symlink) or not a regular file.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?;
    let decoded = percent_decode(path.strip_prefix('/')?)?;
    let relative = Path::new(&decoded);
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) || decoded.contains(['\\', '\0']) {
        return None;
    }
    let mut file = root.join(relative);
    if file.is_dir() {
        file.push("index.html");
    }
    let file = file.canonicalize().ok()?;
    (file.starts_with(root) && file.is_file()).then_some(file)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "md" | "txt" | "ts" | "sand" | "policy" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn ok(content_type: &'static str, body: Vec<u8>) -> Response {
        Response { status: 200, reason: "OK", content_type, body }
    }

    fn status(status: u16, reason: &'static str) -> Response {
        Response { status, reason, content_type: "text/plain; charset=utf-8", body: format!("{status} {reason}\n").into_bytes() }
    }

    fn head(&self) -> Vec<u8> {
        let allow = if self.status == 405 { "Allow: GET, HEAD\r\n" } else { "" };
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\n\
             Cross-Origin-Opener-Policy: same-origin\r\nCross-Origin-Embedder-Policy: require-corp\r\n{allow}Connection: close\r\n\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        )
        .into_bytes()
    }
}
