//! Property tests for Rust-bound enums with payloads, generic types, field
//! defaults, `json.Json` values, and `Num` evidence.

use super::{int, int_lit, str_lit};
use proptest::prelude::*;
use sandlang::json::Json;
use sandlang::native::{Num, Param, Val, bound};
use sandlang::vm::{Ctl, Vm};
use sandlang::{Config, Engine, Error, MemorySink, NativeModule, Value};
use std::sync::{Arc, Mutex};

fn run(config: Config, src: &str) -> (Result<sandlang::Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("main.sand", src));
    (r, out.contents())
}

fn text() -> impl Strategy<Value = String> {
    "[a-z\"\\\n é\u{1}]{0,6}"
}

fn float() -> impl Strategy<Value = f64> {
    prop_oneof![(-400i32..400).prop_map(|q| f64::from(q) / 4.0), any::<f64>().prop_filter("finite", |f| f.is_finite())]
}

fn float_lit(f: f64) -> String {
    format!("({f:?})")
}

/// Has a variant of each runtime representation: a tag, inline payloads
/// (`Num`, and `Wrap` of a tag), and heap payloads.
#[derive(Clone, Debug, PartialEq, sandlang::SandEnum)]
#[sand(module = "fig")]
enum Fig {
    Empty,
    Num(i64),
    Wrap(Box<Fig>),
    Pair(String, Box<Fig>),
    Named { tag: Option<String>, parts: Vec<Fig> },
    Flags(bool, f64, ()),
}

fn fig() -> impl Strategy<Value = Fig> {
    let leaf = prop_oneof![Just(Fig::Empty), int().prop_map(Fig::Num), (any::<bool>(), float()).prop_map(|(b, f)| Fig::Flags(b, f, ())),];
    leaf.prop_recursive(5, 40, 4, |inner| {
        prop_oneof![
            inner.clone().prop_map(|f| Fig::Wrap(Box::new(f))),
            (text(), inner.clone()).prop_map(|(s, f)| Fig::Pair(s, Box::new(f))),
            (prop::option::of(text()), prop::collection::vec(inner, 0..4)).prop_map(|(tag, parts)| Fig::Named { tag, parts }),
        ]
    })
}

fn fig_lit(f: &Fig) -> String {
    match f {
        Fig::Empty => "fig.Fig.Empty".into(),
        Fig::Num(n) => format!("fig.Fig.Num({})", int_lit(*n)),
        Fig::Wrap(f) => format!("fig.Fig.Wrap({})", fig_lit(f)),
        Fig::Pair(s, f) => format!("fig.Fig.Pair({}, {})", str_lit(s), fig_lit(f)),
        Fig::Named { tag, parts } => {
            let tag = tag.as_ref().map_or("None".to_string(), |t| format!("Some({})", str_lit(t)));
            let parts: Vec<String> = parts.iter().map(fig_lit).collect();
            format!("fig.Fig.Named(tag: {tag}, parts: [{}])", parts.join(", "))
        }
        Fig::Flags(b, x, ()) => format!("fig.Fig.Flags({b}, {}, ())", float_lit(*x)),
    }
}

/// A generic struct whose fields use its parameters in every position.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "fig")]
struct Holder<A, B> {
    a: A,
    bs: Vec<B>,
    pair: Option<(A, B)>,
    #[sand(default)]
    extra: Vec<A>,
}

/// Field defaults of each kind of constant.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "fig")]
struct Defaults {
    #[sand(default = -7)]
    n: i64,
    #[sand(default = 2.5)]
    x: f64,
    #[sand(default = true)]
    b: bool,
    #[sand(default = "d\"q")]
    s: String,
    #[sand(default)]
    xs: Vec<String>,
    #[sand(default)]
    opt: Option<Fig>,
    #[sand(default = sandlang::jiff::SignedDuration::from_millis(1500))]
    d: sandlang::jiff::SignedDuration,
}

impl Default for Defaults {
    fn default() -> Defaults {
        Defaults {
            n: -7,
            x: 2.5,
            b: true,
            s: "d\"q".into(),
            xs: Vec::new(),
            opt: None,
            d: sandlang::jiff::SignedDuration::from_millis(1500),
        }
    }
}

fn fig_module(seen: Arc<Mutex<Vec<Fig>>>, made: Fig) -> NativeModule {
    let mut m = NativeModule::new("fig");
    m.bind::<Fig>().bind::<Holder<(), ()>>().bind::<Defaults>();
    m.function("echo", move |f: Fig| {
        seen.lock().unwrap().push(f.clone());
        f
    });
    m.function("make", move || made.clone());
    m
}

