//! Generated generic programs against a Rust model of the trait rules, in the
//! manner of Hephaestus (Chaliasos et al., PLDI 2022). Each case declares
//! random traits (static, instance and default methods, supertraits, built-in
//! supertraits), impls for built-in and generic struct types that require the
//! trait of their type parameters, and generic functions that call through
//! dictionaries built in a loop and through supertraits. The model, not the
//! checker, decides which types implement which traits:
//!
//! - a program that the model types must compile, and print what the model
//!   predicts with and without optimizations; `let` annotations are left out
//!   where the value alone determines the type;
//! - the same program with one more use of a type that the model says lacks a
//!   required trait must give a compile error.

use super::{Mode, int_lit, run_modes, str_lit};
use proptest::prelude::*;
use sandlang::{Config, Engine, Error, Policy};
use std::cmp::Ordering;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum B {
    Eq,
    Ord,
    Hash,
    Encode,
}

impl B {
    fn name(self) -> &'static str {
        match self {
            B::Eq => "Eq",
            B::Ord => "Ord",
            B::Hash => "Hash",
            B::Encode => "Encode",
        }
    }
}

/// The type constructors that user traits have impls for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ctor {
    Int,
    Str,
    List,
    Opt,
    Pair,
    Wrap,
    Keyed,
}

const CTORS: [Ctor; 7] = [Ctor::Int, Ctor::Str, Ctor::List, Ctor::Opt, Ctor::Pair, Ctor::Wrap, Ctor::Keyed];

/// `Keyed<A>` requires `A: Hash` through its field, `Outer<A>` through
/// `Keyed<A>`, and `Tagged<A>` in its declaration.
const TYPES: &str = "struct Wrap<A> { v: A }\nstruct Keyed<A> { s: Set<A> }\nstruct Outer<A> { k: Keyed<A> }\n\
                     struct Tagged<A: Hash> { v: A }\nstruct Cell<A> { var v: A }\n";

