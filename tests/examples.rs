//! The required example programs, run end-to-end with sandboxed I/O.

mod common;

use common::{HttpResponse, TempDir, http_server, rt};
use sandlang::{Capability, Config, Decision, Engine, Error, MemorySink, Policy, RulePolicy};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn example(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(rel)
}

struct Output {
    result: Result<sandlang::Outcome, Error>,
    stdout: String,
    stderr: String,
}

fn run_example(rel: &str, args: &[&str], config: Config) -> Output {
    let out = MemorySink::new();
    let err = MemorySink::new();
    let config = config.stdout(out.clone()).stderr(err.clone()).args(args.iter().map(|s| s.to_string()).collect());
    let mut engine = Engine::new(config);
    let result = rt().block_on(engine.run_file(example(rel)));
    Output { result, stdout: out.contents(), stderr: err.contents() }
}

fn ok(o: &Output) -> &str {
    if let Err(e) = &o.result {
        panic!("example failed: {e}\nstdout:\n{}\nstderr:\n{}", o.stdout, o.stderr);
    }
    &o.stdout
}

const ALL_EXAMPLES: &[&str] = &[
    "life.sand",
    "calc.sand",
    "template.sand",
    "http_fetch.sand",
    "chat_server.sand",
    "adventure.sand",
    "search_index.sand",
    "s3/s3.sand",
    "s3/cli.sand",
];

#[test]
fn examples_type_check_lint_and_are_formatted() {
    for rel in ALL_EXAMPLES {
        let path = example(rel);
        let source = std::fs::read_to_string(&path).unwrap();
        let warnings = sandlang::lint::lint_file(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(warnings.is_empty(), "{rel} has lint warnings: {warnings:?}");
        let formatted = sandlang::fmt::format_source(&source).unwrap();
        assert!(formatted == source, "{rel} is not formatted:\n{}", first_diff(&source, &formatted));
    }
}

fn first_diff(a: &str, b: &str) -> String {
    for (i, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}:\n  source:    {x:?}\n  formatted: {y:?}", i + 1);
        }
    }
    format!("lengths differ: {} vs {} lines", a.lines().count(), b.lines().count())
}

#[test]
fn game_of_life() {
    let o = run_example("life.sand", &["4"], Config::new());
    assert_eq!(ok(&o), "generation 4, population 5\n........\n..#.....\n...#....\n.###....\n........\n........\n");
    // A glider returns to its shape every 4 generations; after 32 it has wrapped around fully.
    let o = run_example("life.sand", &["32"], Config::new());
    assert!(ok(&o).starts_with("generation 32, population 5\n"));
}

#[test]
fn calculator_interpreter() {
    let input = "1 + 2 * 3\nlet r = 2\npi * r ^ 2\n2 ^ 3 ^ 2\n-(4 - 6) * 2\nmax(1, sqrt(16), 3) + -3\n10 / 4\n10 / 0\nx + 1\n1 +\nsqrt(1, 2)\n7 % 4\nfoo(1)\n3 $ 4\n\n";
    let config = Config::new().policy(Policy::rules(RulePolicy::new().allow_stdin())).stdin_bytes(input.as_bytes().to_vec());
    let o = run_example("calc.sand", &[], config);
    assert_eq!(
        ok(&o),
        "7\nr = 2\n12.566370614359172\n512\n4\n1\n2.5\nerror: division by zero\nerror: unknown variable `x`\nerror: unexpected end of input\nerror: `sqrt` takes 1 argument\n3\nerror: unknown function `foo`\nerror: unexpected character `$` at 3\n"
    );
}

