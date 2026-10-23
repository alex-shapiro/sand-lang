//! Property tests for the type arguments of `json.decode`: rejected at
//! compile time unless `Encode`, decoded with run-time errors only for map
//! keys that JSON object keys cannot hold, and decoded the same through a
//! function that builds them from its `reflect` type parameter; and the
//! constant field defaults that fill missing fields.

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
    Tree(Box<Ty>),
    /// The type parameter `T` of a function.
    Param,
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
            Ty::Tree(t) => format!("Tree<{}>", t.sand()),
            Ty::Param => "T".into(),
        }
    }

    /// `self` with `arg` for `Ty::Param`.
    fn subst(&self, arg: &Ty) -> Ty {
        let b = |t: &Ty| Box::new(t.subst(arg));
        match self {
            Ty::Param => arg.clone(),
            Ty::List(t) => Ty::List(b(t)),
            Ty::Set(t) => Ty::Set(b(t)),
            Ty::Map(k, v) => Ty::Map(b(k), b(v)),
            Ty::Option(t) => Ty::Option(b(t)),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| t.subst(arg)).collect()),
            Ty::Wrap(t) => Ty::Wrap(b(t)),
            Ty::Tree(t) => Ty::Tree(b(t)),
            t => t.clone(),
        }
    }

    fn has_param(&self) -> bool {
        match self {
            Ty::Param => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) => t.has_param(),
            Ty::Map(k, v) => k.has_param() || v.has_param(),
            Ty::Tuple(ts) => ts.iter().any(Ty::has_param),
            _ => false,
        }
    }

    /// Whether the type satisfies `Encode`, which `json.decode` requires.
    fn encodable(&self) -> bool {
        match self {
            Ty::Fn | Ty::Task => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) => t.encodable(),
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
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) => t.keys_decodable(),
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
    ty_with(0, 1)
}

/// Types in which `Ty::Param` and the types that are not `Encode` have the
/// weights `param` and `bad` among the leaves.
fn ty_with(param: u32, bad: u32) -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![
        6 => prop_oneof![Just(Ty::Int), Just(Ty::Float), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Bytes), Just(Ty::Unit), Just(Ty::Duration)],
        4 => prop_oneof![Just(Ty::Json), Just(Ty::UnitEnum), Just(Ty::Rec)],
        1 => prop_oneof![Just(Ty::Range), Just(Ty::Keyed)],
        bad => prop_oneof![Just(Ty::Fn), Just(Ty::Task)],
        param => Just(Ty::Param),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| Ty::List(Box::new(t))),
            hashable().prop_map(|t| Ty::Set(Box::new(t))),
            (hashable(), inner.clone()).prop_map(|(k, v)| Ty::Map(Box::new(k), Box::new(v))),
            inner.clone().prop_map(|t| Ty::Option(Box::new(t))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
            inner.clone().prop_map(|t| Ty::Wrap(Box::new(t))),
            inner.prop_map(|t| Ty::Tree(Box::new(t))),
        ]
    })
}

/// The declarations that the types of `ty` refer to.
const TYPES: &str = "enum Unit { A, B }\nstruct Rec { a: int, b: str? }\nstruct Keyed { m: [Keyed: int]? }\nstruct Wrap<T> { v: T }\n\
                     struct Tree<T> { v: T, kids: [Tree<T>] = [] }";

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
            "import json\n{TYPES}\n\
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

/// Types that contain `Ty::Param`.
fn context() -> impl Strategy<Value = Ty> {
    ty_with(10, 0).prop_map(|t| if t.has_param() { t } else { Ty::Tuple(vec![t, Ty::Param]) })
}

