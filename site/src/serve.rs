//! A static file server for the built site on the local host, so that
//! pages open in a browser (which will not run the playground's workers
//! from `file://` pages): GET and HEAD of files under the directory, one
//! request per connection, nothing else.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The longest request head read.
const MAX_HEAD: usize = 16 << 10;

/// How long a connection may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves `root` on `127.0.0.1:port` until Ctrl-C.
pub fn serve(root: &Path, port: u16) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        // The local host only: the server is for whoever runs it.
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("port {port} is in use; choose another with --port=PORT")
            } else {
                format!("cannot listen on 127.0.0.1:{port}: {e}")
            }
        })?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        println!("Serving the site at http://{addr}/ (from {}). Ctrl-C stops.", root.display());
        tokio::select! {
            () = accept(listener, Arc::new(root)) => Ok(()),
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}

async fn accept(listener: TcpListener, root: Arc<PathBuf>) {
    loop {
        let Ok((conn, _)) = listener.accept().await else { continue };
        let root = root.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, respond(conn, &root)).await;
        });
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
            Some(Found::File(path)) => match tokio::fs::read(&path).await {
                Ok(data) => Response::new(200, "OK", content_type(&path), data),
                Err(_) => not_found(root).await,
            },
            Some(Found::Directory) => {
                let path = target.split(['?', '#']).next().unwrap_or("/");
                let mut r = Response::new(301, "Moved Permanently", "text/plain; charset=utf-8", Vec::new());
                r.location = Some(format!("{path}/"));
                r
            }
            None => not_found(root).await,
        },
        _ => Response::new(405, "Method Not Allowed", "text/plain; charset=utf-8", b"405 Method Not Allowed\n".to_vec()),
    };
    conn.write_all(&response.head()).await?;
    if method != "HEAD" {
        conn.write_all(&response.body).await?;
    }
    conn.shutdown().await
}

async fn not_found(root: &Path) -> Response {
    match tokio::fs::read(root.join("404.html")).await {
        Ok(page) => Response::new(404, "Not Found", "text/html; charset=utf-8", page),
        Err(_) => Response::new(404, "Not Found", "text/plain; charset=utf-8", b"404 Not Found\n".to_vec()),
    }
}

enum Found {
    File(PathBuf),
    /// A directory named without its trailing `/`.
    Directory,
}

/// The file under `root` that the request target names: a directory is its
/// `index.html`. `None` for paths with `..`, and for anything outside
/// `root` (through a symlink) or not a regular file.
fn resolve(root: &Path, target: &str) -> Option<Found> {
    let path = target.split(['?', '#']).next()?;
    let decoded = percent_decode(path.strip_prefix('/')?)?;
    let relative = Path::new(&decoded);
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) || decoded.contains(['\\', '\0']) {
        return None;
    }
    let mut file = root.join(relative);
    if file.is_dir() {
        if !decoded.is_empty() && !decoded.ends_with('/') {
            return Some(Found::Directory);
        }
        file.push("index.html");
    }
    let file = file.canonicalize().ok()?;
    (file.starts_with(root) && file.is_file()).then_some(Found::File(file))
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
        "xml" => "application/xml",
        "md" | "txt" | "ts" | "sand" | "policy" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    location: Option<String>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16, reason: &'static str, content_type: &'static str, body: Vec<u8>) -> Response {
        Response { status, reason, content_type, location: None, body }
    }

    fn head(&self) -> Vec<u8> {
        let mut extra = String::new();
        if self.status == 405 {
            extra.push_str("Allow: GET, HEAD\r\n");
        }
        if let Some(l) = &self.location {
            extra.push_str(&format!("Location: {l}\r\n"));
        }
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\n{extra}Connection: close\r\n\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Sends a request to a server on `port`: (status line and headers, body).
    fn request(port: u16, method: &str, path: &str) -> (String, String) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        s.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    #[test]
    fn serves_files_under_its_directory_only() {
        let dir = std::env::temp_dir().join(format!("sand-site-serve-{}", std::process::id()));
        let site = dir.join("site");
        std::fs::create_dir_all(site.join("guide")).unwrap();
        std::fs::write(site.join("index.html"), "<p>hi</p>").unwrap();
        std::fs::write(site.join("guide/index.html"), "guide").unwrap();
        std::fs::write(site.join("404.html"), "lost").unwrap();
        std::fs::write(site.join("engine.wasm"), "\0asm").unwrap();
        std::fs::write(dir.join("secret.txt"), "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret.txt"), site.join("link.txt")).unwrap();
        let root = site.canonicalize().unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let listener = runtime.block_on(TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || runtime.block_on(accept(listener, Arc::new(root))));

        let (head, body) = request(port, "GET", "/");
        assert!(head.starts_with("HTTP/1.1 200") && head.contains("text/html") && body == "<p>hi</p>", "{head}");
        let (head, body) = request(port, "GET", "/guide/?q=1");
        assert!(head.starts_with("HTTP/1.1 200") && body == "guide", "{head}");
        let (head, _) = request(port, "GET", "/guide");
        assert!(head.starts_with("HTTP/1.1 301") && head.contains("Location: /guide/"), "{head}");
        let (head, _) = request(port, "GET", "/engine.wasm");
        assert!(head.contains("Content-Type: application/wasm"), "{head}");
        for path in ["/../secret.txt", "/%2e%2e/secret.txt", "/missing", "/link.txt"] {
            let (head, body) = request(port, "GET", path);
            assert!(head.starts_with("HTTP/1.1 404") && body == "lost", "{path}: {head}");
        }
        let (head, body) = request(port, "HEAD", "/");
        assert!(head.starts_with("HTTP/1.1 200") && body.is_empty(), "{head}");
        let (head, _) = request(port, "POST", "/");
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
