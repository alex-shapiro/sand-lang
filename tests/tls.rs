//! HTTPS and `net.connect_tls` against local TLS servers using the test
//! certificates in `tests/tls` (a test root CA and a `localhost` leaf).
#![cfg(feature = "tls")]

mod common;

use common::{HttpResponse, http_server, https_server, run_with, tls_acceptor, tls_test_root};
use sandlang::{Config, Policy, RulePolicy};

/// Allows everything and trusts the test root.
fn trusting(c: Config) -> Config {
    c.policy(Policy::allow_all()).tls_root_certificates(vec![tls_test_root()])
}

/// A TLS server that answers each line with its upper-case form.
fn tls_echo_server() -> std::net::SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        common::rt().block_on(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let acceptor = tls_acceptor();
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(sock).await else { return };
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    while stream.read_line(&mut line).await.is_ok_and(|n| n > 0) {
                        if stream.write_all(line.to_uppercase().as_bytes()).await.is_err() || stream.flush().await.is_err() {
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
    });
    rx.recv().unwrap()
}

#[test]
fn https_get_with_a_trusted_root() {
    let server = https_server(|req| match req.path() {
        "/hello" => HttpResponse::new(200, "secure hello").header("content-type", "text/plain"),
        "/moved" => HttpResponse::new(301, "").header("location", "/hello"),
        "/chunked" => {
            let mut r = HttpResponse::new(200, "chunked over tls");
            r.chunked = true;
            r
        }
        _ => HttpResponse::new(404, ""),
    });
    let base = format!("https://localhost:{}", server.addr.port());
    let src = format!(
        r#"
import http
let r = http.get("{base}/hello")?
print(r.status)
print(r.text()?)
print(r.header("content-type"))
let m = http.get("{base}/moved")?
print(m.url == "{base}/hello")
print(m.text()?)
print(http.post("{base}/chunked", b"data")?.text()?)
use up = http.upload("POST", "{base}/chunked")?
up.write_str("streamed")?
use resp = up.finish()?
print(resp.body.read_text()?)
"#
    );
    let r = run_with(&src, trusting);
    assert_eq!(r.ok_stdout(), "200\nsecure hello\nSome(\"text/plain\")\ntrue\nsecure hello\nchunked over tls\nchunked over tls\n");
    assert_eq!(server.requests.lock().unwrap().len(), 5);
    assert_eq!(server.requests.lock().unwrap()[4].body, b"streamed");
}

