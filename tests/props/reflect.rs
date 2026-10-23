//! Property tests for the type arguments of `json.decode`: rejected at
//! compile time unless `Encode`, and decoded with run-time errors only for
//! map keys that JSON object keys cannot hold; and the constant field defaults
//! that fill missing fields.

use super::{check, int, int_lit, str_lit};
use proptest::prelude::*;
use sandlang::{Config, Engine, Error, MemorySink, Policy};

#[derive(Clone, Debug)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Unit,
    Duration,
    Json,
    UnitEnum,
    Rec,
    Keyed,
    Fn,
    Task,
    Range,
    List(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Tuple(Vec<Ty>),
    Wrap(Box<Ty>),
}

impl Ty {
    fn sand(&self) -> String {
        match self {
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Unit => "()".into(),
            Ty::Duration => "duration".into(),
            Ty::Json => "json.Json".into(),
            Ty::UnitEnum => "Unit".into(),
            Ty::Rec => "Rec".into(),
            Ty::Keyed => "Keyed".into(),
            Ty::Fn => "(fn() -> int)".into(),
            Ty::Task => "Task<int>".into(),
            Ty::Range => "Range".into(),
            Ty::List(t) => format!("[{}]", t.sand()),
            Ty::Set(t) => format!("Set<{}>", t.sand()),
            Ty::Map(k, v) => format!("[{}: {}]", k.sand(), v.sand()),
            Ty::Option(t) => format!("{}?", t.sand()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::sand).collect::<Vec<_>>().join(", ")),
            Ty::Wrap(t) => format!("Wrap<{}>", t.sand()),
        }
    }

    /// Whether the type satisfies `Encode`, which `json.decode` requires.
    fn encodable(&self) -> bool {
        match self {
            Ty::Fn | Ty::Task => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) => t.encodable(),
            Ty::Map(k, v) => k.encodable() && v.encodable(),
            Ty::Tuple(ts) => ts.iter().all(Ty::encodable),
            _ => true,
        }
    }

    /// Whether every map key type in the type is one that JSON object keys
    /// hold (`Keyed` has a map with `Keyed` keys).
    fn keys_decodable(&self) -> bool {
        match self {
            Ty::Keyed => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) => t.keys_decodable(),
            Ty::Map(k, v) => matches!(**k, Ty::Int | Ty::Bool | Ty::Str | Ty::Duration | Ty::UnitEnum) && v.keys_decodable(),
            Ty::Tuple(ts) => ts.iter().all(Ty::keys_decodable),
            _ => true,
        }
    }
}

fn hashable() -> impl Strategy<Value = Ty> {
    let leaf =
        prop_oneof![Just(Ty::Int), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Bytes), Just(Ty::Unit), Just(Ty::Duration), Just(Ty::UnitEnum)];
    leaf.prop_recursive(2, 6, 2, |inner| {
        prop_oneof![inner.clone().prop_map(|t| Ty::Option(Box::new(t))), prop::collection::vec(inner, 2..3).prop_map(Ty::Tuple)]
    })
}

fn ty() -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![
        3 => prop_oneof![Just(Ty::Int), Just(Ty::Float), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Bytes), Just(Ty::Unit), Just(Ty::Duration)],
        2 => prop_oneof![Just(Ty::Json), Just(Ty::UnitEnum), Just(Ty::Rec)],
        1 => prop_oneof![Just(Ty::Fn), Just(Ty::Task), Just(Ty::Range), Just(Ty::Keyed)],
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| Ty::List(Box::new(t))),
            hashable().prop_map(|t| Ty::Set(Box::new(t))),
            (hashable(), inner.clone()).prop_map(|(k, v)| Ty::Map(Box::new(k), Box::new(v))),
            inner.clone().prop_map(|t| Ty::Option(Box::new(t))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
            inner.prop_map(|t| Ty::Wrap(Box::new(t))),
        ]
    })
}