fn json() -> impl Strategy<Value = Json> {
    let leaf = prop_oneof![
        Just(Json::Null),
        any::<bool>().prop_map(Json::Bool),
        int().prop_map(Json::Int),
        float().prop_map(Json::Float),
        text().prop_map(Json::Str),
    ];
    leaf.prop_recursive(5, 48, 5, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Json::Arr),
            prop::collection::vec((text(), inner), 0..5).prop_map(|kvs| Json::Obj(kvs.into_iter().collect())),
        ]
    })
}

fn sum_module() -> NativeModule {
    let mut m = NativeModule::new("nums");
    m.vm_function("total", |vm: &mut Vm, num: Num<0>, xs: Val<Vec<Param<0, bound::Num>>>| -> Result<Param<0>, Ctl> {
        let mut acc = num.zero(vm)?.raw();
        for &x in xs.items(&vm.heap) {
            acc = match (acc, x) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(b)),
                (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
                _ => return Err(Ctl::internal("not a number")),
            };
        }
        Ok(Param::from_raw(acc))
    });
    m.source("pub native fn total<T: Num>(xs: [T]) -> T");
    m
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn enums_with_payloads_round_trip(value in fig(), gc_stress in any::<bool>()) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let m = fig_module(seen.clone(), value.clone());
        let src = format!(
            "import fig\nlet f = {}\nlet back = fig.echo(f)\nprint(back == f)\nprint(fig.make() == f)\nprint(\"\\(back)\" == \"\\(f)\")\n\
             print(fig.echo(fig.Fig.Wrap(f)) == fig.Fig.Wrap(f))",
            fig_lit(&value)
        );
        let (r, out) = run(Config::new().module(m).gc_stress(gc_stress), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        prop_assert_eq!(out, "true\ntrue\ntrue\ntrue\n", "{}", src);
        let wrapped = Fig::Wrap(Box::new(value.clone()));
        prop_assert_eq!(seen.lock().unwrap().clone(), vec![value, wrapped]);
    }

    #[test]
    fn generic_structs_round_trip(a in int(), bs in prop::collection::vec(text(), 0..4), pair in any::<bool>(), extra in prop::collection::vec(int(), 0..3), f in fig()) {
        let first = Holder { a, bs: bs.clone(), pair: pair.then(|| (a, bs.first().cloned().unwrap_or_default())), extra: extra.clone() };
        let second = Holder { a: f.clone(), bs: vec![a], pair: None, extra: Vec::new() };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = fig_module(Arc::default(), Fig::Empty);
        let s = seen.clone();
        m.function("echo_text", move |h: Holder<i64, String>| {
            s.lock().unwrap().push(h.clone());
            h
        });
        let made = second.clone();
        m.function("make_fig", move || made.clone());
        let bs_lit: Vec<String> = bs.iter().map(|b| str_lit(b)).collect();
        let pair_lit = match &first.pair {
            Some((a, b)) => format!("Some(({}, {}))", int_lit(*a), str_lit(b)),
            None => "None".into(),
        };
        let extra_lit: Vec<String> = extra.iter().map(|&x| int_lit(x)).collect();
        let extra_field = if extra.is_empty() { String::new() } else { format!(", extra: [{}]", extra_lit.join(", ")) };
        let src = format!(
            "import fig\nlet h = fig.Holder {{ a: {}, bs: [{}], pair: {pair_lit}{extra_field} }}\nprint(fig.echo_text(h) == h)\n\
             let g: fig.Holder<fig.Fig, int> = fig.make_fig()\nprint(g == fig.Holder {{ a: {}, bs: [{}], pair: None }})",
            int_lit(a), bs_lit.join(", "), fig_lit(&f), int_lit(a)
        );
        let (r, out) = run(Config::new().module(m), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        prop_assert_eq!(out, "true\ntrue\n", "{}", src);
        prop_assert_eq!(seen.lock().unwrap().clone(), vec![first]);
    }

    #[test]
    fn rust_field_defaults_fill_omitted_fields(given in prop::collection::vec(any::<bool>(), 7), n in int(), s in text()) {
        let full = Defaults { n, x: -1.25, b: false, s: s.clone(), xs: vec![s.clone()], opt: Some(Fig::Num(n)), d: sandlang::jiff::SignedDuration::from_secs(3) };
        let lits = [
            ("n", int_lit(n), format!("{n}")),
            ("x", "(-1.25)".to_string(), "-1.25".to_string()),
            ("b", "false".to_string(), "false".to_string()),
            ("s", str_lit(&s), serde_str(&s)),
            ("xs", format!("[{}]", str_lit(&s)), format!("[{}]", serde_str(&s))),
            ("opt", format!("Some(fig.Fig.Num({}))", int_lit(n)), format!("{{\"Num\": [{n}]}}")),
            ("d", "3s".to_string(), "3000000000".to_string()),
        ];
        let mut expected = Defaults::default();
        let fields: Vec<String> = lits.iter().zip(&given).filter(|(_, g)| **g).map(|((name, lit, _), _)| format!("{name}: {lit}")).collect();
        let keys: Vec<String> = lits.iter().zip(&given).filter(|(_, g)| **g).map(|((name, _, json), _)| format!("\"{name}\": {json}")).collect();
        for (i, &g) in given.iter().enumerate() {
            if g {
                match i {
                    0 => expected.n = full.n,
                    1 => expected.x = full.x,
                    2 => expected.b = full.b,
                    3 => expected.s = full.s.clone(),
                    4 => expected.xs = full.xs.clone(),
                    5 => expected.opt = full.opt.clone(),
                    _ => expected.d = full.d,
                }
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = fig_module(Arc::default(), Fig::Empty);
        let sn = seen.clone();
        m.function("keep", move |d: Defaults| sn.lock().unwrap().push(d));
        let src = format!(
            "import fig\nimport json\nfig.keep(fig.Defaults {{ {} }})\nlet d: fig.Defaults = json.decode({})?\nfig.keep(d)",
            fields.join(", "),
            str_lit(&format!("{{{}}}", keys.join(", ")))
        );
        let (r, _) = run(Config::new().module(m), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        prop_assert_eq!(seen.lock().unwrap().clone(), vec![expected.clone(), expected]);
    }

    #[test]
    fn rust_json_values_round_trip_through_text(value in json(), indent in prop_oneof![Just(0i64), 1i64..4]) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut m = NativeModule::new("doc");
        let s = seen.clone();
        m.function("keep", move |j: Json| s.lock().unwrap().push(j));
        let made = value.clone();
        m.function("make", move || made.clone());
        let src = format!(
            "import doc\nimport json\nlet j = doc.make()\nlet back = json.parse(json.encode(j, indent: {indent}))?\nprint(back == j)\n\
             print(json.to_json(j) == j)\ndoc.keep(back)\nlet d: json.Json = json.decode(json.encode(j))?\ndoc.keep(d)"
        );
        let (r, out) = run(Config::new().module(m), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        prop_assert_eq!(out, "true\ntrue\n");
        prop_assert_eq!(seen.lock().unwrap().clone(), vec![value.clone(), value]);
    }

    #[test]
    fn num_evidence_gives_the_identity_of_sums(ints in prop::collection::vec(-1000i64..1000, 0..10), floats in prop::collection::vec(-400i32..400, 0..10)) {
        let floats: Vec<f64> = floats.into_iter().map(|q| f64::from(q) / 4.0).collect();
        let int_lits: Vec<String> = ints.iter().map(|&i| int_lit(i)).collect();
        let float_lits: Vec<String> = floats.iter().map(|&f| float_lit(f)).collect();
        let src = format!(
            "import nums\nlet xs: [int] = [{}]\nlet ys: [float] = [{}]\nfn both<T: Num>(v: [T]) -> (T, T) {{\n    (nums.total(v), v.sum())\n}}\n\
             print(nums.total(xs))\nprint(xs.sum())\nprint(both(xs))\nprint(nums.total(ys))\nprint(ys.sum())\nprint(both(ys))\nprint(ys.iter().sum())",
            int_lits.join(", "),
            float_lits.join(", ")
        );
        let (r, out) = run(Config::new().module(sum_module()), &src);
        prop_assert!(r.is_ok(), "{r:?}\n{src}");
        let si: i64 = ints.iter().sum();
        let sf = floats.iter().fold(0.0, |a, b| a + b);
        prop_assert_eq!(out, format!("{si}\n{si}\n({si}, {si})\n{sf:?}\n{sf:?}\n({sf:?}, {sf:?})\n{sf:?}\n"), "{}", src);
    }
}

/// `s` as a JSON string.
fn serde_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
