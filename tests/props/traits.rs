//! The built-in traits against a reference model: which types implement
//! `Eq`, `Ord`, `Hash` and `Encode`, and which bounds imply which traits.

use proptest::prelude::*;
use sandlang::{Config, Engine, Error, MemorySink, Policy};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tr {
    Eq,
    Ord,
    Hash,
    Encode,
}

impl Tr {
    const ALL: [Tr; 4] = [Tr::Eq, Tr::Ord, Tr::Hash, Tr::Encode];

    fn name(self) -> &'static str {
        match self {
            Tr::Eq => "Eq",
            Tr::Ord => "Ord",
            Tr::Hash => "Hash",
            Tr::Encode => "Encode",
        }
    }

    /// An expression that uses `x` through a function bounded by the trait
    /// and through the operation that needs it, and what it prints.
    fn probe(self) -> (&'static str, &'static str) {
        match self {
            Tr::Eq => ("need(x) == x", "true"),
            Tr::Ord => ("need(x) <= x", "true"),
            Tr::Hash => ("set([need(x), x]).len()", "1"),
            Tr::Encode => ("json.encode(need(x)).len() > 0", "true"),
        }
    }
}

#[derive(Clone, Debug)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Unit,
    Duration,
    Range,
    Fn,
    Task,
    Plain,
    Mut,
    Fns,
    List(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Tuple(Vec<Ty>),
    Wrap(Box<Ty>),
    Opt(Box<Ty>),
    Tree(Box<Ty>),
    Phantom(Box<Ty>),
}

const TYPES: &str = "struct Plain { a: int, b: str }\nstruct Mut { var n: int }\nstruct Fns { f: fn() -> int }\n\
                     struct Wrap<T> { v: T }\nenum Opt<T> { No, Has(T) }\nstruct Tree<T> { v: T, kids: [Tree<T>] = [] }\n\
                     struct Phantom<T> { n: int }";

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
            Ty::Range => "Range".into(),
            Ty::Fn => "(fn() -> int)".into(),
            Ty::Task => "Task<int>".into(),
            Ty::Plain => "Plain".into(),
            Ty::Mut => "Mut".into(),
            Ty::Fns => "Fns".into(),
            Ty::List(t) => format!("[{}]", t.sand()),
            Ty::Set(t) => format!("Set<{}>", t.sand()),
            Ty::Map(k, v) => format!("[{}: {}]", k.sand(), v.sand()),
            Ty::Option(t) => format!("{}?", t.sand()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::sand).collect::<Vec<_>>().join(", ")),
            Ty::Wrap(t) => format!("Wrap<{}>", t.sand()),
            Ty::Opt(t) => format!("Opt<{}>", t.sand()),
            Ty::Tree(t) => format!("Tree<{}>", t.sand()),
            Ty::Phantom(t) => format!("Phantom<{}>", t.sand()),
        }
    }

    /// An expression of this type.
    fn value(&self) -> String {
        match self {
            Ty::Int => "1".into(),
            Ty::Float => "1.5".into(),
            Ty::Bool => "true".into(),
            Ty::Str => "\"s\"".into(),
            Ty::Bytes => "b\"b\"".into(),
            Ty::Unit => "()".into(),
            Ty::Duration => "1s".into(),
            Ty::Range => "0..2".into(),
            Ty::Fn => "() => 1".into(),
            Ty::Task => "spawn 1".into(),
            Ty::Plain => "Plain { a: 1, b: \"x\" }".into(),
            Ty::Mut => "Mut { n: 1 }".into(),
            Ty::Fns => "Fns { f: () => 1 }".into(),
            Ty::List(t) => format!("[{}]", t.value()),
            Ty::Set(t) => format!("set([{}])", t.value()),
            Ty::Map(k, v) => format!("[{}: {}]", k.value(), v.value()),
            Ty::Option(t) => format!("Some({})", t.value()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::value).collect::<Vec<_>>().join(", ")),
            Ty::Wrap(t) => format!("Wrap {{ v: {} }}", t.value()),
            Ty::Opt(t) => format!("Opt.Has({})", t.value()),
            Ty::Tree(t) => format!("Tree {{ v: {} }}", t.value()),
            Ty::Phantom(_) => "Phantom { n: 1 }".into(),
        }
    }

    /// The reference model of the derived impls.
    fn implements(&self, tr: Tr) -> bool {
        use Tr::*;
        match self {
            Ty::Int | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Unit | Ty::Duration | Ty::Plain => true,
            Ty::Float => tr != Hash,
            Ty::Range => tr != Ord,
            Ty::Fn | Ty::Fns => false,
            Ty::Task => tr == Eq,
            Ty::Mut => tr != Hash,
            Ty::List(t) => tr != Hash && t.implements(tr),
            Ty::Set(t) => matches!(tr, Eq | Encode) && t.implements(tr),
            Ty::Map(k, v) => matches!(tr, Eq | Encode) && k.implements(tr) && v.implements(tr),
            Ty::Option(t) | Ty::Wrap(t) | Ty::Opt(t) => t.implements(tr),
            Ty::Tuple(ts) => ts.iter().all(|t| t.implements(tr)),
            Ty::Tree(t) => tr != Hash && t.implements(tr),
            Ty::Phantom(_) => true,
        }
    }

    /// Whether `json.encode` accepts the map keys in a value of this type at
    /// run time.
    fn json_keys(&self) -> bool {
        match self {
            Ty::Map(k, v) => matches!(**k, Ty::Int | Ty::Bool | Ty::Str | Ty::Duration) && v.json_keys(),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Wrap(t) | Ty::Opt(t) | Ty::Tree(t) => t.json_keys(),
            Ty::Tuple(ts) => ts.iter().all(Ty::json_keys),
            _ => true,
        }
    }
}