const PROBES: &str = "fn eqp<X: Eq>(x: X, y: X) -> str { \"\\(x == y)\" }
fn hashp<X: Hash>(x: X, y: X) -> str { \"\\(set([x, y, x]).len())\" }
fn ordp<X: Ord>(x: X, y: X) -> str { \"\\(x < y) \\(x <= y)\" }
fn rtp<reflect X: Encode + Eq>(x: X) -> str {
    let r: Result<X> = json.decode(json.encode(x))
    \"\\(r == Ok(x))\"
}
fn countb<X: Eq + Ord + Hash + Encode>(xs: [X]) -> int { xs.len() }
";

#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Int,
    Str,
    Bool,
    Float,
    Fn,
    List(Box<Ty>),
    Opt(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Pair(Box<Ty>, Box<Ty>),
    Wrap(Box<Ty>),
    Keyed(Box<Ty>),
    Outer(Box<Ty>),
    Tagged(Box<Ty>),
    Cell(Box<Ty>),
}

impl Ty {
    fn sand(&self) -> String {
        match self {
            Ty::Int => "int".into(),
            Ty::Str => "str".into(),
            Ty::Bool => "bool".into(),
            Ty::Float => "float".into(),
            Ty::Fn => "fn() -> int".into(),
            Ty::List(t) => format!("[{}]", t.sand()),
            Ty::Opt(t) => format!("Option<{}>", t.sand()),
            Ty::Set(t) => format!("Set<{}>", t.sand()),
            Ty::Map(k, v) => format!("[{}: {}]", k.sand(), v.sand()),
            Ty::Pair(a, b) => format!("({}, {})", a.sand(), b.sand()),
            Ty::Wrap(t) => format!("Wrap<{}>", t.sand()),
            Ty::Keyed(t) => format!("Keyed<{}>", t.sand()),
            Ty::Outer(t) => format!("Outer<{}>", t.sand()),
            Ty::Tagged(t) => format!("Tagged<{}>", t.sand()),
            Ty::Cell(t) => format!("Cell<{}>", t.sand()),
        }
    }

    fn ctor(&self) -> Option<Ctor> {
        Some(match self {
            Ty::Int => Ctor::Int,
            Ty::Str => Ctor::Str,
            Ty::List(_) => Ctor::List,
            Ty::Opt(_) => Ctor::Opt,
            Ty::Pair(..) => Ctor::Pair,
            Ty::Wrap(_) => Ctor::Wrap,
            Ty::Keyed(_) => Ctor::Keyed,
            _ => return None,
        })
    }

    fn parts(&self) -> Vec<&Ty> {
        match self {
            Ty::List(t) | Ty::Opt(t) | Ty::Set(t) | Ty::Wrap(t) | Ty::Keyed(t) | Ty::Outer(t) | Ty::Tagged(t) | Ty::Cell(t) => vec![t],
            Ty::Map(a, b) | Ty::Pair(a, b) => vec![a, b],
            _ => Vec::new(),
        }
    }

    /// The model of the derived impls (SPEC §5.6).
    fn implements(&self, b: B) -> bool {
        let own = match self {
            Ty::Int | Ty::Str | Ty::Bool | Ty::Opt(_) | Ty::Pair(..) | Ty::Wrap(_) | Ty::Tagged(_) => true,
            Ty::Float | Ty::List(_) | Ty::Cell(_) => b != B::Hash,
            Ty::Set(_) | Ty::Map(..) | Ty::Keyed(_) | Ty::Outer(_) => matches!(b, B::Eq | B::Encode),
            Ty::Fn => false,
        };
        own && self.parts().iter().all(|t| t.implements(b))
    }

    /// Whether the type meets the requirements of its type constructors:
    /// set elements, map keys and the arguments of `Keyed`, `Outer` and
    /// `Tagged` implement `Hash`.
    fn well_formed(&self) -> bool {
        let own = match self {
            Ty::Set(t) | Ty::Keyed(t) | Ty::Outer(t) | Ty::Tagged(t) | Ty::Map(t, _) => t.implements(B::Hash),
            _ => true,
        };
        own && self.parts().iter().all(|t| t.well_formed())
    }

    /// Whether `json.decode` of `json.encode(x)` gives back `x`: `Some(None)`
    /// encodes as `null`, and only string map keys are kept as they are.
    fn round_trips(&self) -> bool {
        let own = match self {
            Ty::Opt(t) => !matches!(**t, Ty::Opt(_)),
            Ty::Map(k, _) => **k == Ty::Str,
            _ => true,
        };
        own && self.implements(B::Eq) && self.implements(B::Encode) && self.parts().iter().all(|t| t.round_trips())
    }

    /// Whether every value of the type is written without an annotation
    /// that the checker needs (no empty collections or `None`).
    fn self_typed(&self, v: &Val) -> bool {
        match (self, v) {
            (Ty::List(t), Val::List(xs)) | (Ty::Set(t), Val::Set(xs)) | (Ty::Keyed(t), Val::Set(xs)) | (Ty::Outer(t), Val::Set(xs)) => {
                !xs.is_empty() && xs.iter().all(|x| t.self_typed(x))
            }
            (Ty::Map(k, t), Val::Map(kvs)) => !kvs.is_empty() && kvs.iter().all(|(a, b)| k.self_typed(a) && t.self_typed(b)),
            (Ty::Opt(t), Val::Opt(Some(x))) | (Ty::Wrap(t) | Ty::Tagged(t) | Ty::Cell(t), Val::Wrap(x)) => t.self_typed(x),
            (Ty::Opt(_), Val::Opt(None)) => false,
            (Ty::Pair(a, b), Val::Pair(x, y)) => a.self_typed(x) && b.self_typed(y),
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
enum Val {
    Int(i64),
    Str(String),
    Bool(bool),
    Float(f64),
    Fn,
    List(Vec<Val>),
    Opt(Option<Box<Val>>),
    /// A set, also the field of `Keyed` and `Outer`, without duplicates.
    Set(Vec<Val>),
    Map(Vec<(Val, Val)>),
    Pair(Box<Val>, Box<Val>),
    /// The field of `Wrap`, `Tagged` or `Cell`.
    Wrap(Box<Val>),
}

impl Val {
    fn sand(&self, ty: &Ty) -> String {
        let list = |xs: &[Val], t: &Ty| xs.iter().map(|x| x.sand(t)).collect::<Vec<_>>().join(", ");
        match (ty, self) {
            (_, Val::Int(n)) => int_lit(*n),
            (_, Val::Str(s)) => str_lit(s),
            (_, Val::Bool(b)) => b.to_string(),
            (_, Val::Float(f)) => format!("({f:?})"),
            (_, Val::Fn) => "() => 1".into(),
            (Ty::List(t), Val::List(xs)) => format!("[{}]", list(xs, t)),
            (Ty::Opt(t), Val::Opt(Some(x))) => format!("Some({})", x.sand(t)),
            (Ty::Opt(_), Val::Opt(None)) => "None".into(),
            (Ty::Set(t), Val::Set(xs)) => format!("set([{}])", list(xs, t)),
            (Ty::Keyed(t), Val::Set(xs)) => format!("Keyed {{ s: set([{}]) }}", list(xs, t)),
            (Ty::Outer(t), Val::Set(xs)) => format!("Outer {{ k: Keyed {{ s: set([{}]) }} }}", list(xs, t)),
            (Ty::Map(..), Val::Map(kvs)) if kvs.is_empty() => "[:]".into(),
            (Ty::Map(k, t), Val::Map(kvs)) => {
                format!("[{}]", kvs.iter().map(|(a, b)| format!("{}: {}", a.sand(k), b.sand(t))).collect::<Vec<_>>().join(", "))
            }
            (Ty::Pair(a, b), Val::Pair(x, y)) => format!("({}, {})", x.sand(a), y.sand(b)),
            (Ty::Wrap(t), Val::Wrap(x)) => format!("Wrap {{ v: {} }}", x.sand(t)),
            (Ty::Tagged(t), Val::Wrap(x)) => format!("Tagged {{ v: {} }}", x.sand(t)),
            (Ty::Cell(t), Val::Wrap(x)) => format!("Cell {{ v: {} }}", x.sand(t)),
            (t, v) => unreachable!("{v:?} is not a value of {t:?}"),
        }
    }

    fn eq(&self, other: &Val) -> bool {
        let same_set = |xs: &[Val], ys: &[Val]| xs.len() == ys.len() && xs.iter().all(|x| ys.iter().any(|y| x.eq(y)));
        match (self, other) {
            (Val::Int(a), Val::Int(b)) => a == b,
            (Val::Str(a), Val::Str(b)) => a == b,
            (Val::Bool(a), Val::Bool(b)) => a == b,
            (Val::Float(a), Val::Float(b)) => a == b,
            (Val::List(a), Val::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq(y)),
            (Val::Opt(a), Val::Opt(b)) => match (a, b) {
                (Some(x), Some(y)) => x.eq(y),
                (None, None) => true,
                _ => false,
            },
            (Val::Set(a), Val::Set(b)) => same_set(a, b),
            (Val::Map(a), Val::Map(b)) => a.len() == b.len() && a.iter().all(|(k, v)| b.iter().any(|(k2, v2)| k.eq(k2) && v.eq(v2))),
            (Val::Pair(a, b), Val::Pair(c, d)) => a.eq(c) && b.eq(d),
            (Val::Wrap(a), Val::Wrap(b)) => a.eq(b),
            _ => false,
        }
    }

    /// The order of values of a type that implements `Ord`. Variants order by
    /// declaration, so `Some(_) < None`.
    fn cmp(&self, other: &Val) -> Ordering {
        match (self, other) {
            (Val::Int(a), Val::Int(b)) => a.cmp(b),
            (Val::Str(a), Val::Str(b)) => a.cmp(b),
            (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
            (Val::Float(a), Val::Float(b)) => a.partial_cmp(b).unwrap(),
            (Val::List(a), Val::List(b)) => {
                a.iter().zip(b).map(|(x, y)| x.cmp(y)).find(|o| o.is_ne()).unwrap_or_else(|| a.len().cmp(&b.len()))
            }
            (Val::Opt(a), Val::Opt(b)) => match (a, b) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            (Val::Pair(a, b), Val::Pair(c, d)) => a.cmp(c).then_with(|| b.cmp(d)),
            (Val::Wrap(a), Val::Wrap(b)) => a.cmp(b),
            (a, b) => unreachable!("{a:?} and {b:?} are not ordered"),
        }
    }
}

#[derive(Debug)]
struct Trait {
    methods: bool,
    /// Direct supertraits, all with a larger index.
    supers: Vec<usize>,
    builtin: Option<B>,
    /// The constructors with an impl.
    ctors: Vec<Ctor>,
}

struct World {
    traits: Vec<Trait>,
}

impl World {
    fn new(rng: &mut Rng) -> World {
        let n = 1 + rng.below(4);
        let blank = || Trait { methods: false, supers: Vec::new(), builtin: None, ctors: Vec::new() };
        let mut w = World { traits: (0..n).map(|_| blank()).collect() };
        for i in (0..n).rev() {
            w.traits[i].methods = !rng.chance(4);
            w.traits[i].supers = (i + 1..n).filter(|_| rng.chance(3)).collect();
            w.traits[i].builtin = [None, None, Some(B::Eq), Some(B::Ord), Some(B::Hash), Some(B::Encode)][rng.below(6)];
            // The type of each impl must implement the supertraits.
            let builtins = w.builtins(i);
            let allowed: Vec<Ctor> = CTORS
                .into_iter()
                .filter(|c| w.traits[i].supers.iter().all(|&j| w.traits[j].ctors.contains(c)))
                .filter(|&c| builtins.iter().all(|&b| ctor_can(c, b)))
                .collect();
            w.traits[i].ctors = allowed.into_iter().filter(|_| !rng.chance(4)).collect();
        }
        w
    }

    /// Trait `i` and its supertraits, in increasing order.
    fn closure(&self, i: usize) -> Vec<usize> {
        let mut out = vec![i];
        let mut k = 0;
        while k < out.len() {
            for &j in &self.traits[out[k]].supers {
                if !out.contains(&j) {
                    out.push(j);
                }
            }
            k += 1;
        }
        out.sort_unstable();
        out
    }

    /// The built-in traits that trait `i` implies.
    fn builtins(&self, i: usize) -> Vec<B> {
        let mut out = Vec::new();
        for j in self.closure(i) {
            match self.traits[j].builtin {
                Some(b @ (B::Ord | B::Hash)) => out.extend([b, B::Eq]),
                Some(b) => out.push(b),
                None => {}
            }
        }
        out
    }

    fn has_dict(&self, i: usize) -> bool {
        self.closure(i).iter().any(|&j| self.traits[j].methods)
    }

    /// The supertraits of `i` (direct or not) with methods.
    fn method_supers(&self, i: usize) -> Vec<usize> {
        self.closure(i).into_iter().filter(|&j| j != i && self.traits[j].methods).collect()
    }

    /// The model of trait `i`'s impls: one for each of its constructors,
    /// which requires the trait of each type argument.
    fn implements(&self, t: &Ty, i: usize) -> bool {
        t.ctor().is_some_and(|c| self.traits[i].ctors.contains(&c)) && t.parts().iter().all(|p| self.implements(p, i))
    }

    fn sand(&self) -> String {
        let mut s = String::new();
        for (i, t) in self.traits.iter().enumerate() {
            let mut supers: Vec<String> = t.supers.iter().map(|j| format!("T{j}")).collect();
            supers.extend(t.builtin.map(|b| b.name().to_string()));
            let colon = if supers.is_empty() { String::new() } else { format!(": {}", supers.join(" + ")) };
            if t.methods {
                let extra = self.method_supers(i).first().map(|j| format!(" + \"/\" + self.show{j}()")).unwrap_or_default();
                s += &format!(
                    "trait T{i}{colon} {{\n    fn tag{i}() -> str\n    fn show{i}(self) -> str\n    fn both{i}(self) -> str {{ Self.tag{i}() + \"=\" + self.show{i}(){extra} }}\n}}\n"
                );
            } else {
                s += &format!("trait T{i}{colon} {{}}\n");
            }
            for &c in &t.ctors {
                s += &impl_source(i, c, t.methods);
            }
        }
        for (i, t) in self.traits.iter().enumerate() {
            let vias: String = self.method_supers(i).iter().map(|j| format!("    out = out + \" \" + via{j}(x)\n")).collect();
            if t.methods {
                s += &format!("fn via{i}<Y: T{i}>(y: Y) -> str {{ Y.tag{i}() + \":\" + y.show{i}() }}\n");
                let wraps: String =
                    self.wraps(i).iter().map(|&c| format!("        out = out + \" \" + {}.show{i}()\n", wrap_source(c))).collect();
                s += &format!(
                    "fn use{i}<X: T{i}>(x: X) -> str {{\n    var out = X.tag{i}() + \" \" + x.both{i}()\n    for _ in 0..1 {{\n{wraps}    }}\n{vias}    out\n}}\n"
                );
            } else {
                s += &format!("fn use{i}<X: T{i}>(x: X) -> str {{\n    var out = \"m{i}\"\n{vias}    out\n}}\n");
            }
            s += &format!("fn count{i}<X: T{i}>(xs: [X]) -> int {{ xs.len() }}\n");
        }
        s
    }

    /// The constructors that `use{i}` applies to its argument in its loop.
    fn wraps(&self, i: usize) -> Vec<Ctor> {
        let hash = self.builtins(i).contains(&B::Hash);
        self.traits[i].ctors.iter().copied().filter(|&c| !matches!(c, Ctor::Int | Ctor::Str) && (c != Ctor::Keyed || hash)).collect()
    }

    fn tag(&self, t: &Ty, i: usize) -> String {
        match t {
            Ty::Int => format!("int{i}"),
            Ty::Str => format!("str{i}"),
            Ty::List(a) => format!("[{}]", self.tag(a, i)),
            Ty::Opt(a) => format!("{}?", self.tag(a, i)),
            Ty::Pair(a, b) => format!("({},{})", self.tag(a, i), self.tag(b, i)),
            Ty::Wrap(a) => format!("Wrap<{}>", self.tag(a, i)),
            Ty::Keyed(a) => format!("Keyed<{}>", self.tag(a, i)),
            _ => unreachable!("{t:?} has no impl"),
        }
    }

    fn show(&self, t: &Ty, v: &Val, i: usize) -> String {
        match (t, v) {
            (Ty::Int, Val::Int(n)) => format!("{n}#{i}"),
            (Ty::Str, Val::Str(s)) => format!("{s}#{i}"),
            (Ty::List(a), Val::List(xs)) => format!("[{}]", xs.iter().map(|x| self.show(a, x, i)).collect::<Vec<_>>().join(",")),
            (Ty::Opt(a), Val::Opt(Some(x))) => format!("S{}", self.show(a, x, i)),
            (Ty::Opt(_), Val::Opt(None)) => "N".into(),
            (Ty::Pair(a, b), Val::Pair(x, y)) => format!("({},{})", self.show(a, x, i), self.show(b, y, i)),
            (Ty::Wrap(a), Val::Wrap(x)) => format!("W{}", self.show(a, x, i)),
            (Ty::Keyed(_), Val::Set(xs)) => format!("K{}", xs.len()),
            _ => unreachable!("{v:?} of {t:?} has no impl"),
        }
    }

    /// What `use{i}(v)` prints.
    fn use_output(&self, t: &Ty, v: &Val, i: usize) -> String {
        let mut out = if self.traits[i].methods {
            let extra = self.method_supers(i).first().map(|&j| format!("/{}", self.show(t, v, j))).unwrap_or_default();
            let mut out = format!("{} {}={}{extra}", self.tag(t, i), self.tag(t, i), self.show(t, v, i));
            for c in self.wraps(i) {
                let (wt, wv) = wrap(c, t, v);
                out += &format!(" {}", self.show(&wt, &wv, i));
            }
            out
        } else {
            format!("m{i}")
        };
        for j in self.method_supers(i) {
            out += &format!(" {}:{}", self.tag(t, j), self.show(t, v, j));
        }
        out
    }

    /// A type with an impl of trait `i`, if its constructors make one.
    fn impl_ty(&self, rng: &mut Rng, i: usize, depth: u32) -> Option<Ty> {
        for _ in 0..20 {
            let ctors = &self.traits[i].ctors;
            let c = *ctors.get(rng.below(ctors.len()))?;
            let part = |rng: &mut Rng| if depth == 0 { None } else { self.impl_ty(rng, i, depth - 1) };
            let t = match c {
                Ctor::Int => Some(Ty::Int),
                Ctor::Str => Some(Ty::Str),
                Ctor::List => part(rng).map(|t| Ty::List(Box::new(t))),
                Ctor::Opt => part(rng).map(|t| Ty::Opt(Box::new(t))),
                Ctor::Wrap => part(rng).map(|t| Ty::Wrap(Box::new(t))),
                Ctor::Keyed => part(rng).filter(|t| t.implements(B::Hash)).map(|t| Ty::Keyed(Box::new(t))),
                Ctor::Pair => part(rng).zip(part(rng)).map(|(a, b)| Ty::Pair(Box::new(a), Box::new(b))),
            };
            if t.is_some() {
                return t;
            }
        }
        None
    }
}

/// Whether a type with constructor `c` implements built-in trait `b` when
/// its type arguments do.
fn ctor_can(c: Ctor, b: B) -> bool {
    match c {
        Ctor::List => b != B::Hash,
        Ctor::Keyed => matches!(b, B::Eq | B::Encode),
        _ => true,
    }
}

fn impl_source(i: usize, c: Ctor, methods: bool) -> String {
    let (head, tag, show) = match c {
        Ctor::Int => (format!("impl T{i} for int"), format!("\"int{i}\""), format!("\"\\(self)#{i}\"")),
        Ctor::Str => (format!("impl T{i} for str"), format!("\"str{i}\""), format!("self + \"#{i}\"")),
        Ctor::List => (
            format!("impl<A: T{i}> T{i} for [A]"),
            format!("\"[\" + A.tag{i}() + \"]\""),
            format!("\"[\" + self.map(x => x.show{i}()).join(\",\") + \"]\""),
        ),
        Ctor::Opt => (
            format!("impl<A: T{i}> T{i} for A?"),
            format!("A.tag{i}() + \"?\""),
            format!("match self {{ Some(a) => \"S\" + a.show{i}(), None => \"N\" }}"),
        ),
        Ctor::Pair => (
            format!("impl<A: T{i}, B: T{i}> T{i} for (A, B)"),
            format!("\"(\" + A.tag{i}() + \",\" + B.tag{i}() + \")\""),
            format!("\"(\" + self.0.show{i}() + \",\" + self.1.show{i}() + \")\""),
        ),
        Ctor::Wrap => {
            (format!("impl<A: T{i}> T{i} for Wrap<A>"), format!("\"Wrap<\" + A.tag{i}() + \">\""), format!("\"W\" + self.v.show{i}()"))
        }
        Ctor::Keyed => (
            format!("impl<A: T{i} + Hash> T{i} for Keyed<A>"),
            format!("\"Keyed<\" + A.tag{i}() + \">\""),
            "\"K\\(self.s.len())\"".to_string(),
        ),
    };
    if methods {
        format!("{head} {{\n    fn tag{i}() -> str {{ {tag} }}\n    fn show{i}(self) -> str {{ {show} }}\n}}\n")
    } else {
        format!("{head} {{}}\n")
    }
}

fn wrap_source(c: Ctor) -> &'static str {
    match c {
        Ctor::List => "[x]",
        Ctor::Opt => "Some(x)",
        Ctor::Pair => "(x, x)",
        Ctor::Wrap => "(Wrap { v: x })",
        Ctor::Keyed => "(Keyed { s: set([x]) })",
        Ctor::Int | Ctor::Str => unreachable!("not a wrapper"),
    }
}

fn wrap(c: Ctor, t: &Ty, v: &Val) -> (Ty, Val) {
    let (bt, bv) = (Box::new(t.clone()), Box::new(v.clone()));
    match c {
        Ctor::List => (Ty::List(bt), Val::List(vec![v.clone()])),
        Ctor::Opt => (Ty::Opt(bt), Val::Opt(Some(bv))),
        Ctor::Pair => (Ty::Pair(bt.clone(), bt), Val::Pair(bv.clone(), bv)),
        Ctor::Wrap => (Ty::Wrap(bt), Val::Wrap(bv)),
        Ctor::Keyed => (Ty::Keyed(bt), Val::Set(vec![v.clone()])),
        Ctor::Int | Ctor::Str => unreachable!("not a wrapper"),
    }
}

/// A random well-formed type.
fn any_ty(rng: &mut Rng, depth: u32) -> Ty {
    loop {
        let leaf = depth == 0 || rng.chance(3);
        let part = |rng: &mut Rng| Box::new(any_ty(rng, depth.saturating_sub(1)));
        let t = if leaf {
            [Ty::Int, Ty::Str, Ty::Bool, Ty::Float, Ty::Fn][rng.below(5)].clone()
        } else {
            match rng.below(10) {
                0 => Ty::List(part(rng)),
                1 => Ty::Opt(part(rng)),
                2 => Ty::Set(part(rng)),
                3 => Ty::Map(part(rng), part(rng)),
                4 => Ty::Pair(part(rng), part(rng)),
                5 => Ty::Wrap(part(rng)),
                6 => Ty::Keyed(part(rng)),
                7 => Ty::Outer(part(rng)),
                8 => Ty::Tagged(part(rng)),
                _ => Ty::Cell(part(rng)),
            }
        };
        if t.well_formed() {
            return t;
        }
    }
}

/// A random type for which `want` holds.
fn ty_where(rng: &mut Rng, want: impl Fn(&Ty) -> bool) -> Option<Ty> {
    (0..200).map(|_| any_ty(rng, 2)).find(|t| want(t))
}

fn value(rng: &mut Rng, t: &Ty) -> Val {
    let n = rng.below(3);
    let set = |rng: &mut Rng, t: &Ty| {
        let mut xs: Vec<Val> = Vec::new();
        for _ in 0..n {
            let x = value(rng, t);
            if !xs.iter().any(|y| y.eq(&x)) {
                xs.push(x);
            }
        }
        Val::Set(xs)
    };
    match t {
        Ty::Int => Val::Int(rng.below(5) as i64 - 2),
        Ty::Str => Val::Str(["a", "b", "ab", ""][rng.below(4)].into()),
        Ty::Bool => Val::Bool(rng.chance(2)),
        Ty::Float => Val::Float([0.5, 1.5, -2.0][rng.below(3)]),
        Ty::Fn => Val::Fn,
        Ty::List(a) => Val::List((0..n).map(|_| value(rng, a)).collect()),
        Ty::Opt(a) => Val::Opt((!rng.chance(3)).then(|| Box::new(value(rng, a)))),
        Ty::Set(a) | Ty::Keyed(a) | Ty::Outer(a) => set(rng, a),
        Ty::Map(k, v) => {
            let mut kvs: Vec<(Val, Val)> = Vec::new();
            for _ in 0..n {
                let key = value(rng, k);
                if !kvs.iter().any(|(y, _)| y.eq(&key)) {
                    kvs.push((key, value(rng, v)));
                }
            }
            Val::Map(kvs)
        }
        Ty::Pair(a, b) => Val::Pair(Box::new(value(rng, a)), Box::new(value(rng, b))),
        Ty::Wrap(a) | Ty::Tagged(a) | Ty::Cell(a) => Val::Wrap(Box::new(value(rng, a))),
    }
}

/// A generated program: declarations, statements, and what they print.
struct Case {
    decls: String,
    body: String,
    stdout: String,
    next: usize,
}

impl Case {
    /// Binds a new variable to `v` of type `t`, without the annotation when
    /// the value determines the type and `erase` is set.
    fn bind(&mut self, t: &Ty, v: &Val, erase: bool) -> String {
        let name = format!("v{}", self.next);
        self.next += 1;
        if erase && t.self_typed(v) {
            self.body += &format!("let {name} = {}\n", v.sand(t));
        } else {
            self.body += &format!("let {name}: {} = {}\n", t.sand(), v.sand(t));
        }
        name
    }

    fn source(&self) -> String {
        format!("import json\n{}{}", self.decls, self.body)
    }
}

/// A well-typed program, and the ill-typed additions to it: each one more
/// use of a type that lacks a required trait, with what it is.
fn generate(seed: u64) -> (Case, Vec<(String, String)>) {
    let mut rng = Rng(seed);
    let w = World::new(&mut rng);
    let mut case = Case { decls: format!("{TYPES}{PROBES}{}", w.sand()), body: String::new(), stdout: String::new(), next: 0 };
    let erase = rng.chance(2);
    for i in 0..w.traits.len() {
        for _ in 0..2 {
            if let Some(t) = w.impl_ty(&mut rng, i, 2) {
                let v = value(&mut rng, &t);
                let x = case.bind(&t, &v, erase);
                case.body += &format!("print(use{i}({x}))\n");
                case.stdout += &format!("{}\n", w.use_output(&t, &v, i));
            }
        }
    }
    for _ in 0..3 {
        let (probe, b) = [("eqp", B::Eq), ("hashp", B::Hash), ("ordp", B::Ord), ("rtp", B::Encode)][rng.below(4)];
        let want = |t: &Ty| if probe == "rtp" { t.round_trips() } else { t.implements(b) };
        let Some(t) = ty_where(&mut rng, want) else { continue };
        let x = value(&mut rng, &t);
        let y = if rng.chance(3) { x.clone() } else { value(&mut rng, &t) };
        let (a, c) = (case.bind(&t, &x, erase), case.bind(&t, &y, erase));
        let (call, out) = match probe {
            "eqp" => (format!("eqp({a}, {c})"), x.eq(&y).to_string()),
            "hashp" => (format!("hashp({a}, {c})"), if x.eq(&y) { "1" } else { "2" }.to_string()),
            "ordp" => (format!("ordp({a}, {c})"), format!("{} {}", x.cmp(&y).is_lt(), x.cmp(&y).is_le())),
            _ => (format!("rtp({a})"), "true".into()),
        };
        case.body += &format!("print({call})\n");
        case.stdout += &format!("{out}\n");
    }
    case.body += "let none: [never] = []\nprint(countb(none))\n";
    case.stdout += "0\n";
    for i in (0..w.traits.len()).filter(|&i| !w.has_dict(i)) {
        case.body += &format!("print(count{i}(none))\n");
        case.stdout += "0\n";
    }

    let mut bad = Vec::new();
    let lit = |rng: &mut Rng, t: &Ty| format!("let bad: {} = {}\n", t.sand(), value(rng, t).sand(t));
    for i in 0..w.traits.len() {
        if let Some(t) = ty_where(&mut rng, |t| !w.implements(t, i)) {
            bad.push((format!("{}print(use{i}(bad))\n", lit(&mut rng, &t)), format!("{} lacks T{i}", t.sand())));
        }
        if w.has_dict(i) {
            bad.push((format!("print(count{i}(none))\n"), format!("never lacks T{i}, which has methods")));
        }
    }
    for (probe, b) in [("eqp", B::Eq), ("hashp", B::Hash), ("ordp", B::Ord), ("rtp", B::Encode)] {
        if let Some(t) = ty_where(&mut rng, |t| !t.implements(b)) {
            let call = if probe == "rtp" { "rtp(bad)".to_string() } else { format!("{probe}(bad, bad)") };
            bad.push((format!("{}print({call})\n", lit(&mut rng, &t)), format!("{} lacks {}", t.sand(), b.name())));
        }
    }
    if let Some(t) = ty_where(&mut rng, |t| !t.implements(B::Hash)) {
        let s = t.sand();
        bad.push((format!("let bad: Keyed<{s}> = Keyed {{ s: set([]) }}\n"), format!("Keyed<{s}>")));
        bad.push((format!("let bad: Outer<{s}> = Outer {{ k: Keyed {{ s: set([]) }} }}\n"), format!("Outer<{s}>")));
        let v = value(&mut rng, &t).sand(&t);
        bad.push((format!("let bad: Tagged<{s}> = Tagged {{ v: {v} }}\n"), format!("Tagged<{s}>")));
        bad.push((format!("struct Bad {{ s: Set<{s}> }}\n"), format!("a field of type Set<{s}>")));
        bad.push((format!("let bad: Result<Set<{s}>> = json.decode(\"[]\")\n"), format!("decoding Set<{s}>")));
    }
    (case, bad)
}

/// The messages of the compile errors of `src`, or `None` if it compiles.
fn compile_errors(src: &str) -> Option<Vec<String>> {
    let engine = Engine::new(Config::new().policy(Policy::deny_all())).unwrap();
    match engine.check_source("main.sand", src) {
        Ok(()) => None,
        Err(Error::Compile(ds)) => Some(ds.into_iter().map(|d| d.message).collect()),
        Err(e) => panic!("{e}\nprogram:\n{src}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn generic_programs_print_what_the_model_predicts(seed in any::<u64>()) {
        let (case, _) = generate(seed);
        let src = case.source();
        let r = run_modes(&src, &[Mode::Optimized, Mode::Unoptimized])?;
        prop_assert_eq!(r.failure, None, "program:\n{}", src);
        prop_assert_eq!(r.stdout, case.stdout, "program:\n{}", src);
    }

    #[test]
    fn uses_of_types_that_lack_a_required_trait_do_not_compile(seed in any::<u64>()) {
        let (case, bad) = generate(seed);
        for (extra, what) in bad {
            let src = format!("{}{extra}", case.source());
            match compile_errors(&src) {
                Some(msgs) => prop_assert!(msgs.iter().any(|m| m.contains("does not implement")), "{what}: {msgs:?}\nprogram:\n{src}"),
                None => prop_assert!(false, "{what}: the program compiles\nprogram:\n{src}"),
            }
        }
    }
}
