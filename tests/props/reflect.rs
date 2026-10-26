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
    /// `struct Valid { a: int }` with an `init` method (which does not implement `Decode`).
    Valid,
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
    /// `struct Bag<T> { s: Set<T> }`, which requires `T: Hash`.
    Bag(Box<Ty>),
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
            Ty::Valid => "Valid".into(),
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
            Ty::Bag(t) => format!("Bag<{}>", t.sand()),
            Ty::Param => "T".into(),
        }
    }

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
            Ty::Bag(t) => Ty::Bag(b(t)),
            t => t.clone(),
        }
    }

    fn has_param(&self) -> bool {
        match self {
            Ty::Param => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) | Ty::Bag(t) => t.has_param(),
            Ty::Map(k, v) => k.has_param() || v.has_param(),
            Ty::Tuple(ts) => ts.iter().any(Ty::has_param),
            _ => false,
        }
    }

    fn decodable(&self) -> bool {
        match self {
            Ty::Fn | Ty::Task | Ty::Valid => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) | Ty::Bag(t) => t.decodable(),
            Ty::Map(k, v) => k.decodable() && v.decodable(),
            Ty::Tuple(ts) => ts.iter().all(Ty::decodable),
            _ => true,
        }
    }

    /// Returns `true` if the type implements `Hash`.
    fn hashable(&self) -> bool {
        match self {
            Ty::Int | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Unit | Ty::Duration | Ty::UnitEnum | Ty::Rec | Ty::Valid | Ty::Range => true,
            Ty::Option(t) | Ty::Wrap(t) => t.hashable(),
            Ty::Tuple(ts) => ts.iter().all(Ty::hashable),
            _ => false,
        }
    }

    /// Returns `true` if all set elements and map keys in the type implement `Hash`.
    fn well_formed(&self) -> bool {
        match self {
            Ty::Set(t) | Ty::Bag(t) => t.hashable() && t.well_formed(),
            Ty::Map(k, v) => k.hashable() && k.well_formed() && v.well_formed(),
            Ty::List(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) => t.well_formed(),
            Ty::Tuple(ts) => ts.iter().all(Ty::well_formed),
            _ => true,
        }
    }

    fn keys_decodable(&self) -> bool {
        match self {
            Ty::Keyed => false,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Tree(t) | Ty::Bag(t) => t.keys_decodable(),
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

/// Generates types with type parameter `T` weighted by `param`, and non-`Hash`
/// or non-`Decode` types weighted by `bad`.
fn ty_with(param: u32, bad: u32) -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![
        6 => prop_oneof![Just(Ty::Int), Just(Ty::Float), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Bytes), Just(Ty::Unit), Just(Ty::Duration)],
        4 => prop_oneof![Just(Ty::Json), Just(Ty::UnitEnum), Just(Ty::Rec)],
        1 => prop_oneof![Just(Ty::Range), Just(Ty::Keyed)],
        bad => prop_oneof![Just(Ty::Fn), Just(Ty::Task), Just(Ty::Valid)],
        param => Just(Ty::Param),
    ];
    leaf.prop_recursive(3, 12, 3, move |inner| {
        let key = || prop_oneof![4 => hashable(), bad => inner.clone()];
        prop_oneof![
            2 => inner.clone().prop_map(|t| Ty::List(Box::new(t))),
            2 => key().prop_map(|t| Ty::Set(Box::new(t))),
            2 => (key(), inner.clone()).prop_map(|(k, v)| Ty::Map(Box::new(k), Box::new(v))),
            2 => inner.clone().prop_map(|t| Ty::Option(Box::new(t))),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
            2 => inner.clone().prop_map(|t| Ty::Wrap(Box::new(t))),
            2 => inner.clone().prop_map(|t| Ty::Tree(Box::new(t))),
            2 * bad => inner.prop_map(|t| Ty::Bag(Box::new(t))),
        ]
    })
}

const TYPES: &str = "enum Unit { A, B }\nstruct Rec { a: int, b: str? }\nstruct Key { a: int }\nstruct Keyed { m: [Key: int]? }\nstruct Wrap<T> { v: T }\n\
                     struct Tree<T> { v: T, kids: [Tree<T>] = [] }\nstruct Bag<T> { s: Set<T> }\n\
                     struct Valid {\n    a: int\n    fn init(a: int) -> Valid { Self(a) }\n}";

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
    fn json_decode_accepts_exactly_the_decodable_types(t in ty()) {
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
                prop_assert!(t.decodable() && t.well_formed(), "accepted:\n{src}");
                let kinds = out.contents();
                let bad_key = !t.keys_decodable();
                prop_assert!(kinds.lines().all(|k| k == "ok" || k == "parse" || (bad_key && k == "invalid")), "{kinds}\n{src}");
            }
            Err(Error::Compile(ds)) => {
                prop_assert!(!(t.decodable() && t.well_formed()), "rejected: {ds:?}\n{src}");
                let trait_of = |d: &sandlang::Diagnostic| {
                    if d.message.contains("does not implement `Decode`") { Some(true) }
                    else if d.message.contains("does not implement `Hash`") { Some(false) }
                    else { None }
                };
                prop_assert!(ds.iter().all(|d| trait_of(d).is_some()), "{ds:?}\n{src}");
                prop_assert_eq!(ds.iter().any(|d| trait_of(d) == Some(true)), !t.decodable(), "{:?}\n{}", ds, src);
                prop_assert_eq!(ds.iter().any(|d| trait_of(d) == Some(false)), !t.well_formed(), "{:?}\n{}", ds, src);
                prop_assert!(ds.iter().all(|d| !d.message.contains("``")), "{ds:?}\n{src}");
            }
            Err(e) => prop_assert!(false, "{e}\n{src}"),
        }
    }
}

