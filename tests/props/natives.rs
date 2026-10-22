//! Property tests for native function conversions and single-execution guarantees.

use super::{int, int_lit, str_lit};
use proptest::prelude::*;
use sandlang::{Config, Engine, Error, LimitKind, Limits, MemorySink, NativeModule};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn run(config: Config, src: &str) -> (Result<sandlang::Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("main.sand", src));
    (r, out.contents())
}

fn map_text(map: &BTreeMap<i64, String>) -> String {
    let entries: Vec<String> = map.iter().map(|(k, v)| format!("{k}: {}", str_lit(v))).collect();
    if entries.is_empty() { "[:]".to_string() } else { format!("[{}]", entries.join(", ")) }
}

#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "conv")]
struct Inner {
    n: i64,
    tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "conv")]
struct Outer {
    name: String,
    flag: bool,
    ratio: f64,
    inner: Inner,
    items: Vec<Inner>,
    maybe: Option<Inner>,
    scores: BTreeMap<i64, String>,
    pair: (i64, Option<String>),
    #[sand(var)]
    count: i64,
}

fn text() -> impl Strategy<Value = String> {
    "[a-z\"\\\n é]{0,6}"
}

fn inner() -> impl Strategy<Value = Inner> {
    (int(), prop::collection::vec(text(), 0..3)).prop_map(|(n, tags)| Inner { n, tags })
}

fn outer() -> impl Strategy<Value = Outer> {
    let head = (text(), any::<bool>(), -400i32..400, inner(), prop::collection::vec(inner(), 0..3), prop::option::of(inner()));
    let tail = (prop::collection::btree_map(-9i64..9, text(), 0..4), int(), prop::option::of(text()), int());
    (head, tail).prop_map(|((name, flag, quarters, inner, items, maybe), (scores, a, b, count))| Outer {
        name,
        flag,
        ratio: f64::from(quarters) / 4.0,
        inner,
        items,
        maybe,
        scores,
        pair: (a, b),
        count,
    })
}

fn inner_lit(x: &Inner) -> String {
    let tags: Vec<String> = x.tags.iter().map(|t| str_lit(t)).collect();
    format!("conv.Inner {{ n: {}, tags: [{}] }}", int_lit(x.n), tags.join(", "))
}

fn outer_lit(x: &Outer) -> String {
    let items: Vec<String> = x.items.iter().map(inner_lit).collect();
    let maybe = x.maybe.as_ref().map_or("None".to_string(), |i| format!("Some({})", inner_lit(i)));
    let scores: Vec<String> = x.scores.iter().map(|(k, v)| format!("{}: {}", int_lit(*k), str_lit(v))).collect();
    let scores = if scores.is_empty() { "[:]".to_string() } else { format!("[{}]", scores.join(", ")) };
    let b = x.pair.1.as_ref().map_or("None".to_string(), |s| format!("Some({})", str_lit(s)));
    format!(
        "conv.Outer {{ name: {}, flag: {}, ratio: ({:?}), inner: {}, items: [{}], maybe: {maybe}, scores: {scores}, pair: ({}, {b}), count: {} }}",
        str_lit(&x.name),
        x.flag,
        x.ratio,
        inner_lit(&x.inner),
        items.join(", "),
        int_lit(x.pair.0),
        int_lit(x.count)
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A struct converts to Rust and back without change: Rust receives the
    /// value the script built, and the script sees the value Rust built as
    /// equal to its own, in every execution mode.
    #[test]
    fn structs_round_trip(value in outer(), gc_stress in any::<bool>()) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = NativeModule::new("conv");
        m.bind::<Inner>().bind::<Outer>();
        let s = seen.clone();
        m.function("echo", move |x: Outer| {
            s.lock().unwrap().push(x.clone());
            x
        });
        let made = value.clone();
        m.function("make", move || made.clone());
        m.function("bump", |mut x: Outer| {
            x.count = x.count.wrapping_add(1);
            x.inner.n = x.inner.n.wrapping_mul(2);
            x
        });
        let src = format!(
            "import conv\nlet o = {}\nlet back = conv.echo(o)\nprint(back == o)\nprint(conv.make() == o)\nprint(\"\\(back)\" == \"\\(o)\")\n\
             let b = conv.bump(o)\nprint(b.count)\nprint(b.inner.n)\nb.count = 0\nprint(b.count)",
            outer_lit(&value)
        );
        let (r, out) = run(Config::new().module(m).gc_stress(gc_stress), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        let bumped = format!("{}\n{}\n", value.count.wrapping_add(1), value.inner.n.wrapping_mul(2));
        prop_assert_eq!(out, format!("true\ntrue\ntrue\n{bumped}0\n"), "{}", src);
        prop_assert_eq!(seen.lock().unwrap().clone(), vec![value]);
    }

    #[test]
    fn maps_and_sets_round_trip(map in prop::collection::btree_map(-50i64..50, "[a-z\"\\\\]{0,4}", 0..12)) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = NativeModule::new("conv");
        let s = seen.clone();
        m.function("hash_map", move |x: HashMap<i64, String>| {
            s.lock().unwrap().push(x.clone().into_iter().collect::<BTreeMap<_, _>>());
            x
        });
        m.function("btree_map", |x: BTreeMap<i64, String>| x);
        m.function("hash_set", |x: HashSet<i64>| x);
        m.function("btree_set", |x: BTreeSet<i64>| x);
        let inserts: String = map.iter().rev().map(|(k, v)| format!("m[{k}] = {}\n", str_lit(v))).collect();
        let src = format!(
            "import conv\nlet m: [int: str] = [:]\n{inserts}print(conv.hash_map(m))\nprint(conv.btree_map(m))\n\
             let s = m.keys().to_set()\nprint(conv.hash_set(s).to_list())\nprint(conv.btree_set(s).to_list())"
        );
        let (r, out) = run(Config::new().module(m), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        let keys = format!("[{}]", map.keys().map(|k| k.to_string()).collect::<Vec<_>>().join(", "));
        prop_assert_eq!(out, format!("{0}\n{0}\n{keys}\n{keys}\n", map_text(&map)));
        let seen = seen.lock().unwrap().clone();
        prop_assert_eq!(seen, vec![map]);
    }

    #[test]
    fn natives_run_once_per_call(limit_kib in 1024usize..8192, garbage in 0usize..4_000_000, size in 0usize..800_000, calls in 1usize..6) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let mut m = NativeModule::new("q");
        m.function("pop", move || {
            c.fetch_add(1, Ordering::SeqCst);
            "x".repeat(size)
        });
        let limits = Limits { max_memory: Some(limit_kib << 10), ..Limits::default() };
        let src = format!(
            "import q\nvar s = \"y\".repeat({garbage})\ns = \"z\".repeat({garbage})\nvar n = 0\n\
             for _ in 0..{calls} {{\n    n += q.pop().len()\n}}\nprint(n / {})",
            size.max(1)
        );
        let (r, out) = run(Config::new().module(m).limits(limits), &src);
        let count = count.load(Ordering::SeqCst);
        match r {
            Ok(_) => {
                prop_assert_eq!(count, calls);
                let expect = if size == 0 { 0 } else { calls };
                prop_assert_eq!(out, format!("{expect}\n"));
            }
            Err(Error::Limit(LimitKind::Memory)) => prop_assert!(count <= calls, "{count} calls of {calls}"),
            Err(e) => prop_assert!(false, "{e}"),
        }
    }
}
