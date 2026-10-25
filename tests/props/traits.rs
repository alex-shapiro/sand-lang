//! Property tests for derived built-in traits (`Eq`, `Ord`, `Hash`, `Encode`),
//! supertrait implication, and supertrait method dispatch.

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

    /// Returns a Sand literal expression of this type.
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
            Ty::Plain => "Plain(a: 1, b: \"x\")".into(),
            Ty::Mut => "Mut(n: 1)".into(),
            Ty::Fns => "Fns(f: () => 1)".into(),
            Ty::List(t) => format!("[{}]", t.value()),
            Ty::Set(t) => format!("Set([{}])", t.value()),
            Ty::Map(k, v) => format!("[{}: {}]", k.value(), v.value()),
            Ty::Option(t) => format!("Some({})", t.value()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::value).collect::<Vec<_>>().join(", ")),
            Ty::Wrap(t) => format!("Wrap(v: {})", t.value()),
            Ty::Opt(t) => format!("Opt.Has({})", t.value()),
            Ty::Tree(t) => format!("Tree(v: {})", t.value()),
            Ty::Phantom(_) => "Phantom(n: 1)".into(),
        }
    }

    /// Returns `true` if `self` implements derived trait `tr` in the reference model.
    fn implements(&self, tr: Trait) -> bool {
        use Trait::*;
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

    /// Returns `true` if all map keys in `self` are valid JSON object keys at runtime.
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

/// Runs `src` and returns either stdout (or runtime error message) or compile
/// error messages.
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

/// Returns the transitive supertrait closure of `ts`.
fn closure(ts: &[Trait]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for t in ts {
        let implied: &[&str] = match t {
            Trait::Eq => &["Eq"],
            Trait::Ord => &["Ord", "Eq"],
            Trait::Hash => &["Hash", "Eq"],
            Trait::Encode => &["Encode"],
            Trait::Num => &["Num", "Ord", "Eq", "Encode"],
            Trait::Keyed => &["Keyed", "Hash", "Eq"],
            Trait::Scale => &["Scale", "Num", "Ord", "Eq", "Encode"],
            Trait::Marker => &["Marker"],
        };
        out.extend(implied);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Trait {
    Eq,
    Ord,
    Hash,
    Encode,
    Num,
    Keyed,
    Scale,
    Marker,
}

impl Trait {
    /// Built-in traits derived by the compiler.
    const DERIVED: [Trait; 4] = [Trait::Eq, Trait::Ord, Trait::Hash, Trait::Encode];

    /// Returns a probe expression that exercises `self` on `x` and its expected
    /// output.
    fn probe(self) -> (&'static str, &'static str) {
        match self {
            Trait::Eq => ("need(x) == x", "true"),
            Trait::Ord => ("need(x) <= x", "true"),
            Trait::Hash => ("Set([need(x), x]).len()", "1"),
            Trait::Encode => ("json.encode(need(x)).len() > 0", "true"),
            _ => unreachable!("{self:?} is not derived"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Trait::Eq => "Eq",
            Trait::Ord => "Ord",
            Trait::Hash => "Hash",
            Trait::Encode => "Encode",
            Trait::Num => "Num",
            Trait::Keyed => "Keyed",
            Trait::Scale => "Scale",
            Trait::Marker => "Marker",
        }
    }
}

fn trait_strategy() -> impl Strategy<Value = Trait> {
    prop::sample::select(vec![Trait::Eq, Trait::Ord, Trait::Hash, Trait::Encode, Trait::Num, Trait::Keyed, Trait::Scale, Trait::Marker])
}

const TRAITS: &str = "trait Keyed: Hash {}\ntrait Scale: Num {\n    fn twice(self) -> Self { self + self }\n}\ntrait Marker {}\n\
                      impl Keyed for int {}\nimpl Scale for int {}\nimpl Marker for int {}";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn types_implement_the_built_in_traits_that_their_components_implement(t in ty()) {
        for tr in Trait::DERIVED {
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
                    if tr == Trait::Encode && !t.json_keys() {
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
    fn traits_with_built_in_supertraits_need_them_of_their_impls(t in ty(), tr in prop::sample::select(Trait::DERIVED.to_vec())) {
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
    fn required_traits_imply_their_supertraits(ts in prop::collection::vec(trait_strategy(), 1..4), need in trait_strategy()) {
        let names: Vec<&str> = ts.iter().map(|t| t.name()).collect();
        let src = format!(
            "{TRAITS}\nfn need<T: {}>(x: T) -> T {{ x }}\nfn probe<T: {}>(x: T) -> T {{ need(x) }}\nprint(probe(3))",
            need.name(),
            names.join(" + ")
        );
        let implied = closure(&ts).contains(&need.name());
        match outcome(&src) {
            Ok(out) => {
                prop_assert!(implied, "accepted:\n{src}");
                prop_assert_eq!(out, "3\n");
            }
            Err(msgs) => {
                prop_assert!(!implied, "rejected: {msgs:?}\n{src}");
                let want = format!("type parameter `T` does not implement `{0}`; add `T: {0}`", need.name());
                prop_assert_eq!(msgs, vec![want], "{}", src);
            }
        }
    }

    #[test]
    fn calls_through_supertraits_reach_the_right_method(g in graph()) {
        let out = outcome(&g.sand()).unwrap_or_else(|e| panic!("{e:?}\n{}", g.sand()));
        prop_assert_eq!(out, g.expected(), "{}", g.sand());
    }
}

/// Random directed acyclic graph of traits (`T0`..`Tn-1`) implemented for `int`
/// and `[A]`, mixing methodless traits (without runtime dictionaries) and traits
/// with methods.
#[derive(Clone, Debug)]
struct Graph {
    traits: Vec<Node>,
}

#[derive(Clone, Debug)]
struct Node {
    has_method: bool,
    /// Direct supertraits, all with a larger index.
    supers: Vec<usize>,
    builtin: Option<&'static str>,
    /// Additional reachable supertrait bound on `A` in `impl<A> Ti for [A]`.
    extra: Option<usize>,
}

fn graph() -> impl Strategy<Value = Graph> {
    (2..7usize).prop_flat_map(|n| {
        let node = (
            any::<bool>(),
            prop::collection::vec(any::<bool>(), n),
            prop::option::of(prop::sample::select(vec!["Eq", "Ord", "Encode"])),
            any::<prop::sample::Index>(),
            any::<bool>(),
        );
        prop::collection::vec(node, n).prop_map(move |raw| {
            let mut traits: Vec<Node> = raw
                .iter()
                .enumerate()
                .map(|(i, (has_method, sup, builtin, _, _))| Node {
                    has_method: *has_method,
                    supers: (i + 1..n).filter(|&j| sup[j]).collect(),
                    builtin: *builtin,
                    extra: None,
                })
                .collect();
            for (i, (_, _, _, pick, use_extra)) in raw.iter().enumerate() {
                let reach: Vec<usize> = Graph::reach(&traits, i).into_iter().filter(|&j| j != i).collect();
                if *use_extra && !reach.is_empty() {
                    traits[i].extra = Some(reach[pick.index(reach.len())]);
                }
            }
            Graph { traits }
        })
    })
}

impl Graph {
    /// Returns trait `i` and all of its transitive supertraits in ascending order.
    fn reach(traits: &[Node], i: usize) -> Vec<usize> {
        let mut out = vec![i];
        let mut k = 0;
        while k < out.len() {
            for &j in &traits[out[k]].supers {
                if !out.contains(&j) {
                    out.push(j);
                }
            }
            k += 1;
        }
        out.sort_unstable();
        out
    }

    /// Returns the first supertrait of `i` with a method, for `di` to delegate to.
    fn delegate(&self, i: usize) -> Option<usize> {
        Graph::reach(&self.traits, i).into_iter().find(|&j| j != i && self.traits[j].has_method)
    }

    fn sand(&self) -> String {
        let mut s = String::new();
        for (i, t) in self.traits.iter().enumerate() {
            let mut supers: Vec<String> = t.supers.iter().map(|j| format!("T{j}")).collect();
            supers.extend(t.builtin.map(String::from));
            let colon = if supers.is_empty() { String::new() } else { format!(": {}", supers.join(" + ")) };
            s += &format!("trait T{i}{colon} {{\n");
            if t.has_method {
                let body = match self.delegate(i) {
                    Some(j) => format!("\"d\" + self.m{j}()"),
                    None => format!("\"d{i}\""),
                };
                s += &format!("    fn m{i}(self) -> str\n    fn d{i}(self) -> str {{ {body} }}\n");
            }
            s += "}\n";
            let requires = match t.extra {
                Some(k) => format!("T{i} + T{k}"),
                None => format!("T{i}"),
            };
            if t.has_method {
                let extra = match t.extra {
                    Some(k) if self.traits[k].has_method => format!(" + self[0].m{k}()"),
                    _ => String::new(),
                };
                s += &format!("impl T{i} for int {{\n    fn m{i}(self) -> str {{ \"i{i}\" }}\n}}\n");
                s += &format!("impl<A: {requires}> T{i} for [A] {{\n    fn m{i}(self) -> str {{ \"L\" + self[0].m{i}(){extra} }}\n}}\n");
            } else {
                s += &format!("impl T{i} for int {{}}\nimpl<A: {requires}> T{i} for [A] {{}}\n");
            }
        }
        for r in 0..self.traits.len() {
            let calls: Vec<String> = self.called(r).iter().flat_map(|j| [format!("x.m{j}()"), format!("x.d{j}()")]).collect();
            let body = if calls.is_empty() { "\"-\"".to_string() } else { format!("[{}].join(\",\")", calls.join(", ")) };
            s += &format!("fn via{r}<T: T{r}>(x: T) -> str {{ {body} }}\nprint(via{r}(1))\nprint(via{r}([2]))\nprint(via{r}([[3]]))\n");
        }
        s
    }

    /// Returns the reachable traits from `r` that declare methods.
    fn called(&self, r: usize) -> Vec<usize> {
        Graph::reach(&self.traits, r).into_iter().filter(|&j| self.traits[j].has_method).collect()
    }

    /// Returns the expected result of `m{i}` for an `int` nested in `depth` lists.
    fn m(&self, i: usize, depth: usize) -> String {
        if depth == 0 {
            return format!("i{i}");
        }
        let extra = match self.traits[i].extra {
            Some(k) if self.traits[k].has_method => self.m(k, depth - 1),
            _ => String::new(),
        };
        format!("L{}{extra}", self.m(i, depth - 1))
    }

    fn d(&self, i: usize, depth: usize) -> String {
        match self.delegate(i) {
            Some(j) => format!("d{}", self.m(j, depth)),
            None => format!("d{i}"),
        }
    }

    fn expected(&self) -> String {
        let mut out = String::new();
        for r in 0..self.traits.len() {
            for depth in 0..3 {
                let calls: Vec<String> = self.called(r).iter().flat_map(|&j| [self.m(j, depth), self.d(j, depth)]).collect();
                out += &if calls.is_empty() { "-".to_string() } else { calls.join(",") };
                out.push('\n');
            }
        }
        out
    }
}