fn context() -> impl Strategy<Value = Ty> {
    ty_with(10, 0).prop_map(|t| if t.has_param() { t } else { Ty::Tuple(vec![t, Ty::Param]) })
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

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
        Ty::Valid => "{\"a\": 1}".into(),
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
        Ty::Bag(e) => format!("{{\"s\": [{}]}}", many(e, rng).join(", ")),
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
             fn via<reflect T: Decode>(s: str) -> Result<{}> {{ json.decode(s) }}\n\
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
            ("Set<int>".to_string(), if items.is_empty() { "Set()".to_string() } else { format!("Set([{}])", items.join(", ")) })
        }),
        Just(("int?".to_string(), "None".to_string())),
        Just(("Tree<int>".to_string(), "Tree.Empty".to_string())),
        (-3i64..3, -3i64..3, any::<bool>()).prop_map(|(a, b, inclusive)| {
            ("Range".to_string(), format!("{}..{}{}", int_lit(a), if inclusive { "=" } else { "" }, int_lit(b)))
        }),
        prop::sample::select(vec!["Mode.Read", "Mode.Write"]).prop_map(|v| ("Mode".to_string(), v.to_string())),
    ];
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
                (format!("Holder<{ty}>"), format!("Holder(v: {e}{w})"))
            }),
        ]
    })
}

const CONSTANT_TYPES: &str = "enum Mode { Read, Write }\nenum Tree<T> { Leaf(T), Pair(a: T, b: T), Empty }\n\
                              struct Holder<T> { v: T, w: int = 7, tags: [str] = [\"t\"] }\nfn id<T>(x: T) -> T { x }";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn json_decode_fills_missing_fields_with_constant_defaults(fields in prop::collection::vec(constant_default(), 1..6)) {
        let decls: Vec<String> = fields.iter().enumerate().map(|(i, (ty, e))| format!("    f{i}: {ty} = {e}")).collect();
        let src = format!(
            "import json\n{CONSTANT_TYPES}\nstruct S {{\n{}\n}}\nlet s: S = json.decode(\"{{}}\")?\nprint(s == S())\nprint(json.encode(s) == json.encode(S()))",
            decls.join("\n")
        );
        check(&src, "true\ntrue\n", None)?;
    }

    #[test]
    fn json_decode_runs_computed_defaults_as_a_construction_does(fields in prop::collection::vec((constant_default(), any::<bool>()), 1..6)) {
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
             struct S {{\n{}\n}}\nlet s: S = json.decode(\"{{}}\")?\nlet decoded = calls\ncalls = []\nlet literal = S()\n\
             print(s == literal)\nprint(decoded == calls)\ncalls = []\nlet full: S = json.decode(json.encode(literal))?\nprint(json.encode(full) == json.encode(literal))\nprint(calls)\nprint(decoded)",
            decls.join("\n")
        );
        let order: Vec<String> = fields.iter().enumerate().filter(|(_, (_, computed))| *computed).map(|(i, _)| i.to_string()).collect();
        check(&src, &format!("true\ntrue\ntrue\n[]\n[{}]\n", order.join(", ")), None)?;
    }
}

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
        let mut lets = vec![format!("let a0 = [{}]", vec!["0"; width].join(", "))];
        let mut ty = "[int]".to_string();
        for i in 1..depth {
            lets.push(format!("let a{i} = [{}]", vec![format!("a{}", i - 1); width].join(", ")));
            ty = format!("[{ty}]");
        }
        let mut src = format!(
            "import json\nstruct S0 {{\n    list: {ty} = {{\n        {}\n        a{}\n    }}\n    next: S1 = S1()\n}}\n",
            lets.join("\n        "),
            depth - 1
        );
        for i in 1..chain {
            src += &format!("struct S{i} {{ next: S{} = S{}() }}\n", i + 1, i + 1);
        }
        src += &format!("struct S{chain} {{ v: int = 1 }}\nlet s: S0 = json.decode(\"{{}}\")?\nprint(s == S0())\nprint(json.encode(s) == json.encode(S0()))\n");
        check(&src, "true\ntrue\n", None)?;
    }
}
