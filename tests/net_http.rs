//! Acceptance tests for the `http` and `net` modules against real sockets.

mod common;

use common::{HttpResponse, http_server, run_with};
use sandlang::{Config, Engine, Error, Limits, MemorySink, Policy, RulePolicy};
use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

fn run_net(src: &str) -> String {
    let r = run_with(src, |c| c.policy(Policy::allow_all()));
    r.ok_stdout().to_string()
}

#[test]
fn http_get_basics() {
    let server = http_server(|req| match req.path() {
        "/hello" => HttpResponse::new(200, "hello world")
            .header("Content-Type", "text/plain")
            .header("X-Custom", "yes")
            .header("Set-Cookie", "a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT")
            .header("Set-Cookie", "b=2"),
        "/chunked" => {
            let mut r = HttpResponse::new(200, "this body arrives in several chunks");
            r.chunked = true;
            r
        }
        "/json" => HttpResponse::new(200, r#"{"a": [1, 2]}"#),
        "/echo-headers" => {
            HttpResponse::new(200, format!("{}|{}", req.header("x-token").unwrap_or("-"), req.header("host").unwrap_or("-")))
        }
        _ => HttpResponse::new(404, "nope"),
    });
    let base = format!("http://{}", server.addr);
    let src = format!(
        r#"
import http
let r = http.get("{base}/hello")?
print(r.status)
print(r.ok())
print(r.text()?)
print(r.header("content-type") ?? "")
print(r.header("X-CUSTOM"))
// Repeated headers stay separate (a cookie's date contains a comma).
print(r.header_values("set-cookie"))
print(r.headers.filter(h => h.0 == "set-cookie").len())
print(r.url)
let c = http.get("{base}/chunked")?
print(c.text()?)
let j = http.get("{base}/json")?.json()?
print(j.path("a.1"))
let h = http.get("{base}/echo-headers", headers: [("X-Token", "abc")])?
print(h.text()?)
let missing = http.get("{base}/missing")?
print(missing.status)
print(missing.ok())
"#
    );
    assert_eq!(
        run_net(&src),
        format!(
            "200\ntrue\nhello world\ntext/plain\nSome(\"yes\")\n[\"a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT\", \"b=2\"]\n2\n{base}/hello\nthis body arrives in several chunks\nSome(Num(2.0))\nabc|{}\n404\nfalse\n",
            server.addr
        )
    );
}

#[test]
fn http_methods_and_bodies() {
    let server = http_server(|req| {
        HttpResponse::new(200, format!("{} {} {} [{}]", req.method, req.target, req.body.len(), String::from_utf8_lossy(&req.body)))
    });
    let base = format!("http://{}", server.addr);
    let src = format!(
        r#"
import http
print(http.post("{base}/p?x=1", b"payload")?.text()?)
print(http.request("PUT", "{base}/put", body: "abc".bytes())?.text()?)
print(http.request("DELETE", "{base}/d")?.text()?)
print(http.request("HEAD", "{base}/h")?.body.len())
"#
    );
    assert_eq!(run_net(&src), "POST /p?x=1 7 [payload]\nPUT /put 3 [abc]\nDELETE /d 0 []\n0\n");
    let reqs = server.requests.lock().unwrap();
    assert_eq!(reqs[0].header("content-length"), Some("7"));
}

#[test]
fn http_redirects() {
    let server = http_server(|req| match req.path() {
        "/a" => HttpResponse::new(302, "").header("Location", "/b"),
        "/b" => HttpResponse::new(307, "").header("Location", "/c"),
        "/c" => HttpResponse::new(200, "final"),
        "/loop" => HttpResponse::new(302, "").header("Location", "/loop"),
        _ => HttpResponse::new(404, ""),
    });
    let base = format!("http://{}", server.addr);
    let src = format!(
        r#"
import http
let r = http.get("{base}/a")?
print(r.status)
print(r.url)
print(r.text()?)
let nf = http.request("GET", "{base}/a", follow_redirects: false)?
print(nf.status)
print(nf.header("location"))
print(http.get("{base}/loop").unwrap_err().message)
"#
    );
    assert_eq!(run_net(&src), format!("200\n{base}/c\nfinal\n302\nSome(\"/b\")\ntoo many redirects\n"));
}

#[test]
fn http_redirects_are_checked_against_policy() {
    let target = http_server(|_| HttpResponse::new(200, "secret"));
    let target_addr = target.addr;
    let origin = http_server(move |_| HttpResponse::new(302, "").header("location", &format!("http://{target_addr}/x")));
    let policy = RulePolicy::new().allow_net(&origin.addr.to_string());
    let src = format!("import http\nprint(http.get(\"http://{}/\").unwrap_err().kind)", origin.addr);
    let r = run_with(&src, |c| c.policy(Policy::rules(policy)));
    assert_eq!(r.ok_stdout(), "permission_denied\n");
    assert!(target.requests.lock().unwrap().is_empty());
}

#[test]
fn http_rules_decide_methods_and_routes() {
    let server = http_server(|req| match req.path() {
        "/old" => HttpResponse::new(302, "").header("location", "/private/x"),
        path => HttpResponse::new(200, format!("{} {path}", req.method)),
    });
    let addr = server.addr;
    let policy = RulePolicy::new()
        .allow_http(&format!("GET {addr}/api/** {addr}/old"))
        .allow_http(&format!("POST {addr}/api/items"))
        .deny_http(&format!("{addr}/api/admin"));
    let src = format!(
        r#"
import http
fn show(r: Result<http.Response>) {{
    match r {{
        Ok(r) => print(r.text().unwrap())
        Err(e) => print(e.message)
    }}
}}
show(http.get("http://{addr}/api/v1"))
show(http.post("http://{addr}/api/items", b""))
show(http.post("http://{addr}/api/other", b""))
show(http.get("http://{addr}/api/admin/users"))
show(http.get("http://{addr}/api/../api/./v1/../v2"))
show(http.get("http://{addr}/api/v1/%2e%2e/%2E%2E/private"))
show(http.get("http://{addr}/old"))
"#
    );
    let r = run_with(&src, |c| c.policy(Policy::rules(policy)));
    assert_eq!(
        r.ok_stdout(),
        format!(
            "GET /api/v1\nPOST /api/items\npermission denied: POST http://{addr}/api/other\n\
             permission denied: GET http://{addr}/api/admin/users\nGET /api/v2\n\
             permission denied: GET http://{addr}/private\npermission denied: GET http://{addr}/private/x\n"
        )
    );
    // Denied requests reach no server, and the server saw the paths as checked.
    let seen: Vec<String> = server.requests.lock().unwrap().iter().map(|r| r.target.clone()).collect();
    assert_eq!(seen, ["/api/v1", "/api/items", "/api/v2", "/old"]);
}

#[test]
fn http_body_limit() {
    let server = http_server(|_| HttpResponse::new(200, vec![b'x'; 100_000]));
    let src = format!("import http\nprint(http.get(\"http://{}/\").unwrap_err().kind)", server.addr);
    let r = run_with(&src, |c| c.policy(Policy::allow_all()).limits(Limits { max_http_body: 1000, ..Limits::default() }));
    assert_eq!(r.ok_stdout(), "limit\n");
}

#[test]
fn http_timeout() {
    // A server that accepts but never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    let src = format!("import http\nlet r = http.get(\"http://{addr}/\", timeout: 100ms)\nprint(r.unwrap_err().kind)");
    let start = Instant::now();
    assert_eq!(run_net(&src), "timeout\n");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn http_connection_denied_and_bad_urls() {
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let src = format!(
        "import http\nprint(http.get(\"http://{unused}/\").unwrap_err().kind)\nprint(http.get(\"not a url\").unwrap_err().kind)\nprint(http.get(\"ftp://x/\").unwrap_err().kind)"
    );
    assert_eq!(run_net(&src), "io\nparse\ninvalid\n");
}

#[test]
fn concurrent_http_requests() {
    let server = http_server(|req| {
        std::thread::sleep(Duration::from_millis(50));
        HttpResponse::new(200, req.path().to_string())
    });
    let base = format!("http://{}", server.addr);
    let src = format!(
        r#"
import http
import time
let start = time.monotonic()
let ts = (0..8).map(i => spawn http.get("{base}/" + i.to_str()))
print(task.all(ts).map(r => r.unwrap().text().unwrap()))
print(start.elapsed() < 350ms)
"#
    );
    assert_eq!(run_net(&src), "[\"/0\", \"/1\", \"/2\", \"/3\", \"/4\", \"/5\", \"/6\", \"/7\"]\ntrue\n");
}

#[test]
fn http_server_requests_expose_their_connection() {
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
let addr = server.addr()
let client = spawn http.get("http://\(addr)/stream?x=1")
let req = server.accept()?
print("\(req.method) \(req.path) \(req.query) \(req.header("Host").is_some())")
// Stream a chunked response by hand instead of using `respond`.
req.conn.write_str("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")?
for part in ["streamed ", "by ", "hand"] {
    req.conn.write_str("\(part.len():x)\r\n\(part)\r\n")?
}
req.conn.write_str("0\r\n\r\n")?
req.conn.close()
print(client.wait()?.text()?)
print(req.respond(200, b"again").unwrap_err().kind)
server.close()
print(server.accept().unwrap_err().kind)
"#;
    assert_eq!(run_net(src), "GET /stream x=1 true\nstreamed by hand\nclosed\nclosed\n");
}

#[test]
fn http_server_api() {
    let out = MemorySink::new();
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("addr \(server.addr())")
for _ in 0..2 {
    let req = server.accept()?
    let body = "you sent \(req.method) \(req.path) q=\(req.query) body=\(req.text()?) ua=\(req.header("user-agent") ?? "-")"
    req.respond(201, body.bytes(), headers: [("x-reply", "1")])?
}
"#;
    let mut engine = Engine::new(Config::new().policy(Policy::allow_all()).stdout(out.clone()));
    let handle = std::thread::spawn(move || common::rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let addr = loop {
        if let Some(l) = out.contents().lines().next() {
            break l.trim_start_matches("addr ").to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    for (method, path, body) in [("GET", "/a?x=1", ""), ("POST", "/b", "data")] {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: x\r\nUser-Agent: test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 201"), "{resp}");
        assert!(resp.to_lowercase().contains("x-reply: 1"), "{resp}");
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        assert!(resp.ends_with(&format!("you sent {method} {p} q={q} body={body} ua=test")), "{resp}");
    }
    handle.join().unwrap().unwrap();
}

#[test]
fn tcp_client_and_server_in_one_script() {
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let addr = listener.addr()
let server = spawn {
    let conn = listener.accept().unwrap()
    var lines: [str] = []
    var echo = true
    while let Some(line) = conn.read_line().unwrap() {
        // Stop answering at "bye": the client closes after sending it, and
        // writing to a closed peer makes it answer with a reset.
        echo = echo and line != "bye"
        if echo {
            conn.write_str(line.to_upper() + "\n").unwrap()
        }
        lines.push(line)
    }
    conn.close()
    lines
}
let client = net.connect(addr)?
client.write_str("hello\nworld\n")?
print(client.read_line()?)
print(client.read_line()?)
client.write(b"bye\npartial")?
client.close()
print(server.wait())
"#;
    assert_eq!(run_net(src), "Some(\"HELLO\")\nSome(\"WORLD\")\n[\"hello\", \"world\", \"bye\", \"partial\"]\n");
}

#[test]
fn reads_after_end_of_stream_keep_returning_eof() {
    // Writing to a peer that has closed makes it answer with a reset; on some
    // platforms the next read would then fail even though EOF was already seen.
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let client = net.connect(listener.addr())?
let conn = listener.accept()?
client.write(b"last")?
client.close()
print(conn.read_line()?)
print(conn.write_str("too late\n").is_ok())
task.sleep(50ms)
print(conn.read_line()?)
print(conn.read()?)
"#;
    assert_eq!(run_net(src), "Some(\"last\")\ntrue\nNone\nb\"\"\n");
}

#[test]
fn tcp_reads_raw_bytes_and_eof() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"\x00\x01\x02rest").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        s.write_all(line.to_uppercase().as_bytes()).unwrap();
    });
    let src = format!(
        r#"
import net
let c = net.connect("{addr}")?
var got = b""
while got.len() < 7 {{
    got = got + c.read(max: 3)?
}}
print(got)
c.write_str("ping\n")?
print(c.read_line()?)
print(c.read()?)
print(c.peer_addr() == "{addr}")
"#
    );
    assert_eq!(run_net(&src), "b\"\\x00\\x01\\x02rest\"\nSome(\"PING\")\nb\"\"\ntrue\n");
}

#[test]
fn net_permissions() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let policy = RulePolicy::new().allow_net("localhost");
    let src = format!("import net\nprint(net.connect(\"{addr}\").unwrap_err().kind)\nprint(net.listen(\"127.0.0.1:0\").unwrap_err().kind)");
    let r = run_with(&src, |c| c.policy(Policy::rules(policy)));
    assert_eq!(r.ok_stdout(), "permission_denied\npermission_denied\n");
}