/// Deterministic choices for [`json_text`].
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A JSON text for a value of type `t`, except that one in `1 / wrong` of
/// the values in it is of another type.
fn json_text(t: &Ty, rng: &mut Rng, depth: u32, wrong: u64) -> String {
    if rng.below(wrong) == 0 {
        return ["7", "\"zzz\"", "null", "[]", "{}"][rng.below(5) as usize].into();
    }
    let many = |t: &Ty, rng: &mut Rng| {
        let n = if depth > 3 { 0 } else { rng.below(3) };
        (0..n).map(|_| json_text(t, rng, depth + 1, wrong)).collect::<Vec<_>>()
    };
    match t {
        Ty::Int | Ty::Duration => (rng.below(5) as i64 - 2).to_string(),
        Ty::Float => ["1.5", "-2", "0.25"][rng.below(3) as usize].into(),
        Ty::Bool => ["true", "false"][rng.below(2) as usize].into(),
        Ty::Str => ["\"a\"", "\"b\"", "\"\""][rng.below(3) as usize].into(),
        Ty::Bytes => ["\"AQI=\"", "\"\""][rng.below(2) as usize].into(),
        Ty::Unit => "null".into(),
        Ty::Json => ["{\"a\": [1, null]}", "2", "\"s\""][rng.below(3) as usize].into(),
        Ty::UnitEnum => ["\"A\"", "\"B\""][rng.below(2) as usize].into(),
        Ty::Rec => ["{\"a\": 1, \"b\": \"x\"}", "{\"a\": 2}"][rng.below(2) as usize].into(),
        Ty::Keyed => ["{}", "{\"m\": null}"][rng.below(2) as usize].into(),
        Ty::Range => ["{\"start\": 1, \"end\": 3}", "{\"start\": 0, \"end\": 0, \"inclusive\": true}"][rng.below(2) as usize].into(),
        Ty::Fn | Ty::Task | Ty::Param => "null".into(),
        Ty::List(e) | Ty::Set(e) => format!("[{}]", many(e, rng).join(", ")),
        Ty::Map(k, v) => {
            let key = match **k {
                Ty::Int | Ty::Duration => "\"1\"",
                Ty::Bool => "\"true\"",
                Ty::UnitEnum => "\"A\"",
                _ => "\"k\"",
            };
            let entries = many(v, rng).into_iter().map(|v| format!("{key}: {v}")).collect::<Vec<_>>();
            format!("{{{}}}", entries.join(", "))
        }
        Ty::Option(e) => {
            if rng.below(3) == 0 {
                "null".into()
            } else {
                json_text(e, rng, depth + 1, wrong)
            }
        }
        Ty::Tuple(ts) => format!("[{}]", ts.iter().map(|t| json_text(t, rng, depth + 1, wrong)).collect::<Vec<_>>().join(", ")),
        Ty::Wrap(e) => format!("{{\"v\": {}}}", json_text(e, rng, depth + 1, wrong)),
        Ty::Tree(e) => {
            let kids = many(t, rng);
            let v = json_text(e, rng, depth + 1, wrong);
            if kids.is_empty() && rng.below(2) == 0 {
                format!("{{\"v\": {v}}}")
            } else {
                format!("{{\"v\": {v}, \"kids\": [{}]}}", kids.join(", "))
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn reflect_type_parameters_build_descriptions_of_types_that_contain_them(ctx in context(), arg in ty_with(0, 0), seed in 1..u64::MAX) {
        let full = ctx.subst(&arg);
        let mut rng = Rng(seed);
        let texts: Vec<String> = (0..8).map(|i| format!("{:?}", json_text(&full, &mut rng, 0, [1000, 12][i % 2]))).collect();
        let src = format!(
            "import json\n{TYPES}\n\
             fn direct(s: str) -> Result<{}> {{ json.decode(s) }}\n\
             fn via<reflect T: Encode>(s: str) -> Result<{}> {{ json.decode(s) }}\n\
             fn read(s: str) -> Result<{}> {{ via(s) }}\n\
             for s in [{}] {{\n    print(direct(s))\n    print(read(s))\n}}",
            full.sand(),
            ctx.sand(),
            full.sand(),
            texts.join(", ")
        );
        let r = super::run(&src)?;
        prop_assert!(r.failure.is_none(), "{:?}\n{}", r.failure, src);
        let lines: Vec<&str> = r.stdout.lines().collect();
        for pair in lines.chunks(2) {
            prop_assert_eq!(pair[0], pair[1], "{}", src);
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
        prop::collection::vec(-3i64..3, 0..3).prop_map(|xs| {
            let items: Vec<String> = xs.iter().map(|&x| int_lit(x)).collect();
            ("Set<int>".to_string(), if items.is_empty() { "set()".to_string() } else { format!("set([{}])", items.join(", ")) })
        }),
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

/// The width and depth of a list default whose parts stay below 2^18, so
/// that some exceed the limits of constants and some do not.
fn list_shape() -> impl Strategy<Value = (usize, usize)> {
    (1usize..4).prop_flat_map(|width| {
        let max_depth = match width {
            1 => 80usize,
            2 => 18,
            _ => 11,
        };
        (Just(width), 1..=max_depth)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn json_decode_fills_defaults_beyond_the_limits_of_constants((width, depth) in list_shape(), chain in 1usize..80) {
        // Locals repeat a list `width` times at each of `depth` levels, and
        // each struct of the chain nests the default of the next one level
        // deeper: defaults within the limits of constants are built
        // directly, the others run, with the same value.
        let mut lets = vec![format!("let a0 = [{}]", vec!["0"; width].join(", "))];
        let mut ty = "[int]".to_string();
        for i in 1..depth {
            lets.push(format!("let a{i} = [{}]", vec![format!("a{}", i - 1); width].join(", ")));
            ty = format!("[{ty}]");
        }
        let mut src = format!(
            "import json\nstruct S0 {{\n    list: {ty} = {{\n        {}\n        a{}\n    }}\n    next: S1 = S1 {{}}\n}}\n",
            lets.join("\n        "),
            depth - 1
        );
        for i in 1..chain {
            src += &format!("struct S{i} {{ next: S{} = S{} {{}} }}\n", i + 1, i + 1);
        }
        src += &format!("struct S{chain} {{ v: int = 1 }}\nlet s: S0 = json.decode(\"{{}}\")?\nprint(s == S0 {{}})\nprint(json.encode(s) == json.encode(S0 {{}}))\n");
        check(&src, "true\ntrue\n", None)?;
    }
}
