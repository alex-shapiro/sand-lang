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
        "/hello" => HttpResponse::new(200, "hello world").header("Content-Type", "text/plain").header("X-Custom", "yes"),
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
print(r.headers["content-type"])
print(r.header("X-CUSTOM"))
print(r.url)
let c = http.get("{base}/chunked")?
print(c.text()?)
let j = http.get("{base}/json")?.json()?
print(j.path("a.1"))
let h = http.get("{base}/echo-headers", headers: ["X-Token": "abc"])?
print(h.text()?)
let missing = http.get("{base}/missing")?
print(missing.status)
print(missing.ok())
"#
    );
    assert_eq!(
        run_net(&src),
        format!(
            "200\ntrue\nhello world\ntext/plain\nSome(\"yes\")\n{base}/hello\nthis body arrives in several chunks\nSome(Num(2.0))\nabc|{}\n404\nfalse\n",
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
fn http_connection_refused_and_bad_urls() {
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
fn http_server_api() {
    let out = MemorySink::new();
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("addr {server.addr()}")
for _ in 0..2 {
    let req = server.accept()?
    let body = "you sent {req.method} {req.path} q={req.query} body={req.text()?} ua={req.headers.get("user-agent") ?? "-"}"
    req.respond(201, body.bytes(), headers: ["x-reply": "1"])?
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
    while let Some(line) = conn.read_line().unwrap() {
        conn.write_str(line.to_upper() + "\n").unwrap()
    }
    conn.close()
    "server done"
}
let client = net.connect(addr)?
client.write_str("hello\nworld\n")?
print(client.read_line()?)
print(client.read_line()?)
client.write(b"partial")?
client.close()
print(server.wait())
"#;
    assert_eq!(run_net(src), "Some(\"HELLO\")\nSome(\"WORLD\")\nserver done\n");
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