const INPUTS: [&str; 14] = [
    "null",
    "1",
    "1.5",
    "true",
    "\"A\"",
    "\"x\"",
    "[]",
    "[1, 2]",
    "[[1]]",
    "{}",
    "{\"a\": 1, \"b\": null}",
    "{\"v\": [], \"A\": 1}",
    "{\"start\": 1, \"end\": 2}",
    "{\"m\": {\"k\": 1}}",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn json_decode_accepts_exactly_the_encodable_types(t in ty()) {
        let inputs: Vec<String> = INPUTS.iter().map(|s| format!("{s:?}")).collect();
        let src = format!(
            "import json\nenum Unit {{ A, B }}\nstruct Rec {{ a: int, b: str? }}\nstruct Keyed {{ m: [Keyed: int]? }}\nstruct Wrap<T> {{ v: T }}\n\
             fn read(s: str) -> Result<{}> {{ json.decode(s) }}\n\
             for s in [{}] {{\n    match read(s) {{\n        Ok(_) => print(\"ok\")\n        Err(e) => print(e.kind)\n    }}\n}}",
            t.sand(),
            inputs.join(", ")
        );
        let out = MemorySink::new();
        let mut engine = Engine::new(Config::new().policy(Policy::deny_all()).stdout(out.clone())).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        match rt.block_on(engine.run_source("main.sand", &src)) {
            Ok(_) => {
                prop_assert!(t.encodable(), "accepted:\n{src}");
                let kinds = out.contents();
                let bad_key = !t.keys_decodable();
                prop_assert!(kinds.lines().all(|k| k == "ok" || k == "parse" || (bad_key && k == "invalid")), "{kinds}\n{src}");
            }
            Err(Error::Compile(ds)) => {
                prop_assert!(!t.encodable(), "rejected: {ds:?}\n{src}");
                prop_assert!(ds.iter().all(|d| d.message.contains("does not satisfy Encode")), "{ds:?}\n{src}");
                prop_assert!(ds.iter().all(|d| !d.message.contains("``")), "{ds:?}\n{src}");
            }
            Err(e) => prop_assert!(false, "{e}\n{src}"),
        }
    }
}

/// A field type and a constant default expression of that type, built from
/// literals and constructors.
fn constant_default() -> impl Strategy<Value = (String, String)> {
    let finite = any::<f64>().prop_filter("finite", |f| f.is_finite());
    let leaf = prop_oneof![
        int().prop_filter("a literal", |&i| i != i64::MIN).prop_map(|i| ("int".to_string(), int_lit(i))),
        finite.prop_map(|f| ("float".to_string(), if f.is_sign_negative() { format!("(-{:?})", -f) } else { format!("{f:?}") })),
        (-1000i64..1000).prop_map(|n| ("duration".to_string(), if n < 0 { format!("-{}ms", -n) } else { format!("{n}ms") })),
        "[a-z\"\\\n é]{0,4}".prop_map(|t| ("str".to_string(), str_lit(&t))),
        prop::collection::vec(any::<u8>(), 0..4)
            .prop_map(|b| ("bytes".to_string(), format!("b\"{}\"", b.iter().map(|x| format!("\\x{x:02x}")).collect::<String>()))),
        any::<bool>().prop_map(|b| ("bool".to_string(), b.to_string())),
        Just(("()".to_string(), "()".to_string())),
        Just(("[int]".to_string(), "[]".to_string())),
        Just(("[str: int]".to_string(), "[:]".to_string())),
        Just(("int?".to_string(), "None".to_string())),
        Just(("Tree<int>".to_string(), "Tree.Empty".to_string())),
        (-3i64..3, -3i64..3, any::<bool>()).prop_map(|(a, b, inclusive)| {
            ("Range".to_string(), format!("{}..{}{}", int_lit(a), if inclusive { "=" } else { "" }, int_lit(b)))
        }),
        prop::sample::select(vec!["Mode.Read", "Mode.Write"]).prop_map(|v| ("Mode".to_string(), v.to_string())),
    ];
    // Negating twice keeps a number constant.
    let leaf = (leaf, any::<bool>()).prop_map(|((ty, e), twice)| match ty.as_str() {
        "int" | "float" | "duration" if twice => (ty, format!("-(-{e})")),
        _ => (ty, e),
    });
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|(ty, e)| (format!("{ty}?"), format!("Some({e})"))),
            (inner.clone(), inner.clone()).prop_map(|((t1, e1), (t2, e2))| (format!("({t1}, {t2})"), format!("({e1}, {e2})"))),
            (inner.clone(), 1..3usize).prop_map(|((ty, e), n)| (format!("[{ty}]"), format!("[{}]", vec![e; n].join(", ")))),
            inner.clone().prop_map(|(ty, e)| (format!("[str: {ty}]"), format!("[\"a\": {e}, \"b\": {e}]"))),
            inner.clone().prop_map(|(ty, e)| (format!("Tree<{ty}>"), format!("Tree.Leaf({e})"))),
            inner.clone().prop_map(|(ty, e)| (format!("Tree<{ty}>"), format!("Tree.Pair(a: {e}, b: {e})"))),
            (inner, any::<bool>()).prop_map(|((ty, e), w)| {
                let w = if w { ", w: -1" } else { "" };
                (format!("Holder<{ty}>"), format!("Holder {{ v: {e}{w} }}"))
            }),
        ]
    })
}