fn hashable() -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![Just(Ty::Int), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Range), Just(Ty::Plain)];
    leaf.prop_recursive(2, 4, 2, |inner| {
        prop_oneof![inner.clone().prop_map(|t| Ty::Option(Box::new(t))), prop::collection::vec(inner, 2..3).prop_map(Ty::Tuple)]
    })
}

fn ty() -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![
        6 => prop_oneof![Just(Ty::Int), Just(Ty::Float), Just(Ty::Bool), Just(Ty::Str), Just(Ty::Bytes), Just(Ty::Unit), Just(Ty::Duration)],
        3 => prop_oneof![Just(Ty::Range), Just(Ty::Plain), Just(Ty::Mut)],
        1 => prop_oneof![Just(Ty::Fn), Just(Ty::Task), Just(Ty::Fns)],
    ];
    leaf.prop_recursive(3, 10, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| Ty::List(Box::new(t))),
            hashable().prop_map(|t| Ty::Set(Box::new(t))),
            (hashable(), inner.clone()).prop_map(|(k, v)| Ty::Map(Box::new(k), Box::new(v))),
            inner.clone().prop_map(|t| Ty::Option(Box::new(t))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
            inner.clone().prop_map(|t| Ty::Wrap(Box::new(t))),
            inner.clone().prop_map(|t| Ty::Opt(Box::new(t))),
            inner.clone().prop_map(|t| Ty::Tree(Box::new(t))),
            inner.prop_map(|t| Ty::Phantom(Box::new(t))),
        ]
    })
}

/// Runs `src`: its standard output (or run-time error), or the messages of
/// its compile errors.
fn outcome(src: &str) -> Result<String, Vec<String>> {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().policy(Policy::deny_all()).stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    match rt.block_on(engine.run_source("main.sand", src)) {
        Ok(_) => Ok(out.contents()),
        Err(Error::Runtime { message, .. }) => Ok(message),
        Err(Error::Compile(ds)) => Err(ds.into_iter().map(|d| d.message).collect()),
        Err(e) => panic!("{e}\n{src}"),
    }
}

/// The traits that bounds `bs` imply, by the prelude's supertraits.
fn closure(bs: &[Bound]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for b in bs {
        let implied: &[&str] = match b {
            Bound::Eq => &["Eq"],
            Bound::Ord => &["Ord", "Eq"],
            Bound::Hash => &["Hash", "Eq"],
            Bound::Encode => &["Encode"],
            Bound::Num => &["Num", "Ord", "Eq", "Encode"],
            Bound::Keyed => &["Keyed", "Hash", "Eq"],
            Bound::Scale => &["Scale", "Num", "Ord", "Eq", "Encode"],
            Bound::Marker => &["Marker"],
        };
        out.extend(implied);
    }
    out
}

