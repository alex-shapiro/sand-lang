//! Policy benchmarks: parsing a large policy file, building its indices,
//! and checking requests against it. Run with `cargo bench --bench
//! policy`; `cargo test --benches` runs each once.
//!
//! The policy has many literal path rules (as "always" answers to prompts
//! accumulate; `BENCH_RULES`, default 100000), wildcard path rules, and
//! http rules for many hosts.

use sandlang::policy::{PolicyFile, normalize_path};
use sandlang::{Capability, Decision};
use std::fmt::Write;
use std::time::{Duration, Instant};

fn main() {
    let quick = std::env::var("BENCH_QUICK").is_ok() || !std::env::args().any(|a| a == "--bench");
    let rules = if quick { 1_000 } else { std::env::var("BENCH_RULES").ok().and_then(|n| n.parse().ok()).unwrap_or(100_000) };
    let base = normalize_path(&std::env::temp_dir()).join("sand-policy-bench");
    let mut text = String::new();
    for i in 0..rules {
        let _ = writeln!(text, "allow read ./data/d{}/f{i}.txt", i % 1000);
        let _ = writeln!(text, "allow http GET POST api{}.example.com/v1/users/{i}/**", i % 1000);
    }
    for i in 0..rules / 100 {
        let _ = writeln!(text, "deny ./data/d{i}/**/*.secret");
        let _ = writeln!(text, "deny http DELETE *.example{i}.org/admin");
    }
    text.push_str("allow read ./data/*/public\ndeny ./**/.env\nallow net *.example.net:443\n");

    let start = Instant::now();
    let file = PolicyFile::parse(&text, &base, None).unwrap();
    let parse = start.elapsed();
    let policy = file.policy();
    let checks: Vec<(Capability, Decision)> = vec![
        (Capability::FsRead(base.join("data/d7/f7.txt")), Decision::Allow),
        (Capability::FsRead(base.join("data/d7/f8.txt")), Decision::Deny),
        (Capability::FsRead(base.join("data/d7/x/y/k.secret")), Decision::Deny),
        (Capability::FsRead(base.join("data/d9/public/a/b")), Decision::Allow),
        (Capability::FsRead(base.join("data/d9/public/.env")), Decision::Deny),
        (http("POST", "api5.example.com", "/v1/users/5/repos"), Decision::Allow),
        (http("POST", "api5.example.com", "/v1/users/6/repos"), Decision::Deny),
        (http("DELETE", "x.example3.org", "/admin/x"), Decision::Deny),
        (http("GET", "a.example.net", "/"), Decision::Allow),
    ];
    // The first check builds the indices.
    let start = Instant::now();
    assert_eq!(policy.check_normalized(&checks[0].0), checks[0].1);
    let build = start.elapsed();

    let rounds = if quick { 10 } else { 20_000 };
    let start = Instant::now();
    for _ in 0..rounds {
        for (cap, want) in &checks {
            assert_eq!(policy.check_normalized(cap), *want, "{cap}");
        }
    }
    let per_check = start.elapsed() / (rounds * checks.len()) as u32;
    let lines = text.lines().count();
    println!("{lines} rules: parse {}, build {}, check {}", ms(parse), ms(build), us(per_check));
    let start = Instant::now();
    let warnings = sandlang::policy::lint_policy(&text, &base, None).unwrap();
    println!("lint: {} ({} warnings)", ms(start.elapsed()), warnings.len());
    let start = Instant::now();
    assert_eq!(sandlang::policy::format_policy(&text).unwrap(), text);
    println!("format: {}", ms(start.elapsed()));
}

fn http(method: &str, host: &str, path: &str) -> Capability {
    Capability::Http { method: method.into(), scheme: "https".into(), host: host.into(), port: 443, path: path.into() }
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1e3)
}

fn us(d: Duration) -> String {
    format!("{:.2}us", d.as_secs_f64() * 1e6)
}
