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

#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "tree")]
struct Tree {
    n: i64,
    children: Vec<Tree>,
}

fn tree() -> impl Strategy<Value = Tree> {
    let leaf = int().prop_map(|n| Tree { n, children: Vec::new() });
    leaf.prop_recursive(6, 48, 4, |child| (int(), prop::collection::vec(child, 0..4)).prop_map(|(n, children)| Tree { n, children }))
}

fn tree_lit(t: &Tree) -> String {
    let children: Vec<String> = t.children.iter().map(tree_lit).collect();
    format!("tree.Tree {{ n: {}, children: [{}] }}", int_lit(t.n), children.join(", "))
}

fn depth(t: &Tree) -> usize {
    1 + t.children.iter().map(depth).max().unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "chain")]
struct Link {
    n: i64,
    next: Option<Box<Link>>,
}

fn link_of(ns: &[i64]) -> Option<Box<Link>> {
    ns.iter().rev().fold(None, |next, &n| Some(Box::new(Link { n, next })))
}

#[derive(sandlang::SandStruct)]
#[sand(module = "dag")]
struct Fan {
    children: Vec<Fan>,
}

#[derive(sandlang::SandStruct)]
#[sand(module = "dag")]
struct HashFan {
    children: HashMap<i64, HashFan>,
}

#[derive(sandlang::SandStruct)]
#[sand(module = "dag")]
struct TreeFan {
    children: BTreeMap<String, TreeFan>,
}

#[derive(sandlang::SandStruct)]
#[sand(module = "dag")]
struct Bin {
    left: Option<Box<Bin>>,
    right: Option<Box<Bin>>,
}

#[derive(sandlang::SandStruct)]
#[sand(module = "dag")]
struct Pair {
    children: Option<Box<(Pair, Pair)>>,
}

const SHAPES: [(&str, &str, &str); 5] = [
    ("dag.Fan { children: [] }", "dag.Fan { children: [a, a] }", "fan"),
    ("dag.HashFan { children: [:] }", "dag.HashFan { children: [0: a, 1: a] }", "hash_fan"),
    ("dag.TreeFan { children: [:] }", "dag.TreeFan { children: [\"l\": a, \"r\": a] }", "tree_fan"),
    ("dag.Bin { left: None, right: None }", "dag.Bin { left: Some(a), right: Some(a) }", "bin"),
    ("dag.Pair { children: None }", "dag.Pair { children: Some((a, a)) }", "pair"),
];