#[derive(Clone, Copy, Debug)]
enum Bound {
    Eq,
    Ord,
    Hash,
    Encode,
    Num,
    Keyed,
    Scale,
    Marker,
}

impl Bound {
    fn name(self) -> &'static str {
        match self {
            Bound::Eq => "Eq",
            Bound::Ord => "Ord",
            Bound::Hash => "Hash",
            Bound::Encode => "Encode",
            Bound::Num => "Num",
            Bound::Keyed => "Keyed",
            Bound::Scale => "Scale",
            Bound::Marker => "Marker",
        }
    }
}

fn bound() -> impl Strategy<Value = Bound> {
    prop::sample::select(vec![Bound::Eq, Bound::Ord, Bound::Hash, Bound::Encode, Bound::Num, Bound::Keyed, Bound::Scale, Bound::Marker])
}

const TRAITS: &str = "trait Keyed: Hash {}\ntrait Scale: Num {\n    fn twice(self) -> Self { self + self }\n}\ntrait Marker {}\n\
                      impl Keyed for int {}\nimpl Scale for int {}\nimpl Marker for int {}";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn types_implement_the_built_in_traits_that_their_components_implement(t in ty()) {
        for tr in Tr::ALL {
            let (probe, printed) = tr.probe();
            let src = format!(
                "import json\n{TYPES}\nfn need<T: {}>(x: T) -> T {{ x }}\nlet x: {} = {}\nprint({probe})",
                tr.name(),
                t.sand(),
                t.value()
            );
            match outcome(&src) {
                Ok(out) => {
                    prop_assert!(t.implements(tr), "accepted:\n{src}");
                    if tr == Tr::Encode && !t.json_keys() {
                        prop_assert!(out.contains("a JSON object key must be"), "{out}\n{src}");
                    } else {
                        prop_assert_eq!(out.trim(), printed, "{}", src);
                    }
                }
                Err(msgs) => {
                    prop_assert!(!t.implements(tr), "rejected: {msgs:?}\n{src}");
                    let want = format!("does not implement `{}`", tr.name());
                    prop_assert!(msgs.iter().all(|m| m.contains(&want) && !m.contains("``")), "{msgs:?}\n{src}");
                }
            }
        }
    }

    #[test]
    fn traits_with_built_in_supertraits_need_them_of_their_impls(t in ty(), tr in prop::sample::select(Tr::ALL.to_vec())) {
        prop_assume!(!matches!(t, Ty::Fn));
        let src = format!(
            "{TYPES}\ntrait Via: {} {{\n    fn via(self) -> int {{ 7 }}\n}}\nimpl Via for {} {{}}\nlet x: {} = {}\nprint(x.via())",
            tr.name(),
            t.sand(),
            t.sand(),
            t.value()
        );
        match outcome(&src) {
            Ok(out) => {
                prop_assert!(t.implements(tr), "accepted:\n{src}");
                prop_assert_eq!(out.trim(), "7", "{}", src);
            }
            Err(msgs) => {
                prop_assert!(!t.implements(tr), "rejected: {msgs:?}\n{src}");
                let want = format!("does not implement `{}`, which trait `Via` requires", tr.name());
                prop_assert!(msgs.iter().all(|m| m.contains(&want)), "{msgs:?}\n{src}");
            }
        }
    }

    #[test]
    fn bounds_imply_their_supertraits(bs in prop::collection::vec(bound(), 1..4), need in bound()) {
        let names: Vec<&str> = bs.iter().map(|b| b.name()).collect();
        let src = format!(
            "{TRAITS}\nfn need<T: {}>(x: T) -> T {{ x }}\nfn probe<T: {}>(x: T) -> T {{ need(x) }}\nprint(probe(3))",
            need.name(),
            names.join(" + ")
        );
        let implied = closure(&bs).contains(&need.name());
        match outcome(&src) {
            Ok(out) => {
                prop_assert!(implied, "accepted:\n{src}");
                prop_assert_eq!(out, "3\n");
            }
            Err(msgs) => {
                prop_assert!(!implied, "rejected: {msgs:?}\n{src}");
                let want = format!("type parameter `T` does not implement `{0}`; add a bound `T: {0}`", need.name());
                prop_assert_eq!(msgs, vec![want], "{}", src);
            }
        }
    }
}
