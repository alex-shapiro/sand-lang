//! Randomized robustness ("fuzz") tests.
//!
//! Every input must produce a compile diagnostic, a runtime error, or a normal
//! result, without panicking, hanging, or returning an internal error. Set
//! `FUZZ_ITERS` to increase the iteration count. Coverage-guided fuzz targets
//! for `cargo fuzz` are in `fuzz/`.

mod common;

use common::rt;
use sandlang::{Config, Engine, Error, Limits, MemorySink};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

fn iters(default: usize) -> usize {
    std::env::var("FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn limited_config() -> Config {
    Config::new().policy(sandlang::RulePolicy::standard()).stdout(MemorySink::new()).stderr(MemorySink::new()).limits(Limits {
        max_instructions: Some(200_000),
        max_memory: Some(32 << 20),
        max_wall_time: Some(Duration::from_secs(2)),
        max_call_depth: 500,
        ..Limits::default()
    })
}

/// Compiles and runs `src`, asserting that the host does not panic and that the
/// engine does not return an internal error.
fn exercise(src: &str) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut engine = Engine::new(limited_config()).unwrap();
        rt().block_on(engine.run_source("fuzz.sand", src))
    }));
    match result {
        Err(_) => panic!("host panic on input:\n-----\n{src}\n-----"),
        Ok(Err(e @ Error::Internal { .. })) => panic!("{e}\non input:\n-----\n{src}\n-----"),
        Ok(_) => {}
    }
}

fn check_only(src: &str) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let engine = Engine::new(Config::new()).unwrap();
        let _ = engine.check_source("fuzz.sand", src);
        let _ = sandlang::fmt::format_source(src);
        let _ = engine.lint_source("fuzz.sand", src);
    }));
    if result.is_err() {
        panic!("host panic in front end on input:\n-----\n{src:?}\n-----");
    }
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(1);
    for _ in 0..iters(400) {
        let len = rng.below(200);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        check_only(&String::from_utf8_lossy(&bytes));
    }
}

const TOKENS: &[&str] = &[
    "let",
    "var",
    "fn",
    "struct",
    "enum",
    "match",
    "if",
    "else",
    "for",
    "in",
    "while",
    "loop",
    "return",
    "break",
    "continue",
    "defer",
    "@outer",
    "spawn",
    "import",
    "from",
    "pub",
    "test",
    "type",
    "extend",
    "trait",
    "impl",
    "native",
    "and",
    "or",
    "not",
    "self",
    "Self",
    "true",
    "false",
    "inf",
    "nan",
    "x",
    "y",
    "f",
    "Point",
    "Some",
    "None",
    "Ok",
    "Err",
    "0",
    "1",
    "42",
    "1.5",
    "10ms",
    "\"s\"",
    "\"a\\(x)b\"",
    "r\"raw\"",
    "b\"by\"",
    "\"\"\"\n  m\n  \"\"\"",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ",",
    ":",
    ";",
    ".",
    "..",
    "..=",
    "=>",
    "->",
    "??",
    "?",
    "+",
    "-",
    "*",
    "/",
    "%",
    "**",
    "==",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "=",
    "+=",
    "&",
    "|",
    "^",
    "~",
    "<<",
    ">>",
    "\n",
    "\n",
    "\n",
    " ",
    "int",
    "str",
    "[int]",
    "_",
    "@",
];

#[test]
fn token_soup_never_panics() {
    let mut rng = Rng(2);
    for _ in 0..iters(400) {
        let n = 1 + rng.below(40);
        let src: Vec<&str> = (0..n).map(|_| *rng.pick(TOKENS)).collect();
        let src = src.join(" ");
        check_only(&src);
        exercise(&src);
    }
}

fn corpus() -> Vec<String> {
    let mut out = Vec::new();
    let spec_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/spec");
    let mut files: Vec<_> = std::fs::read_dir(&spec_dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for case in text.split("\n=== ") {
            let body: Vec<&str> = case.lines().skip(1).take_while(|l| !l.starts_with("--- ")).collect();
            if !body.is_empty() {
                out.push(body.join("\n"));
            }
        }
    }
    for ex in ["life.sand", "calc.sand", "template.sand", "adventure.sand", "search_index.sand", "ledger.sand", "s3/s3.sand"] {
        out.push(std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(ex)).unwrap());
    }
    out
}