fn shared_dag(shape: usize, levels: u32, limits: Limits) -> Result<sandlang::Outcome, Error> {
    let mut m = NativeModule::new("dag");
    m.bind::<Fan>().bind::<HashFan>().bind::<TreeFan>().bind::<Bin>().bind::<Pair>();
    m.function("fan", |_: Fan| 0);
    m.function("hash_fan", |_: HashFan| 0);
    m.function("tree_fan", |_: TreeFan| 0);
    m.function("bin", |_: Bin| 0);
    m.function("pair", |_: Pair| 0);
    let (leaf, node, call) = SHAPES[shape];
    let src = format!("import dag\nvar a = {leaf}\nfor _ in 0..{} {{\n    a = {node}\n}}\nprint(dag.{call}(a))", levels - 1);
    run(Config::new().module(m).limits(limits), &src).0
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

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
    fn recursive_structs_round_trip(value in tree(), spine in 230usize..270, gc_stress in any::<bool>()) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = NativeModule::new("tree");
        m.bind::<Tree>();
        let s = seen.clone();
        m.function("echo", move |x: Tree| {
            s.lock().unwrap().push(x.clone());
            x
        });
        let src = format!(
            "import tree\nlet t = {}\nprint(tree.echo(t) == t)\nvar deep = t\nfor _ in 0..{spine} {{\n    deep = tree.Tree {{ n: 0, children: [t, deep] }}\n}}\n\
             print(tree.echo(deep) == deep)",
            tree_lit(&value)
        );
        let (r, out) = run(Config::new().module(m).gc_stress(gc_stress), &src);
        if depth(&value) + spine <= 256 {
            prop_assert!(r.is_ok(), "{r:?}\n{src}");
            prop_assert_eq!(out, "true\ntrue\n");
        } else {
            let message = match r {
                Err(Error::Runtime { message, .. }) => message,
                r => return Err(TestCaseError::fail(format!("expected a runtime error, got {r:?}"))),
            };
            prop_assert!(message.contains("value too deeply nested to convert"), "{message}");
            prop_assert_eq!(out, "true\n");
        }
        prop_assert_eq!(&seen.lock().unwrap()[0], &value);
    }

    #[test]
    fn boxed_lists_round_trip(ns in prop::collection::vec(int(), 1..300), from_rust in any::<bool>(), gc_stress in any::<bool>()) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = NativeModule::new("chain");
        m.bind::<Link>();
        let s = seen.clone();
        m.function("echo", move |x: Link| {
            s.lock().unwrap().push(x.clone());
            x
        });
        let made = link_of(&ns);
        m.function("make", move || made.clone().map(|l| *l));
        let backwards: Vec<String> = ns.iter().rev().map(|&n| int_lit(n)).collect();
        let build = if from_rust {
            "chain.make()".to_string()
        } else {
            format!("var l: chain.Link? = None\nfor n in [{}] {{\n    l = Some(chain.Link {{ n, next: l }})\n}}\nl", backwards.join(", "))
        };
        let src = format!("import chain\nlet l = {{\n{build}\n}}?\nprint(chain.echo(l) == l)");
        let (r, out) = run(Config::new().module(m).gc_stress(gc_stress), &src);
        if ns.len() <= 256 {
            prop_assert!(r.is_ok(), "{r:?}\n{src}");
            prop_assert_eq!(out, "true\n");
            prop_assert_eq!(seen.lock().unwrap().clone(), vec![*link_of(&ns).unwrap()]);
        } else {
            let message = match r {
                Err(Error::Runtime { message, .. }) => message,
                r => return Err(TestCaseError::fail(format!("expected a runtime error, got {r:?}"))),
            };
            prop_assert!(message.contains("value too deeply nested to convert"), "{message}");
            prop_assert!(seen.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn shared_structs_cost_fuel_and_memory_for_each_copy(shape in 0..SHAPES.len(), levels in 14u32..20) {
        let copies = (1u64 << levels) - 1;
        let fuel = |n: u64| Limits { max_instructions: Some(n), ..Limits::default() };
        let r = shared_dag(shape, levels, fuel(copies * 8 + 10_000));
        prop_assert!(r.is_ok(), "{r:?}");
        let r = shared_dag(shape, levels, fuel(copies / 2));
        prop_assert!(matches!(r, Err(Error::Limit(LimitKind::Instructions))), "{r:?}");
        let memory = Limits { max_memory: Some(16 << 20), ..Limits::default() };
        let r = shared_dag(shape, 40 + levels, memory);
        prop_assert!(matches!(r, Err(Error::Limit(LimitKind::Memory))), "{r:?}");
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

const NAMES: [&str; 12] = ["match", "fn", "Self", "self", "inf", "nan", "use", "type", "Match", "matches", "_", "_1"];

proptest! {
    #[test]
    fn derive_name_checks_agree_with_the_lexer(s in prop_oneof!["[a-zA-Z_0-9 .]{0,6}", prop::sample::select(NAMES.as_slice()).prop_map(String::from)]) {
        use sandlang::syntax::lexer::{Tok, lex};
        let lexed = lex(&s, 0);
        let toks: Vec<_> = lexed.tokens.iter().filter(|t| !matches!(t.tok, Tok::Semi | Tok::Eof)).collect();
        let ident = lexed.errors.is_empty() && matches!(toks[..], [t] if t.tok == Tok::Ident && t.lo == 0 && t.hi as usize == s.len());
        prop_assert_eq!(sandlang::native::is_name(&s), ident, "{:?}", s);
        prop_assert_eq!(sandlang::native::is_type_name(&s), ident && s.starts_with(|c: char| c.is_ascii_uppercase()), "{:?}", s);
    }
}