#[test]
fn template_engine() {
    let dir = TempDir::new("tmpl");
    let tmpl = dir.write(
        "page.tmpl",
        "<h1>{{ title | upper }}</h1>\n{# a comment #}\n<ul>\n{% for u in users %}  <li>{{ loop.index }}. {{ u.name | title }}{% if u.admin %} (admin){% endif %}{% if not loop.last %},{% endif %}</li>\n{% endfor %}</ul>\n{% if count > 1 %}many{% elif count == 1 %}one{% else %}none{% endif %}\n{{ missing | default(\"n/a\") }} {{ tags | join(\", \") }} {{ users | length }}\n{{ \"<b>\" | escape }}\n",
    );
    let data = dir.write(
        "data.json",
        r#"{"title": "Team", "users": [{"name": "ann lee", "admin": true}, {"name": "bob"}], "count": 2, "tags": ["x", "y"]}"#,
    );
    let policy = RulePolicy::new().allow_read(dir.path());
    let o = run_example("template.sand", &[tmpl.to_str().unwrap(), data.to_str().unwrap()], Config::new().policy(Policy::rules(policy)));
    assert_eq!(ok(&o), "<h1>TEAM</h1>\n\n<ul>\n  <li>1. Ann Lee (admin),</li>\n  <li>2. Bob</li>\n</ul>\nmany\nn/a x, y 2\n&lt;b&gt;\n");
}

#[test]
fn template_engine_reports_errors() {
    let dir = TempDir::new("tmplerr");
    let tmpl = dir.write("bad.tmpl", "{% for x in xs %}never closed");
    let data = dir.write("data.json", "{}");
    let o = run_example(
        "template.sand",
        &[tmpl.to_str().unwrap(), data.to_str().unwrap()],
        Config::new().policy(Policy::rules(RulePolicy::new().allow_read(dir.path()))),
    );
    assert_eq!(o.result.as_ref().unwrap().exit_code, 1);
    assert!(o.stderr.contains("unexpected end of template"), "{}", o.stderr);
}

