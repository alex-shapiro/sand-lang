//! Resources: open files, sockets and HTTP bodies, closed by `use`, at
//! their end, or by the collector when a script drops them; the
//! open-resource limit; and the sandbox around handles.

mod common;

use common::{HttpResponse, TempDir, http_server, rt, run_with};
use sandlang::{Config, Engine, Limits, MemorySink, Policy, RulePolicy};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn with_resources(n: usize) -> impl FnOnce(Config) -> Config {
    let limits = Limits { max_open_resources: n, max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    move |c| c.policy(Policy::allow_all()).limits(limits)
}

// ---------------------------------------------------------------------------
// The open-resource limit and the collector
// ---------------------------------------------------------------------------

#[test]
fn dropped_files_are_closed_before_the_limit_is_reported() {
    // Each file is dropped without being closed: the collector closes them
    // when they are unreachable (at the latest when the limit is reached).
    let src = r#"
import fs
fs.write_text("f.txt", "x")?
for i in 0..500 {
    let f = fs.open("f.txt")?
    assert(f.read_text()? == "x")
}
print("done")
"#;
    let r = run_with(src, with_resources(10));
    assert_eq!(r.ok_stdout(), "done\n");
}

#[test]
fn dropped_connections_are_closed_before_the_limit_is_reported() {
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let addr = listener.addr()
spawn {
    loop {
        let Ok(_c) = listener.accept() else { break }
    }
}
for i in 0..300 {
    let c = net.connect(addr)?
    c.write_str("x")?
}
print("done")
"#;
    let r = run_with(src, with_resources(20));
    assert_eq!(r.ok_stdout(), "done\n");
}

#[test]
fn live_resources_count_against_the_limit() {
    let src = r#"
import fs
fs.write_text("f.txt", "x")?
var open: [fs.File] = []
loop {
    match fs.open("f.txt") {
        Ok(f) => open.push(f)
        Err(e) => {
            print("\(open.len()) open: \(e.kind): \(e.message)")
            break
        }
    }
}
// Closing one makes room for another.
open[0].close()
print(fs.open("f.txt").is_ok())
"#;
    let r = run_with(src, with_resources(8));
    assert_eq!(r.ok_stdout(), "8 open: limit: too many open resources (limit 8)\ntrue\n");
}

#[test]
fn use_closes_files_at_once() {
    // With `use`, a loop never holds more than one file, whatever the limit.
    let src = r#"
import fs
fs.write_text("f.txt", "x")?
var held: [fs.File] = []
for i in 0..100 {
    use f = fs.open("f.txt")?
    held.push(f)
}
print(held.all(f => f.read().is_err()))
"#;
    let r = run_with(src, with_resources(1));
    assert_eq!(r.ok_stdout(), "true\n");
}

// ---------------------------------------------------------------------------
// Files and the sandbox
// ---------------------------------------------------------------------------

#[test]
fn opening_files_is_checked_for_each_mode() {
    let dir = TempDir::new("open-modes");
    dir.write("ro/a.txt", "read only");
    dir.write("rw/b.txt", "writable");
    let policy = RulePolicy::new().allow_read(dir.path().join("ro")).allow_write(dir.path().join("rw"));
    let src = r#"
import fs
let modes = [fs.Mode.Read, fs.Mode.Write, fs.Mode.Append, fs.Mode.ReadWrite]
for path in ["ro/a.txt", "rw/b.txt", "elsewhere.txt"] {
    print([match fs.open(path, m) { Ok(_) => "ok", Err(e) => e.kind } for m in modes])
}
print(fs.open("ro/a.txt", fs.Mode.Write).unwrap_err().message)
print(fs.read_text("ro/a.txt")?)
"#;
    let cwd = dir.path().to_path_buf();
    let r = run_with(src, move |c| c.policy(Policy::rules(policy)).cwd(cwd));
    assert_eq!(
        r.ok_stdout(),
        "[\"ok\", \"permission_denied\", \"permission_denied\", \"permission_denied\"]\n\
         [\"ok\", \"ok\", \"ok\", \"ok\"]\n\
         [\"permission_denied\", \"permission_denied\", \"permission_denied\", \"permission_denied\"]\n\
         permission denied: write ro/a.txt\n\
         read only\n"
    );
}

#[cfg(unix)]
#[test]
fn open_files_stay_the_files_the_policy_checked() {
    // A file is opened through the directories the policy checked: a
    // symlink swapped in afterwards changes nothing for the open file, and
    // one in place when it is opened is resolved and checked first.
    let dir = TempDir::new("open-swap");
    dir.write("allowed/data.txt", "allowed data\n");
    dir.write("secret/data.txt", "secret data\n");
    dir.write("sync/.keep", "");
    let policy = RulePolicy::new()
        .allow_write(dir.path().join("allowed"))
        .allow_write(dir.path().join("sync"))
        .allow_read(dir.path().join("swapped"));
    let root = dir.path().to_path_buf();
    let src = r#"
import fs
use f = fs.open("allowed/data.txt", fs.Mode.ReadWrite)?
print(f.read_line()?)
// (The host swaps a symlink to the secret directory in here.)
fs.write_text("sync/ready", "")?
while fs.exists("sync/ready")? {
    task.sleep(10ms)
}
f.seek(0)?
print(f.read_text()?)
f.write_str("still the allowed file\n")?
print(fs.open("swapped/data.txt").unwrap_err().kind)
"#;
    let out = MemorySink::new();
    let config = Config::new().policy(Policy::rules(policy)).cwd(&root).stdout(out.clone());
    let main = dir.write("main.sand", src);
    let mut engine = Engine::new(config);
    let handle = std::thread::spawn(move || rt().block_on(engine.run_file(&main)));
    let start = Instant::now();
    while !root.join("sync/ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::rename(root.join("allowed"), root.join("moved")).unwrap();
    std::os::unix::fs::symlink(root.join("secret"), root.join("allowed")).unwrap();
    std::os::unix::fs::symlink(root.join("secret"), root.join("swapped")).unwrap();
    std::fs::remove_file(root.join("sync/ready")).unwrap();
    handle.join().unwrap().unwrap();
    assert_eq!(out.contents(), "Some(\"allowed data\")\nallowed data\n\npermission_denied\n");
    assert_eq!(std::fs::read_to_string(root.join("secret/data.txt")).unwrap(), "secret data\n");
    assert_eq!(std::fs::read_to_string(root.join("moved/data.txt")).unwrap(), "allowed data\nstill the allowed file\n");
}

#[test]
fn reading_files_is_bounded_by_the_memory_limit() {
    let dir = TempDir::new("open-memory");
    dir.write("big.txt", &"z".repeat(10 << 20));
    dir.write("long-line.txt", &"y".repeat(10 << 20));
    let src = r#"
import fs
use f = fs.open("big.txt")?
print(f.read_all().unwrap_err().kind)
// What was not read stays readable a piece at a time.
var total = 0
loop {
    let chunk = f.read(1048576)?
    if chunk.is_empty() { break }
    total += chunk.len()
}
print(total > 0)
use g = fs.open("long-line.txt")?
print(g.read_line().unwrap_err().kind)
"#;
    let cwd = dir.path().to_path_buf();
    let limits = Limits { max_memory: Some(8 << 20), ..Limits::default() };
    let r = run_with(src, move |c| c.policy(Policy::allow_all()).limits(limits).cwd(cwd));
    assert_eq!(r.ok_stdout(), "limit\ntrue\nlimit\n");
}

// ---------------------------------------------------------------------------
// HTTP bodies
// ---------------------------------------------------------------------------

#[test]
fn http_bodies_are_read_as_they_arrive() {
    let server = http_server(|req| match req.path() {
        "/lines" => HttpResponse::new(200, "one\ntwo\nthree\n"),
        "/chunked" => {
            let mut r = HttpResponse::new(200, "x".repeat(100_000));
            r.chunked = true;
            r
        }
        _ => HttpResponse::new(200, "y".repeat(3 << 20)),
    });
    let base = format!("http://{}", server.addr);
    let dir = TempDir::new("http-stream");
    let src = format!(
        r#"
import fs
import http
import io
{{
    use s = http.open("GET", "{base}/lines")?
    print(s.status)
    while let Some(line) = s.body.read_line()? {{
        print(line)
    }}
}}
{{
    use s = http.open("GET", "{base}/chunked")?
    print(s.body.read_all()?.len())
}}
// A body larger than the memory limit, copied to a file a chunk at a time.
{{
    use s = http.open("GET", "{base}/big")?
    use out = fs.open("big.bin", fs.Mode.Write)?
    print(io.copy(s.body, out)?)
}}
print(fs.metadata("big.bin")?.size)
print(http.get("{base}/big").unwrap_err().kind)
"#
    );
    let cwd = dir.path().to_path_buf();
    let limits = Limits { max_memory: Some(2 << 20), ..Limits::default() };
    let r = run_with(&src, move |c| c.policy(Policy::allow_all()).limits(limits).cwd(cwd));
    assert_eq!(r.ok_stdout(), format!("200\none\ntwo\nthree\n100000\n{0}\n{0}\nlimit\n", 3 << 20));
}

#[test]
fn http_bodies_release_their_connection_at_their_end() {
    let server = http_server(|_| HttpResponse::new(200, "body"));
    let base = format!("http://{}", server.addr);
    // The streams are kept: only reading them to their end releases their
    // connections (the collector cannot).
    let src = format!(
        r#"
import http
var kept: [http.Stream] = []
for i in 0..20 {{
    let s = http.open("GET", "{base}/")?
    assert(s.body.read_text()? == "body")
    kept.push(s)
}}
print(kept.len())
// A stream that is not read holds its connection until closed.
let a = http.open("GET", "{base}/")?
let b = http.open("GET", "{base}/")?
print(http.open("GET", "{base}/").unwrap_err().kind)
a.close()
print(http.get("{base}/")?.text()?)
print(a.body.read().unwrap_err().kind)
"#
    );
    let r = run_with(&src, with_resources(2));
    assert_eq!(r.ok_stdout(), "20\nlimit\nbody\nclosed\n");
}

#[test]
fn http_server_bodies_are_read_by_the_handler() {
    let out = MemorySink::new();
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
for _ in 0..2 {
    use req = server.accept()?
    let first = req.body.read(5)?
    let rest = req.text()?
    req.respond(200, "\(first.len()) + \(rest.len()) bytes".bytes(), headers: [("set-cookie", "a=1"), ("set-cookie", "b=2")])?
}
"#;
    let limits = Limits { max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let mut engine = Engine::new(Config::new().policy(Policy::allow_all()).limits(limits).stdout(out.clone()));
    let handle = std::thread::spawn(move || rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let addr = loop {
        if let Some(l) = out.contents().lines().next() {
            break l.trim_start_matches("ready ").to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    // A chunked body.
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n6\r\nefghij\r\n0\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.ends_with("4 + 6 bytes"), "{resp}");
    assert!(resp.contains("set-cookie: a=1\r\nset-cookie: b=2\r\n"), "{resp}");
    // `100 Continue` is sent when the handler starts reading the body.
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "PUT / HTTP/1.1\r\nContent-Length: 8\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = [0u8; 25];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"HTTP/1.1 100 Continue\r\n\r\n");
    s.write_all(b"12345678").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200") && resp.ends_with("5 + 3 bytes"), "{resp}");
    handle.join().unwrap().unwrap();
}

#[test]
fn idle_connections_do_not_hold_up_new_requests() {
    // The server reads at most half of `max_open_resources` connections at
    // once; beyond, the oldest is answered with 408 and closed.
    let out = MemorySink::new();
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
use req = server.accept()?
req.respond(200, b"served")?
"#;
    let limits = Limits { max_open_resources: 20, max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let mut engine = Engine::new(Config::new().policy(Policy::allow_all()).limits(limits).stdout(out.clone()));
    let handle = std::thread::spawn(move || rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let addr = loop {
        if let Some(l) = out.contents().lines().next() {
            break l.trim_start_matches("ready ").to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut idle: Vec<_> = (0..12).map(|_| std::net::TcpStream::connect(&addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.ends_with("served"), "{resp}");
    assert!(start.elapsed() < Duration::from_secs(5), "served after {:?}", start.elapsed());
    // The oldest idle connections were answered with 408.
    let first = &mut idle[0];
    first.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut resp = String::new();
    let _ = first.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 408 Request Timeout"), "{resp}");
    handle.join().unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Removing trees
// ---------------------------------------------------------------------------

#[test]
fn removing_a_tree_checks_each_entry() {
    // The policy is asked about every entry before it is removed; a denial
    // stops the removal there, naming the entry as the script would.
    let dir = TempDir::new("remove-tree");
    for f in ["tree/a.txt", "tree/sub/b.txt", "tree/sub/deep/keep.txt", "tree/z.txt"] {
        dir.write(f, "x");
    }
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = asked.clone();
    let base = root.clone();
    let policy = Policy::custom(move |cap| match cap {
        sandlang::Capability::FsWrite(p) | sandlang::Capability::FsRead(p) => {
            log.lock().unwrap().push(p.strip_prefix(&base).unwrap_or(p).display().to_string());
            if p.ends_with("keep.txt") { sandlang::Decision::Deny } else { sandlang::Decision::Allow }
        }
        _ => sandlang::Decision::Allow,
    });
    let src = r#"
import fs
let e = fs.remove_dir("tree", recursive: true).unwrap_err()
print(e.kind)
print(e.message)
"#;
    let cwd = root.clone();
    let r = run_with(src, move |c| c.policy(policy).cwd(cwd));
    assert_eq!(r.ok_stdout(), "permission_denied\npermission denied: write tree/sub/deep/keep.txt\n");
    assert!(root.join("tree/sub/deep/keep.txt").exists());
    let asked = asked.lock().unwrap().clone();
    assert!(asked.contains(&"tree".to_string()), "{asked:?}");
    assert!(asked.contains(&"tree/sub/deep/keep.txt".to_string()), "{asked:?}");
    // Entries are checked before they are removed.
    for p in &asked {
        if p.starts_with("tree/") && !root.join(p).exists() {
            assert!(!p.ends_with("keep.txt"));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn removing_a_tree_removes_names_that_are_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let dir = TempDir::new("remove-tree-bytes");
    dir.write("tree/ok.txt", "x");
    let odd = dir.path().join("tree").join(std::ffi::OsStr::from_bytes(b"\xff\xfe.txt"));
    if std::fs::write(&odd, "y").is_err() {
        return;
    }
    let cwd = dir.path().to_path_buf();
    let r = run_with("import fs\nfs.remove_dir(\"tree\", recursive: true)?\nprint(fs.exists(\"tree\")?)", move |c| {
        c.policy(Policy::allow_all()).cwd(cwd)
    });
    assert_eq!(r.ok_stdout(), "false\n");
}

#[test]
fn removing_a_deep_tree_holds_few_directories_open() {
    // Deeper than the directories a removal keeps open: the levels it
    // closed are opened again on the way back up.
    let src = r#"
import fs
var p = "deep"
for i in 0..300 {
    p = p + "/d\(i)"
    if i % 40 == 0 {
        fs.create_dir(p, recursive: true)?
        fs.write_text(p + "/f.txt", "x")?
    }
}
fs.create_dir(p, recursive: true)?
fs.write_text(p + "/leaf.txt", "x")?
fs.remove_dir("deep", recursive: true)?
print(fs.exists("deep")?)
"#;
    let r = run_with(src, with_resources(64));
    assert_eq!(r.ok_stdout(), "false\n");
}

#[test]
fn large_reads_of_small_files_fit_small_memory_limits() {
    let src = r#"
import fs
fs.write_text("small.txt", "abc")?
use f = fs.open("small.txt")?
print(f.read(1048576)?)
print(f.read(1048576)?)
"#;
    let limits = Limits { max_memory: Some(1 << 20), ..Limits::default() };
    let r = run_with(src, move |c| c.policy(Policy::allow_all()).limits(limits));
    assert_eq!(r.ok_stdout(), "b\"abc\"\nb\"\"\n");
}

/// Starts `src` (which prints `ready ADDR` first) as a server script.
fn server_script(
    src: &'static str,
    limits: Limits,
) -> (String, std::thread::JoinHandle<Result<sandlang::Outcome, sandlang::Error>>, MemorySink) {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().policy(Policy::allow_all()).limits(limits).stdout(out.clone()));
    let handle = std::thread::spawn(move || rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let addr = loop {
        if let Some(l) = out.contents().lines().next() {
            break l.trim_start_matches("ready ").to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    (addr, handle, out)
}

#[test]
fn closing_a_request_closes_its_socket_even_if_its_body_is_kept() {
    // A request's body does not keep its connection open: the connections
    // are closed (and stop counting) when the script closes them.
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
var bodies: [http.Body] = []
for _ in 0..20 {
    let req = server.accept()?
    // lint: allow(resource-escapes)
    bodies.push(req.body)
    req.conn.close()
}
print(bodies.len())
"#;
    let limits = Limits { max_open_resources: 6, max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let (addr, handle, out) = server_script(src, limits);
    for _ in 0..20 {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        write!(s, "POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // The server's side is closed: end of stream, at once.
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
    handle.join().unwrap().unwrap();
    assert!(out.contents().ends_with("20\n"), "{}", out.contents());
}

#[test]
fn a_request_body_leaves_its_connection_usable_between_reads() {
    let src = r#"
import http
let server = http.serve("127.0.0.1:0")?
print("ready \(server.addr())")
use req = server.accept()?
print(req.body.read(2)?)
// The connection itself reads past the body.
print(req.conn.read(100)?)
"#;
    let limits = Limits { max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let (addr, handle, out) = server_script(src, limits);
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "POST / HTTP/1.1\r\nContent-Length: 4\r\n\r\nabcdEXTRA").unwrap();
    handle.join().unwrap().unwrap();
    let out = out.contents();
    assert!(out.ends_with("b\"ab\"\nb\"cdEXTRA\"\n"), "{out}");
}