#[test]
fn untrusted_certificates_are_rejected() {
    let server = https_server(|_| HttpResponse::new(200, "should not be read"));
    let echo = tls_echo_server();
    let src = format!(
        r#"
import http
import net
let e = http.get("https://localhost:{}/").unwrap_err()
print(e.kind)
print(e.message.contains("certificate"))
let c = net.connect_tls("localhost:{}")
print(c.unwrap_err().message.contains("certificate"))
"#,
        server.addr.port(),
        echo.port()
    );
    // The bundled web PKI roots do not include the test root.
    let r = run_with(&src, |c| c.policy(Policy::allow_all()));
    assert_eq!(r.ok_stdout(), "invalid\ntrue\ntrue\n");
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn certificates_must_match_the_host_name() {
    let server = https_server(|_| HttpResponse::new(200, "should not be read"));
    let echo = tls_echo_server();
    // The certificate names only `localhost`, not the address.
    let src = format!(
        r#"
import http
import net
let e = http.get("https://127.0.0.1:{}/").unwrap_err()
print(e.kind)
print(e.message)
print(net.connect_tls("127.0.0.1:{}").is_err())
"#,
        server.addr.port(),
        echo.port()
    );
    let r = run_with(&src, trusting);
    let out = r.ok_stdout();
    assert!(out.starts_with("invalid\nTLS handshake with 127.0.0.1 failed: "), "{out}");
    assert!(out.contains("not valid for name"), "{out}");
    assert!(out.ends_with("\ntrue\n"), "{out}");
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn connect_tls_reads_and_writes() {
    let echo = tls_echo_server();
    let src = format!(
        r#"
import net
let c = net.connect_tls("localhost:{}")?
c.write_str("hello tls\n")?
print(c.read_line()?)
c.write(b"second\n")?
print(c.read_line()?)
print(c.peer_addr() == "{echo}")
c.close()
print(c.read().unwrap_err().kind)
"#,
        echo.port()
    );
    let r = run_with(&src, trusting);
    assert_eq!(r.ok_stdout(), "Some(\"HELLO TLS\")\nSome(\"SECOND\")\ntrue\nclosed\n");
}

#[test]
fn redirects_across_schemes() {
    let plain =
        http_server(|req| HttpResponse::new(200, format!("plain {} auth={}", req.path(), req.header("authorization").unwrap_or("-"))));
    let plain_addr = plain.addr;
    let secure = https_server(move |req| match req.path() {
        "/to-plain" => HttpResponse::new(302, "").header("location", &format!("http://{plain_addr}/landed")),
        _ => HttpResponse::new(200, format!("secure {} auth={}", req.path(), req.header("authorization").unwrap_or("-"))),
    });
    let secure_port = secure.addr.port();
    let up = http_server(move |_| HttpResponse::new(307, "").header("location", &format!("https://localhost:{secure_port}/upgraded")));
    let src = format!(
        r#"
import http
let a = http.get("http://{}/", headers: [("authorization", "secret")])?
print(a.text()?)
print(a.url)
let b = http.get("https://localhost:{secure_port}/to-plain", headers: [("authorization", "secret")])?
print(b.text()?)
"#,
        up.addr
    );
    let r = run_with(&src, trusting);
    // Credentials are dropped when the origin (including the scheme) changes.
    assert_eq!(r.ok_stdout(), format!("secure /upgraded auth=-\nhttps://localhost:{secure_port}/upgraded\nplain /landed auth=-\n"));
}

#[test]
fn https_with_private_network_policy() {
    let server = https_server(|_| HttpResponse::new(200, "ok"));
    let port = server.addr.port();
    let src = format!("import http\nprint(http.get(\"https://localhost:{port}/\")?.text()?)");
    let allowed = RulePolicy::new().allow_net(&format!("localhost:{port}")).deny_private_networks();
    let r = run_with(&src, |c| trusting(c).policy(Policy::rules(allowed)));
    assert_eq!(r.ok_stdout(), "ok\n");
    let wildcard = RulePolicy::new().allow_net("*").deny_private_networks();
    let r = run_with(&src, |c| trusting(c).policy(Policy::rules(wildcard)));
    assert!(r.stderr.contains("permission denied: connect to localhost"), "{}", r.stderr);
}

#[test]
fn invalid_extra_root_fails_cleanly() {
    let server = https_server(|_| HttpResponse::new(200, "ok"));
    let src = format!(
        "import http\nlet e = http.get(\"https://localhost:{}/\").unwrap_err()\nprint(e.kind)\nprint(e.message)",
        server.addr.port()
    );
    let r = run_with(&src, |c| c.policy(Policy::allow_all()).tls_root_certificates(vec![b"not a certificate".to_vec()]));
    let out = r.ok_stdout();
    assert!(out.starts_with("io\nTLS setup failed: invalid root certificate #1"), "{out}");
}

#[test]
fn closing_a_tls_connection_sends_close_notify() {
    // The server reports how the client's side of the session ended.
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (end_tx, end_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        common::rt().block_on(async move {
            use tokio::io::AsyncReadExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            let (sock, _) = listener.accept().await.unwrap();
            let mut stream = tls_acceptor().accept(sock).await.unwrap();
            let mut buf = Vec::new();
            end_tx.send(stream.read_to_end(&mut buf).await.map(|_| buf).map_err(|e| e.kind())).unwrap();
        });
    });
    let addr: std::net::SocketAddr = addr_rx.recv().unwrap();
    let src = format!("import net\nuse c = net.connect_tls(\"localhost:{}\")?\nc.write_str(\"bye\")?", addr.port());
    let r = run_with(&src, trusting);
    assert_eq!(r.ok_stdout(), "");
    let end = end_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(end, Ok(b"bye".to_vec()), "a clean TLS shutdown");
}