#[test]
fn closing_engine_closes_sockets() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let src = format!("import net\nlet c = net.connect(\"{addr}\")?\nc.write_str(\"hi\")?");
    let r = run_with(&src, |c| c.policy(Policy::allow_all()));
    r.ok_stdout();
    let (mut s, _) = listener.accept().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    // The engine has finished, so the connection must have been closed (EOF).
    s.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"hi");
}

#[test]
fn errors_are_distinguishable() {
    let r = run_with("import net\nlet c = net.connect(\"nonsense\")?", |c| c.policy(Policy::allow_all()));
    match r.result {
        Ok(o) => assert_eq!(o.exit_code, 1),
        Err(e) => panic!("{e:?}"),
    }
    assert!(r.stderr.contains("error:"));
    let _ = Error::Interrupted;
}

/// A TCP server that accepts connections and never reads or answers them.
#[cfg(feature = "tls")]
fn silent_server() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    addr
}

// Uses TLS handshakes that never complete.
#[cfg(feature = "tls")]
#[test]
fn sockets_of_pending_operations_count_as_open_resources() {
    // Connections being established (a TLS handshake that never completes)
    // and HTTP requests in flight hold sockets: they count against
    // `max_open_resources` until they end, fail or are cancelled.
    let addr = silent_server();
    let port = addr.port();
    let src = format!(
        r#"
import http
import net
let handshakes = (0..2).map(_ => spawn net.connect_tls("127.0.0.1:{port}", timeout: 30s))
let requests = (0..2).map(_ => spawn http.get("http://{addr}/", timeout: 30s))
task.sleep(200ms)
print(net.listen("127.0.0.1:0").unwrap_err().kind)
print(net.connect("{addr}").unwrap_err().kind)
print(http.get("http://{addr}/").unwrap_err().kind)
for t in handshakes {{ t.cancel() }}
for t in requests {{ t.cancel() }}
let l = net.listen("127.0.0.1:0")?
print("listening on a released slot")
let pending = spawn l.accept()
task.sleep(50ms)
let others = (0..3).map(_ => net.listen("127.0.0.1:0"))
print(others.filter(r => r.is_ok()).len())
pending.cancel()
"#
    );
    let limits = Limits { max_open_resources: 4, max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let r = run_with(&src, |c| c.policy(Policy::allow_all()).limits(limits));
    // The listener and the pending accept leave two of the four slots.
    assert_eq!(r.ok_stdout(), "limit\nlimit\nlimit\nlistening on a released slot\n2\n");
}

#[cfg(feature = "tls")]
#[test]
fn tls_handshakes_time_out() {
    let addr = silent_server();
    let src = format!("import net\nlet e = net.connect_tls(\"127.0.0.1:{}\", timeout: 200ms).unwrap_err()\nprint(e.kind)", addr.port());
    let start = Instant::now();
    let r = run_with(&src, |c| c.policy(Policy::allow_all()));
    assert_eq!(r.ok_stdout(), "timeout\n");
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// Runs a server script that prints `ready ADDR` first; returns the address
/// and the run's thread.
fn start_server_script(src: &'static str, limits: Limits) -> (String, std::thread::JoinHandle<Result<sandlang::Outcome, Error>>) {
    let out = MemorySink::new();
    let config = Config::new().policy(Policy::allow_all()).limits(limits).stdout(out.clone());
    let mut engine = Engine::new(config);
    let handle = std::thread::spawn(move || common::rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let addr = loop {
        if let Some(l) = out.contents().lines().next() {
            break l.trim_start_matches("ready ").to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    (addr, handle)
}

#[test]
fn idle_connections_do_not_delay_http_requests() {
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
for i in 0..2 {
    let req = server.accept()?
    req.respond(200, "served \(req.path)".bytes())?
}
"#;
    let limits = Limits { max_wall_time: Some(Duration::from_secs(25)), ..Limits::default() };
    let (addr, handle) = start_server_script(src, limits);
    // Idle connections, and one that stops in the middle of its request.
    let idle: Vec<_> = (0..3).map(|_| std::net::TcpStream::connect(&addr).unwrap()).collect();
    let mut partial = std::net::TcpStream::connect(&addr).unwrap();
    write!(partial, "GET /partial HT").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    for path in ["/one", "/two"] {
        let start = Instant::now();
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.ends_with(&format!("served {path}")), "{resp}");
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }
    handle.join().unwrap().unwrap();
    drop((idle, partial));
}

#[test]
fn connection_buffers_count_against_the_memory_limit() {
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let addr = listener.addr()
let server = spawn {
    var accepted: [net.Conn] = []
    while let Ok(c) = listener.accept() { accepted.push(c) }
    accepted.len()
}
var conns: [net.Conn] = []
var err = ""
while conns.len() < 1000 {
    match net.connect(addr) {
        Ok(c) => conns.push(c)
        Err(e) => {
            err = "\(e.kind): \(e.message)"
            break
        }
    }
}
print(err)
print(conns.len())
for c in conns[0..10] { c.close() }
print(net.connect(addr).is_ok())
listener.close()
print(server.wait() > 0)
"#;
    let limits = Limits { max_memory: Some(1 << 20), ..Limits::default() };
    let r = run_with(src, |c| c.policy(Policy::allow_all()).limits(limits));
    let out = r.ok_stdout();
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("limit: cannot open a connection"), "{out}");
    let n: usize = lines[1].parse().unwrap();
    assert!((20..=128).contains(&n), "{n} connections");
    assert_eq!(&lines[2..], ["true", "true"]);

    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
let req = server.accept()?
req.respond(200, "served".bytes())?
"#;
    let limits = Limits { max_memory: Some(768 << 10), max_wall_time: Some(Duration::from_secs(25)), ..Limits::default() };
    let (addr, handle) = start_server_script(src, limits);
    // Each connection being read holds an 8 KiB buffer: not all of them fit.
    let mut idle: Vec<_> = (0..120).map(|_| std::net::TcpStream::connect(&addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(500));
    let mut denied = 0;
    for s in &mut idle {
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let mut resp = String::new();
        if s.read_to_string(&mut resp).is_ok() {
            assert!(resp.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{resp}");
            denied += 1;
        }
    }
    assert!((1..120).contains(&denied), "{denied} denied");
    drop(idle);
    std::thread::sleep(Duration::from_millis(200));
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200") && resp.ends_with("served"), "{resp}");
    handle.join().unwrap().unwrap();
}

#[test]
fn unlimited_http_bodies() {
    // `max_http_body = usize::MAX` must not overflow when reading bodies.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            // A body delimited by the end of the connection.
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\nto the end");
        }
    });
    let src = format!("import http\nprint(http.get(\"http://{addr}/\")?.text()?)");
    let limits = Limits { max_http_body: usize::MAX, ..Limits::default() };
    let r = run_with(&src, |c| c.policy(Policy::allow_all()).limits(limits));
    assert_eq!(r.ok_stdout(), "to the end\n");
}

#[test]
fn http_uploads_stream_their_bodies() {
    let server = http_server(|req| match req.path() {
        "/moved" => HttpResponse::new(301, "").header("location", "/echo"),
        _ => HttpResponse::new(
            200,
            format!("{} {} {}", req.method, req.header("transfer-encoding").unwrap_or("-"), String::from_utf8_lossy(&req.body)),
        ),
    });
    let base = format!("http://{}", server.addr);
    let src = format!(
        r#"
import http
import io
fn send(method: str, url: str, parts: [str], headers: [(str, str)] = []) -> Result<str> {{
    use up = http.upload(method, url, headers: headers)?
    for p in parts {{
        up.write_str(p)?
    }}
    use resp = up.finish()?
    Ok("\(resp.status) \(resp.body.read_text()?)")
}}
print(send("POST", "{base}/echo", ["one ", "", "two"]))
print(send("PUT", "{base}/echo", ["abc"], headers: [("content-length", "3")]))
print(send("PUT", "{base}/echo", ["abcd"], headers: [("content-length", "3")]))
print(send("PUT", "{base}/echo", ["ab"], headers: [("content-length", "3")]))
print(send("POST", "{base}/moved", ["x"]))
use up = http.upload("POST", "{base}/echo")?
up.close()
print(up.write_str("late").unwrap_err().kind)
"#
    );
    assert_eq!(
        run_net(&src),
        "Ok(\"200 POST chunked one two\")\nOk(\"200 PUT - abc\")\n\
         Err(Error { kind: \"invalid\", message: \"the body is longer than its content-length\" })\n\
         Err(Error { kind: \"invalid\", message: \"the body is 1 bytes shorter than its content-length\" })\n\
         Ok(\"301 \")\nclosed\n"
    );
    let reqs = server.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3, "{reqs:?}");
    assert_eq!(reqs[1].header("content-length"), Some("3"));
}

#[test]
fn http_servers_stream_responses() {
    let (addr, handle) = start_server_script(
        r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
for _ in 0..4 {
    use req = server.accept()?
    let headers = if req.path == "/sized" { [("Content-Length", "5")] } else { [("x-a", "1")] }
    use out = req.respond_stream(200, headers)?
    out.write_str("he")?
    out.write(b"")?
    out.write_str("llo")?
    if req.path == "/finish" {
        out.finish()?
        print(out.write_str("late").unwrap_err().kind)
    }
}
"#,
        Limits::default(),
    );
    let exchange = |request: &str| {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    };
    let resp = exchange("GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.contains("x-a: 1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"), "{resp}");
    assert!(resp.ends_with("\r\n\r\n2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n"), "{resp}");
    let resp = exchange("GET /finish HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.ends_with("\r\n\r\n2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n"), "{resp}");
    let resp = exchange("GET /sized HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.contains("Content-Length: 5\r\nConnection: close\r\n\r\nhello"), "{resp}");
    assert!(!resp.contains("chunked"), "{resp}");
    let resp = exchange("HEAD / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.ends_with("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"), "{resp}");
    let r = handle.join().unwrap();
    assert!(r.is_ok(), "{r:?}");
}