fn mutate(rng: &mut Rng, src: &str) -> String {
    let mut chars: Vec<char> = src.chars().collect();
    for _ in 0..1 + rng.below(4) {
        if chars.is_empty() {
            break;
        }
        let i = rng.below(chars.len());
        match rng.below(6) {
            0 => {
                let j = (i + rng.below(20)).min(chars.len());
                chars.drain(i..j);
            }
            1 => {
                let j = (i + rng.below(30)).min(chars.len());
                let dup: Vec<char> = chars[i..j].to_vec();
                let at = rng.below(chars.len());
                chars.splice(at..at, dup);
            }
            2 => {
                let tok: Vec<char> = rng.pick(TOKENS).chars().collect();
                chars.splice(i..i, tok);
            }
            3 => {
                let j = rng.below(chars.len());
                chars.swap(i, j);
            }
            4 => chars[i] = *rng.pick(&['(', ')', '{', '}', '[', ']', '"', '\n', '0', 'x', '.', '?']),
            _ => {
                let lines: Vec<String> = chars.iter().collect::<String>().lines().map(String::from).collect();
                if lines.len() > 1 {
                    let mut lines = lines;
                    let a = rng.below(lines.len());
                    let b = rng.below(lines.len());
                    lines.swap(a, b);
                    chars = lines.join("\n").chars().collect();
                }
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn mutated_programs_never_panic() {
    let corpus = corpus();
    assert!(corpus.len() > 100);
    let mut rng = Rng(3);
    for _ in 0..iters(600) {
        let base = rng.pick(&corpus).clone();
        let src = mutate(&mut rng, &base);
        check_only(&src);
        exercise(&src);
    }
}

#[test]
fn unmutated_corpus_runs_under_limits() {
    // Every corpus program terminates (successfully or not) under tight limits.
    for src in corpus() {
        exercise(&src);
    }
}

#[test]
fn formatter_on_parseable_mutants_is_stable() {
    let corpus = corpus();
    let mut rng = Rng(4);
    for _ in 0..iters(400) {
        let base = rng.pick(&corpus).clone();
        let src = mutate(&mut rng, &base);
        if let Ok(once) = sandlang::fmt::format_source(&src) {
            let twice = sandlang::fmt::format_source(&once)
                .unwrap_or_else(|e| panic!("formatted output does not parse: {e}\n--- input\n{src}\n--- output\n{once}"));
            assert_eq!(once, twice, "formatter not idempotent for:\n{src}");
        }
    }
}

#[test]
fn deep_nesting_is_rejected_gracefully() {
    for (open, close) in [("(", ")"), ("[", "]"), ("{", "}"), ("Some(", ")"), ("-", ""), ("not ", ""), ("x => ", "")] {
        let n = 100_000;
        let src = format!("let v = {}1{}", open.repeat(n), close.repeat(n));
        check_only(&src);
        exercise(&src);
    }
    let long_chain = format!("let v = {}1", "1 + ".repeat(100_000));
    check_only(&long_chain);
    exercise(&long_chain);
    let long_method_chain = format!("let v = [1]{}", ".copy()".repeat(50_000));
    check_only(&long_method_chain);
    exercise(&long_method_chain);
    let many_ifs = format!("{}1{}", "if true { ".repeat(5_000), " }".repeat(5_000));
    check_only(&many_ifs);
    let nested_types = format!("let v: {}int{} = 1", "[".repeat(10_000), "]".repeat(10_000));
    check_only(&nested_types);
    let nested_patterns = format!("match 1 {{ {}_{} => 1 }}", "(".repeat(10_000), ")".repeat(10_000));
    check_only(&nested_patterns);
    let interp = format!("let s = \"{}x{}\"", "{\"".repeat(5_000), "\"}".repeat(5_000));
    check_only(&interp);
}

#[test]
fn hostile_runtime_programs_are_contained() {
    let programs = [
        // Exponential allocation.
        "var s = \"x\"\nwhile true { s = s + s }",
        "let xs = [0]\nwhile true { xs.extend(xs.copy()) }",
        // Deep recursion.
        "fn f(n: int) -> int { f(n + 1) + 1 }\nprint(f(0))",
        // Unbounded task spawning.
        "while true { spawn task.sleep(1h) }",
        // Unbounded channel growth.
        "let ch = Channel()\nwhile true { ch.send([1, 2, 3]) }",
        // Huge repeat.
        "print(\"ab\".repeat(9223372036854775807).len())",
        "print([0; 1000000000].len())",
        // Map growth.
        "let m = [:]\nvar i = 0\nwhile true { m[i] = i.to_str()\n i += 1 }",
        // Cyclic structures.
        "struct N { var next: N? }\nlet a = N(next: None)\na.next = Some(a)\nprint(a == a)\nprint(a)",
        // Regex blow-up attempts.
        "import regex\nprint(regex.Regex(\"(a{1000}){1000}\").is_err())",
        // JSON bombs.
        "import json\nlet t = \"[\".repeat(100000)\nprint(json.parse(t).is_err())",
        // Formatting huge widths.
        "print(\"\\(1:999999999)\".len())",
        // Sleeping forever.
        "task.sleep(1000h)",
        "struct P<A, B> { a: A, b: B }\nfn grow<reflect A, reflect B>(n: int, a: A?, b: B?) -> int {\n    let x: P<A, B>? = None\n    grow(n + 1, x, x)\n}\nprint(grow(0, Some(1), Some(2)))",
    ];
    for src in programs {
        let start = std::time::Instant::now();
        exercise(src);
        assert!(start.elapsed() < Duration::from_secs(10), "took too long: {src}");
    }
}

#[test]
fn random_json_and_uri_inputs() {
    let mut rng = Rng(5);
    let alphabet: Vec<char> = "{}[]\":,0123456789.eE+-truefalsnul \\u/abc%?#@:".chars().collect();
    let mut cases = Vec::new();
    for _ in 0..iters(300) {
        let n = rng.below(30);
        let s: String = (0..n).map(|_| *rng.pick(&alphabet)).collect();
        cases.push(s);
    }
    let lits: Vec<String> = cases.iter().map(|s| format!("{s:?}")).collect();
    let src = format!(
        "import json\nimport uri\nimport base64\nimport regex\nimport time\nimport uuid\nvar n = 0\nfor s in [{}] {{\n    if json.parse(s).is_ok() {{ n += 1 }}\n    let typed: Result<json.Json> = json.decode(s)\n    if json.parse(s).ok() != typed.ok() {{ n -= 1000000 }}\n    if uri.parse(s).is_ok() {{ n += 1 }}\n    if base64.decode(s).is_ok() {{ n += 1 }}\n    if regex.Regex(s).is_ok() {{ n += 1 }}\n    if time.Instant.parse(s).is_ok() {{ n += 1 }}\n    if time.ZonedDateTime.parse(s).is_ok() {{ n += 1 }}\n    if time.PlainDate.parse(s).is_ok() {{ n += 1 }}\n    if time.Span.parse(s).is_ok() {{ n += 1 }}\n    if time.TimeZone(s).is_ok() {{ n += 1 }}\n    if uuid.parse(s).is_ok() {{ n += 1 }}\n    let _ = uri.decode_component(s)\n    let _ = s.parse_float()\n}}\nprint(n >= 0)\n",
        lits.join(", ")
    );
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().stdout(out.clone())).unwrap();
    let r = rt().block_on(engine.run_source("fuzz.sand", &src));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out.contents(), "true\n");
}

/// Reproducers found by the coverage-guided targets in `fuzz/` (`cargo +nightly fuzz run ...`).
/// Each must be handled by the front end, formatter and VM without a host panic, and
/// the formatter must stay idempotent on it.
#[test]
fn fuzz_regressions() {
    let cases: &[&str] = &[
        // Duration literals whose value times the unit overflows i128.
        "fn f(b: bool) -> int {\n    match 33333333333333333333333333333333333333333333333333333333333333333333333333h b {\n        true => 1\n    }\n}\n",
        "let d = 3333333333333333333333333333333333333333h",
        // Variant patterns on a scrutinee of error type or `never`.
        "let tasks = urks.map(u => spawn fetch(u))\nfor (url, t) in urls.zip(tasks) {\n    match t.wait() {\n        Ok(page) => print(page)\n        Err(e) => print(\"\\(url): \\(e)\")\n    }\n}\n",
        "fn f() -> int {\n    match fail(\"x\") {\n        Some(v) => v\n        None => 0\n    }\n}\nprint(f())\n",
        "fn f() -> int {\n    match [fail(\"x\")] {\n        [Ok(Some((a, b)))] => a + b\n        _ => 0\n    }\n}\nprint(f())\n",
        // A trailing comment ending in `}` after a one-line declaration.
        "import json\nstruct Item { name: str }//\0\0\0\0e name: str }//\0\0\0\0e mOrdson.decodesth.\u{19}\u{12}\0\0e",
        "enum E { A } // }",
        // Two `?` in a row (not the `??` operator).
        "l ? ?",
        // A `\x` escape followed by a multi-byte character.
        "let maybe = Some(3)\nprint(b\"\\x0П\")\nprint(b\"\\xП\")\nlet x = b\"\\x",
    ];
    for src in cases {
        check_only(src);
        exercise(src);
        if let Ok(once) = sandlang::fmt::format_source(src) {
            let twice = sandlang::fmt::format_source(&once).expect("formatted output must parse");
            assert_eq!(once, twice, "formatter not idempotent for:\n{src}");
        }
    }
}

#[test]
fn hostile_native_work_is_contained() {
    // Single operations doing unbounded work: each must be stopped by a
    // limit while it runs, not after.
    let dag = "enum T { Leaf(int), Node(T, T) }\nvar a = T.Leaf(1)\nvar b = T.Leaf(1)\nfor i in 0..60 {\n    a = T.Node(a, a)\n    b = T.Node(b, b)\n}\n";
    let programs = [
        format!("{dag}print(a == b)"),
        format!("{dag}print(a < b)"),
        format!("{dag}print(a)"),
        format!("{dag}print(\"\\(a:?)\")"),
        format!("{dag}print([a, b, a, b].sorted().len())"),
        format!("{dag}print([a].contains(b))"),
        format!("{dag}print(Set([a]).len())"),
        format!("{dag}let m = [a: 1]\nprint(m.len())"),
        format!("import json\n{dag}print(json.encode(a).len())"),
        "let s = \"x\".repeat(20000000)\nvar n = 0\nwhile true { n += s.len() + s.find(\"y\").unwrap_or(0) }".into(),
        "let s = \"x\".repeat(20000000)\nlet m = [s: 1]\nwhile true { m.contains(s) }".into(),
        "let xs = (0..1000000).to_list()\nwhile true { xs.sorted() }".into(),
        "import crypto\nlet b = \"x\".repeat(20000000).bytes()\nwhile true { crypto.sha256(b) }".into(),
        "import regex\nlet s = \"a\".repeat(20000000)\nlet r = regex.Regex(\"a\")?\nwhile true { r.find_all(s) }".into(),
        "let s = \"ab,\".repeat(5000000)\nwhile true { s.split(\",\") }".into(),
    ];
    for src in &programs {
        let start = std::time::Instant::now();
        exercise(src);
        assert!(start.elapsed() < Duration::from_secs(10), "took too long: {src}");
    }
}

#[test]
fn hostile_compile_inputs_are_contained() {
    let n = 40;
    let mut doubling = String::from("let a0 = 1\n");
    for i in 1..n {
        doubling += &format!("let a{i} = (a{}, a{})\n", i - 1, i - 1);
    }
    let mut repeated = String::from("let a0 = [0, 0]\n");
    let mut ty = String::from("[int]");
    for i in 1..n {
        repeated += &format!("let a{i} = [a{}, a{}]\n", i - 1, i - 1);
        ty = format!("[{ty}]");
    }
    let mut chain = String::from("import json\n");
    for i in 0..20_000 {
        chain += &format!("struct S{i} {{ next: S{} = S{}() }}\n", i + 1, i + 1);
    }
    chain += "struct S20000 { v: int = 1 }\nlet s: Result<S0> = json.decode(\"{}\")\nprint(s.is_ok())\n";
    let programs = [
        format!("import json\nstruct S {{\n    x: {ty} = {{\n{repeated}a{}\n    }}\n}}\nlet s: Result<S> = json.decode(\"{{}}\")\n", n - 1),
        chain,
        doubling,
        format!(
            "enum E {{ A, B, C }}\nfn f(t: ({})) -> int {{\n    match t {{\n        ({}) => 0\n        ({}) => 1\n    }}\n}}\n",
            vec!["E"; n].join(", "),
            vec!["_"; n].join(", "),
            vec!["A | B"; n].join(", ")
        ),
        format!("fn dup<T>(x: T) -> (T, T) {{ (x, x) }}\nlet a = {}1{}\n", "dup(".repeat(n), ")".repeat(n)),
        format!("let s = {}1{}\n", "\"\\(".repeat(200_000), ")\"".repeat(200_000)),
    ];
    for src in &programs {
        let start = std::time::Instant::now();
        check_only(src);
        exercise(src);
        assert!(start.elapsed() < Duration::from_secs(10), "took too long: {}", &src[..src.len().min(200)]);
    }
}