#[test]
fn text_adventure() {
    let input = "look\ntake lamp\ngo north\ne\nuse key\ns\ne\ntake key\ni\nw\nn\nuse key\neast\n";
    let config = Config::new().policy(Policy::rules(RulePolicy::new().allow_stdin())).stdin_bytes(input.as_bytes().to_vec());
    let o = run_example("adventure.sand", &[], config);
    let out = ok(&o);
    for expected in [
        "== Great Hall ==\nA dusty hall with portraits on the walls.\nExits: north, east\nYou see: lamp\n",
        "Taken: lamp.",
        "== Library ==",
        "The door to the east is locked.",
        "You don't have key.",
        "You carry: key, lamp",
        "You unlock the door to the east.",
        "== Vault ==",
        "You found the treasure in 6 moves. You win!",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    // Quitting early.
    let config = Config::new().policy(Policy::rules(RulePolicy::new().allow_stdin())).stdin_bytes(b"dance\nquit\n".to_vec());
    let o = run_example("adventure.sand", &[], config);
    let out = ok(&o);
    assert!(out.contains("I don't understand that."));
    assert!(out.ends_with("Goodbye.\n"));
}

#[test]
fn persisted_inverted_index() {
    let dir = TempDir::new("idx");
    dir.write("docs/a.txt", "The quick brown fox jumps over the lazy dog");
    dir.write("docs/b.txt", "A quick brown dog outpaces a quick red fox");
    dir.write("docs/sub/c.md", "Lorem ipsum dolor sit amet");
    dir.write("docs/d.log", "fox fox fox");
    let config = || Config::new().cwd(dir.path()).policy(Policy::rules(RulePolicy::new().allow_read(dir.path()).allow_write(dir.path())));
    let o = run_example("search_index.sand", &["index", "idx.json", "docs"], config());
    assert_eq!(ok(&o), "indexed 3 documents, 14 terms\n");
    assert!(dir.path().join("idx.json").exists());
    assert!(!dir.path().join("idx.json.tmp").exists());

    // Each search is a fresh engine: the index must come from disk.
    let o = run_example("search_index.sand", &["search", "idx.json", "quick"], config());
    assert_eq!(ok(&o), "0.2618  docs/b.txt\n0.1309  docs/a.txt\n");
    let o = run_example("search_index.sand", &["search", "idx.json", "lorem", "FOX"], config());
    assert_eq!(ok(&o), "0.2773  docs/sub/c.md\n0.1309  docs/a.txt\n0.1309  docs/b.txt\n");
    let o = run_example("search_index.sand", &["search", "idx.json", "zebra"], config());
    assert_eq!(ok(&o), "no results for \"zebra\"\n");

    dir.write("docs/new.txt", "zebra zebra fox");
    let o = run_example("search_index.sand", &["add", "idx.json", "docs/new.txt"], config());
    assert_eq!(ok(&o), "added 1 documents\n");
    let o = run_example("search_index.sand", &["search", "idx.json", "zebra"], config());
    assert_eq!(ok(&o), "1.0730  docs/new.txt\n");
    let o = run_example("search_index.sand", &["stats", "idx.json"], config());
    assert_eq!(ok(&o), "documents: 4\nterms: 15\nmost common: fox(3), brown(2), dog(2), quick(2), amet(1)\n");

    // Writing outside the allowed directory is refused by the sandbox.
    let o = run_example(
        "search_index.sand",
        &["index", "idx.json", "docs"],
        Config::new().cwd(dir.path()).policy(Policy::rules(RulePolicy::new().allow_read(dir.path()))),
    );
    assert_eq!(o.result.as_ref().unwrap().exit_code, 1);
    assert!(o.stderr.contains("permission_denied"), "{}", o.stderr);
}

#[test]
fn http_client_fetches_and_parses_a_page() {
    let page = r##"<!doctype html>
<html><head><title>Test &amp; Page</title></head>
<body>
<h1>Main <em>Heading</em></h1>
<h2>Sub</h2>
<a href="/about">About</a>
<a class="x" href='https://other.example/x?y=1'>Other</a>
<a href="#top">Top</a>
<a href="/about">Duplicate</a>
<a href="rel/page.html">Relative</a>
</body></html>"##;
    let server = http_server(move |req| match req.path() {
        "/old" => HttpResponse::new(301, "").header("location", "/page"),
        "/page" => {
            let mut r = HttpResponse::new(200, page).header("content-type", "text/html; charset=utf-8");
            r.chunked = true;
            r
        }
        "/data.json" => HttpResponse::new(200, "{}").header("content-type", "application/json"),
        _ => HttpResponse::new(404, "not found"),
    });
    let base = format!("http://{}", server.addr);
    let policy = RulePolicy::new().allow_net(&server.addr.to_string());
    let o = run_example(
        "http_fetch.sand",
        &[&format!("{base}/old"), &format!("{base}/missing"), &format!("{base}/data.json")],
        Config::new().policy(Policy::rules(policy)),
    );
    let expected = format!(
        "{base}/page [200]\ntitle: Test & Page\nheadings:\n  Main Heading\n    Sub\nlinks (3):\n  {base}/about\n  https://other.example/x?y=1\n  {base}/rel/page.html\n{base}/missing: http: {base}/missing returned status 404\n{base}/data.json: invalid: {base}/data.json is not HTML (application/json)\n"
    );
    assert_eq!(ok(&o), expected);
    let reqs = server.requests.lock().unwrap();
    assert!(reqs.iter().any(|r| r.header("user-agent") == Some("sand-fetch/0.1")));
}

#[test]
fn http_client_is_sandboxed() {
    let server = http_server(|_| HttpResponse::new(200, "<title>x</title>").header("content-type", "text/html"));
    let o = run_example("http_fetch.sand", &[&format!("http://{}/", server.addr)], Config::new().policy(Policy::deny_all()));
    assert!(ok(&o).contains("permission_denied"), "{}", o.stdout);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn tcp_chat_server() {
    let out = MemorySink::new();
    let err = MemorySink::new();
    let policy = RulePolicy::new().allow_listen("127.0.0.1:0");
    let config = Config::new().policy(Policy::rules(policy)).stdout(out.clone()).stderr(err.clone()).args(vec!["127.0.0.1:0".into()]);
    let mut engine = Engine::new(config);
    let interrupt = engine.interrupt_handle();
    let server = std::thread::spawn(move || rt().block_on(engine.run_file(example("chat_server.sand"))));

    let start = Instant::now();
    let addr = loop {
        let text = out.contents();
        if let Some(line) = text.lines().find(|l| l.starts_with("listening on ")) {
            break line["listening on ".len()..].to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "server did not start: {text} {}", err.contents());
        std::thread::sleep(Duration::from_millis(10));
    };

    let connect = || {
        let s = std::net::TcpStream::connect(&addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let reader = BufReader::new(s.try_clone().unwrap());
        (s, reader)
    };
    let read_line = |r: &mut BufReader<std::net::TcpStream>| {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    };

    let (mut a, mut ar) = connect();
    assert_eq!(read_line(&mut ar), "welcome guest1");
    let (mut b, mut br) = connect();
    assert_eq!(read_line(&mut br), "welcome guest2");
    assert_eq!(read_line(&mut ar), "* guest2 joined");

    a.write_all(b"/nick alice\n").unwrap();
    assert_eq!(read_line(&mut ar), "you are now alice");
    assert_eq!(read_line(&mut br), "* guest1 is now alice");

    a.write_all(b"hello everyone\n").unwrap();
    assert_eq!(read_line(&mut br), "alice: hello everyone");

    b.write_all(b"/who\n").unwrap();
    assert_eq!(read_line(&mut br), "online: alice, guest2");

    b.write_all(b"/quit\n").unwrap();
    assert_eq!(read_line(&mut ar), "* guest2 left");

    // Abrupt disconnects are handled too.
    let (c, mut cr) = connect();
    assert_eq!(read_line(&mut cr), "welcome guest3");
    assert_eq!(read_line(&mut ar), "* guest3 joined");
    drop(cr);
    drop(c);
    assert_eq!(read_line(&mut ar), "* guest3 left");

    interrupt.interrupt();
    let result = server.join().unwrap();
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
    assert!(err.contents().contains("INFO client joined"));
}

#[test]
fn chat_server_cannot_listen_without_permission() {
    let o = run_example("chat_server.sand", &["127.0.0.1:0"], Config::new().policy(Policy::deny_all()));
    assert_eq!(o.result.as_ref().unwrap().exit_code, 1);
    assert!(o.stderr.contains("permission_denied"), "{}", o.stderr);
}

#[test]
fn s3_signing_tests_pass() {
    let mut engine = Engine::new(Config::new());
    let report = rt().block_on(engine.run_tests(example("s3/s3.sand"))).unwrap();
    assert_eq!(report.failed(), 0, "{}", report.render());
    assert_eq!(report.passed(), 3);
}

#[test]
fn s3_client_against_mock_server() {
    let store: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let s = store.clone();
    let server = http_server(move |req| {
        let auth = req.header("authorization").unwrap_or("");
        if !auth.starts_with("AWS4-HMAC-SHA256 Credential=TESTKEY/") || !auth.contains("Signature=") {
            return HttpResponse::new(403, "<Error><Code>AccessDenied</Code><Message>bad auth</Message></Error>");
        }
        let sha = req.header("x-amz-content-sha256").unwrap_or("");
        if sha.len() != 64 || req.header("x-amz-date").is_none() {
            return HttpResponse::new(400, "<Error><Code>BadRequest</Code><Message>missing headers</Message></Error>");
        }
        let path = percent_decode(req.path().trim_start_matches('/'));
        let mut store = s.lock().unwrap();
        match req.method.as_str() {
            "PUT" => {
                store.insert(path, req.body.clone());
                HttpResponse::new(200, "").header("etag", "\"abc\"")
            }
            "GET" if req.query().contains("list-type=2") => {
                let bucket = path.trim_end_matches('/');
                let params: BTreeMap<String, String> =
                    req.query().split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_string(), v.replace("%2F", "/"))).collect();
                let prefix = format!("{bucket}/{}", params.get("prefix").cloned().unwrap_or_default());
                let keys: Vec<(&String, &Vec<u8>)> = store.iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
                // Two keys per page to exercise continuation tokens.
                let start: usize = params.get("continuation-token").and_then(|t| t.parse().ok()).unwrap_or(0);
                let page: Vec<_> = keys.iter().skip(start).take(2).collect();
                let truncated = start + 2 < keys.len();
                let mut xml = String::from("<ListBucketResult>");
                for (k, v) in page {
                    let key = k[bucket.len() + 1..].replace('&', "&amp;");
                    xml.push_str(&format!(
                        "<Contents><Key>{key}</Key><LastModified>2024-01-01T00:00:00.000Z</LastModified><ETag>&quot;e&quot;</ETag><Size>{}</Size></Contents>",
                        v.len()
                    ));
                }
                xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
                if truncated {
                    xml.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", start + 2));
                }
                xml.push_str("</ListBucketResult>");
                HttpResponse::new(200, xml)
            }
            "GET" | "HEAD" => match store.get(&path) {
                Some(v) => {
                    HttpResponse::new(200, v.clone()).header("etag", "\"abc\"").header("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT")
                }
                None => HttpResponse::new(404, "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"),
            },
            "DELETE" => {
                store.remove(&path);
                HttpResponse::new(204, "")
            }
            _ => HttpResponse::new(405, ""),
        }
    });

    let dir = TempDir::new("s3");
    dir.write("upload.txt", "hello from sand");
    let endpoint = format!("http://{}", server.addr);
    let run = |args: &[&str]| {
        let env = vec![
            ("AWS_ACCESS_KEY_ID".to_string(), "TESTKEY".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "secret".to_string()),
            ("S3_ENDPOINT".to_string(), endpoint.clone()),
        ];
        let policy = RulePolicy::new()
            .allow_net(&server.addr.to_string())
            .allow_read(dir.path())
            .allow_write(dir.path())
            .allow_env("AWS_ACCESS_KEY_ID")
            .allow_env("AWS_SECRET_ACCESS_KEY")
            .allow_env("AWS_SESSION_TOKEN")
            .allow_env("AWS_REGION")
            .allow_env("S3_ENDPOINT");
        run_example("s3/cli.sand", args, Config::new().cwd(dir.path()).env(env).policy(Policy::rules(policy)))
    };

    let o = run(&["put", "bucket", "notes/one.txt", "upload.txt"]);
    assert_eq!(ok(&o), "uploaded upload.txt to s3://bucket/notes/one.txt (15B)\n");
    for (k, v) in [("notes/two.txt", "2"), ("notes/three&four.txt", "34"), ("other.txt", "x")] {
        dir.write("tmp.txt", v);
        ok(&run(&["put", "bucket", k, "tmp.txt"]));
    }
    assert_eq!(store.lock().unwrap().get("bucket/notes/one.txt").unwrap(), b"hello from sand");

    let o = run(&["cat", "bucket", "notes/one.txt"]);
    assert_eq!(ok(&o), "hello from sand");

    let o = run(&["ls", "bucket", "notes/"]);
    let row = |size: &str, key: &str| format!("{:<24} {:>8}  {}\n", "2024-01-01T00:00:00.000Z", size, key);
    let expected = row("15B", "notes/one.txt") + &row("2B", "notes/three&four.txt") + &row("1B", "notes/two.txt") + "3 objects, 18B\n";
    assert_eq!(ok(&o), expected);

    let o = run(&["stat", "bucket", "notes/one.txt"]);
    assert_eq!(ok(&o), "notes/one.txt: 15 bytes, etag abc, modified Mon, 01 Jan 2024 00:00:00 GMT\n");

    let o = run(&["get", "bucket", "notes/one.txt", "down.txt"]);
    assert_eq!(ok(&o), "downloaded s3://bucket/notes/one.txt to down.txt (15B)\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("down.txt")).unwrap(), "hello from sand");

    let o = run(&["rm", "bucket", "notes/one.txt"]);
    assert_eq!(ok(&o), "deleted s3://bucket/notes/one.txt\n");
    let o = run(&["stat", "bucket", "notes/one.txt"]);
    assert_eq!(ok(&o), "not found: s3://bucket/notes/one.txt\n");

    let o = run(&["cat", "bucket", "missing"]);
    assert_eq!(o.result.as_ref().unwrap().exit_code, 1);
    assert!(o.stderr.contains("NoSuchKey: The specified key does not exist."), "{}", o.stderr);

    // Every request was signed, and the signed host matches what was sent.
    for r in server.requests.lock().unwrap().iter() {
        let auth = r.header("authorization").unwrap();
        assert!(auth.contains("SignedHeaders=") && auth.contains("host"), "{auth}");
        assert_eq!(r.header("host").unwrap(), server.addr.to_string());
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[test]
fn s3_cli_requires_credentials_permission() {
    let policy = Policy::custom(|cap: &Capability| match cap {
        Capability::Env(_) => Decision::Deny,
        _ => Decision::Allow,
    });
    let o = run_example("s3/cli.sand", &["ls", "b"], Config::new().policy(policy));
    assert_eq!(o.result.as_ref().unwrap().exit_code, 1);
    assert!(o.stderr.contains("AWS_ACCESS_KEY_ID is not set"), "{}", o.stderr);
}