/// The declarations that the types of `constant_default` refer to.
const CONSTANT_TYPES: &str = "enum Mode { Read, Write }\nenum Tree<T> { Leaf(T), Pair(a: T, b: T), Empty }\n\
                              struct Holder<T> { v: T, w: int = 7, tags: [str] = [\"t\"] }\nfn id<T>(x: T) -> T { x }";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn json_decode_fills_missing_fields_with_constant_defaults(fields in prop::collection::vec(constant_default(), 1..6)) {
        let decls: Vec<String> = fields.iter().enumerate().map(|(i, (ty, e))| format!("    f{i}: {ty} = {e}")).collect();
        let src = format!(
            "import json\n{CONSTANT_TYPES}\nstruct S {{\n{}\n}}\nlet s: S = json.decode(\"{{}}\")?\nprint(s == S {{}})\nprint(json.encode(s) == json.encode(S {{}}))",
            decls.join("\n")
        );
        check(&src, "true\ntrue\n", None)?;
    }

    #[test]
    fn json_decode_runs_computed_defaults_as_a_struct_literal_does(fields in prop::collection::vec((constant_default(), any::<bool>()), 1..6)) {
        // A call makes a default computed, even if the value it gives is
        // constant; `note` records the order of the calls. Encodings are
        // compared, as `Some(())` and `None` both encode as `null`.
        let decls: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, ((ty, e), computed))| match computed {
                true => format!("    f{i}: {ty}? = Some(note({i}, {e}))"),
                false => format!("    f{i}: {ty} = {e}"),
            })
            .collect();
        let src = format!(
            "import json\n{CONSTANT_TYPES}\nvar calls: [int] = []\nfn note<T>(i: int, x: T) -> T {{\n    calls.push(i)\n    x\n}}\n\
             struct S {{\n{}\n}}\nlet s: S = json.decode(\"{{}}\")?\nlet decoded = calls\ncalls = []\nlet literal = S {{}}\n\
             print(s == literal)\nprint(decoded == calls)\ncalls = []\nlet full: S = json.decode(json.encode(literal))?\nprint(json.encode(full) == json.encode(literal))\nprint(calls)\nprint(decoded)",
            decls.join("\n")
        );
        let order: Vec<String> = fields.iter().enumerate().filter(|(_, (_, computed))| *computed).map(|(i, _)| i.to_string()).collect();
        check(&src, &format!("true\ntrue\ntrue\n[]\n[{}]\n", order.join(", ")), None)?;
    }
}
