//! Properties of native functions: conversions of maps and sets, and calls
//! that run exactly once under any memory limit.

use super::str_lit;
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

/// The Sand text of `map` in key order, as `print` shows a map.
fn map_text(map: &BTreeMap<i64, String>) -> String {
    let entries: Vec<String> = map.iter().map(|(k, v)| format!("{k}: {}", str_lit(v))).collect();
    if entries.is_empty() { "[:]".to_string() } else { format!("[{}]", entries.join(", ")) }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Maps and sets cross between Sand and Rust unchanged, and a Rust
    /// `HashMap` or `HashSet` becomes a Sand map or set in key order.
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
        // Sand keeps insertion order: insert in reverse so that key order is a change.
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

    /// A native with a side effect runs once per call whatever the memory
    /// limit and the garbage on the heap: its result is made after the side
    /// effect, so a collection never calls it again.
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
