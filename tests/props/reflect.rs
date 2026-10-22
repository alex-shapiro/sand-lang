//! Property tests for the compile-time check of `json.decode` type arguments.

use proptest::prelude::*;
use sandlang::{Config, Engine, Error, MemorySink, Policy};

/// A Sand type, for `json.decode<T>`.
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
    /// `enum Unit { A, B }`
    UnitEnum,
    /// `struct Rec { a: int, b: str? }`
    Rec,
    /// `struct Keyed { m: [Keyed: int]? }`, a struct that is its own map key
    Keyed,
    /// `fn() -> int`
    Fn,
    Task,
    Range,
    List(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Tuple(Vec<Ty>),
    /// `struct Wrap<T> { v: T }`
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

    /// The reference model: whether `json.decode` can decode values of the type.
    fn decodable(&self) -> bool {
        match self {
            Ty::Fn | Ty::Task | Ty::Range | Ty::Keyed => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) => t.decodable(),
            Ty::Map(k, v) => matches!(**k, Ty::Int | Ty::Bool | Ty::Str | Ty::Duration | Ty::UnitEnum) && v.decodable(),
            Ty::Tuple(ts) => ts.iter().all(Ty::decodable),
            _ => true,
        }
    }
}

/// Types that satisfy `Hash`, for map keys and set elements.
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

const INPUTS: [&str; 12] =
    ["null", "1", "1.5", "true", "\"A\"", "\"x\"", "[]", "[1, 2]", "[[1]]", "{}", "{\"a\": 1, \"b\": null}", "{\"v\": [], \"A\": 1}"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The compiler accepts `json.decode<T>` exactly when `T` is decodable,
    /// and a call it accepts never fails at run time for an unsupported type.
    #[test]
    fn json_decode_accepts_exactly_the_decodable_types(t in ty()) {
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
                prop_assert!(t.decodable(), "accepted:\n{src}");
                let kinds = out.contents();
                prop_assert!(kinds.lines().all(|k| k == "ok" || k == "parse"), "{kinds}\n{src}");
            }
            Err(Error::Compile(ds)) => {
                prop_assert!(!t.decodable(), "rejected: {ds:?}\n{src}");
                prop_assert!(ds.iter().all(|d| d.message.starts_with("`json.decode` cannot decode")), "{ds:?}\n{src}");
                // Each message names the type it rejects.
                prop_assert!(ds.iter().all(|d| !d.message.contains("``")), "{ds:?}\n{src}");
            }
            Err(e) => prop_assert!(false, "{e}\n{src}"),
        }
    }
}
