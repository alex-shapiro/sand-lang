//! Generates random Sand programs and compares their execution against a
//! reference interpreter (`Model`).
//!
//! Programs are generated as skeletons (`SProgram`) with integer indices for
//! variable references. `Resolver` maps those indices to in-scope variables so
//! that every skeleton produces a valid program.

use super::{BOUNDARY, Mode, Run, int_lit, list_str, run_modes};
use proptest::prelude::*;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
enum Bin {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ne,
}

#[derive(Debug, Clone)]
enum SInt {
    Lit(i64),
    Var(u8),
    Bin(Bin, Box<SInt>, Box<SInt>),
    Neg(Box<SInt>),
    If(Box<SCond>, Box<SInt>, Box<SInt>),
    Call(u8, Box<SInt>, Box<SInt>),
    Apply(u8, Box<SInt>),
    Agg(Box<SSeq>, Box<SAgg>),
    Match(Box<SInt>, Vec<(i64, u8, SInt)>, Box<SInt>),
    Index(u8, Box<SInt>),
}

#[derive(Debug, Clone)]
enum SCond {
    Cmp(Cmp, SInt, SInt),
    Even(SInt),
    Not(Box<SCond>),
    And(Box<SCond>, Box<SCond>),
    Or(Box<SCond>, Box<SCond>),
    Contains(SSrc, SInt),
    Any(Box<SSeq>, Box<SPred>),
    All(Box<SSeq>, Box<SPred>),
    Test(u8, SInt),
}

#[derive(Debug, Clone)]
struct SLam {
    effect: bool,
    early: Option<(SCond, SInt)>,
    body: SInt,
}

#[derive(Debug, Clone)]
struct SPred {
    effect: bool,
    early: Option<(SCond, bool)>,
    body: SCond,
}

#[derive(Debug, Clone)]
struct SFilterMap {
    effect: bool,
    cond: SCond,
    value: SInt,
}

#[derive(Debug, Clone)]
struct SLam2 {
    effect: bool,
    body: SInt,
}

#[derive(Debug, Clone)]
enum SSrc {
    List(u8),
    Log,
    Lit(Vec<SInt>),
    Range {
        lo: SLo,
        width: i8,
        inclusive: bool,
    },
    RangeVar(u8),
    /// Range with arbitrary literal bounds; used only for $O(1)$ aggregates.
    Wide(i64, i64, bool),
}

#[derive(Debug, Clone)]
enum SLo {
    Lit(i64),
    Var(u8),
}

#[derive(Debug, Clone)]
enum SPipe {
    None,
    Eager(Vec<SEager>),
    Lazy(Vec<SAdapter>),
    Comp { filter: Option<SCond>, map: Option<(bool, SInt)> },
}

#[derive(Debug, Clone)]
struct SSeq {
    src: SSrc,
    pipe: SPipe,
}

#[derive(Debug, Clone)]
enum SEager {
    Map(SLam),
    Filter(SPred),
    FilterMap(SFilterMap),
    TakeWhile(SPred),
    SkipWhile(SPred),
    Take(SInt),
    Skip(SInt),
    Reversed,
    Sorted,
}

#[derive(Debug, Clone)]
enum SAdapter {
    Map(SLam),
    Filter(SPred),
    FilterMap(SFilterMap),
    Take(SInt),
    Skip(SInt),
    TakeWhile(SPred),
    SkipWhile(SPred),
    Inspect,
    EnumerateMap(SLam2),
    StepBy(i64),
}

#[derive(Debug, Clone)]
enum SAgg {
    Len,
    Sum,
    Product,
    Count(SPred),
    Fold(SInt, SLam2),
    Reduce(SLam2, i64),
    Min(i64),
    Max(i64),
    First(i64),
    Last(i64),
    Find(SPred, i64),
    FindIndex(SPred, i64),
}

#[derive(Debug, Clone)]
enum SStmt {
    Let { mutable: bool, value: SInt },
    LetList(SSeq),
    LetAlias(u8, bool),
    LetRange(SSrc),
    LetLam(SLam),
    LetPred(SPred),
    PrintInt(SInt),
    PrintBool(SCond),
    PrintList(SSeq),
    Assign(u8, bool, SInt),
    Push(u8, SInt),
    Pop(u8),
    If(SCond, Vec<SStmt>, Vec<SStmt>),
    For(SSeq, Vec<SStmt>),
    ForEnumerate(u8, Vec<SStmt>),
    Break(SCond, u8),
    Continue(SCond, u8),
}

#[derive(Debug, Clone)]
struct SProgram {
    fns: Vec<(bool, SInt)>,
    stmts: Vec<SStmt>,
}

fn lit() -> impl Strategy<Value = i64> + Clone {
    prop_oneof![8 => -10i64..10, 1 => prop::sample::select(BOUNDARY.as_slice())]
}

fn bin() -> impl Strategy<Value = Bin> {
    prop::sample::select(vec![Bin::Add, Bin::Sub, Bin::Mul, Bin::Div, Bin::Mod, Bin::Min, Bin::Max])
}

fn cmp() -> impl Strategy<Value = Cmp> {
    prop::sample::select(vec![Cmp::Lt, Cmp::Le, Cmp::Eq, Cmp::Ne])
}

#[derive(Clone)]
struct Exprs {
    int: BoxedStrategy<SInt>,
    cond: BoxedStrategy<SCond>,
    test: BoxedStrategy<SCond>,
    lam: BoxedStrategy<SLam>,
    pred: BoxedStrategy<SPred>,
    lam2: BoxedStrategy<SLam2>,
    seq: BoxedStrategy<SSeq>,
    src: BoxedStrategy<SSrc>,
}

fn exprs() -> Exprs {
    let leaf = prop_oneof![lit().prop_map(SInt::Lit), any::<u8>().prop_map(SInt::Var)];
    let int = leaf
        .prop_recursive(3, 24, 3, |inner| {
            let e = parts(inner.clone().boxed());
            let arms = prop::collection::vec((-10i64..10, 0u8..4, inner.clone()), 1..4);
            prop_oneof![
                4 => (bin(), inner.clone(), inner.clone()).prop_map(|(o, a, b)| SInt::Bin(o, Box::new(a), Box::new(b))),
                1 => inner.clone().prop_map(|a| SInt::Neg(Box::new(a))),
                1 => (e.test.clone(), inner.clone(), inner.clone()).prop_map(|(c, a, b)| SInt::If(Box::new(c), Box::new(a), Box::new(b))),
                1 => (any::<u8>(), inner.clone(), inner.clone()).prop_map(|(f, a, b)| SInt::Call(f, Box::new(a), Box::new(b))),
                1 => (any::<u8>(), inner.clone()).prop_map(|(g, a)| SInt::Apply(g, Box::new(a))),
                4 => (e.seq.clone(), agg(&e)).prop_map(|(s, a)| SInt::Agg(Box::new(s), Box::new(a))),
                1 => (inner.clone(), arms, inner.clone()).prop_map(|(s, arms, d)| SInt::Match(Box::new(s), arms, Box::new(d))),
                1 => (any::<u8>(), inner).prop_map(|(l, i)| SInt::Index(l, Box::new(i))),
            ]
        })
        .boxed();
    parts(int)
}

fn parts(int: BoxedStrategy<SInt>) -> Exprs {
    let base = prop_oneof![
        3 => (cmp(), int.clone(), int.clone()).prop_map(|(o, a, b)| SCond::Cmp(o, a, b)),
        1 => int.clone().prop_map(SCond::Even),
        1 => (any::<u8>(), int.clone()).prop_map(|(p, a)| SCond::Test(p, a)),
    ];
    let cond = base
        .prop_recursive(2, 6, 2, |inner| {
            prop_oneof![
                inner.clone().prop_map(|c| SCond::Not(Box::new(c))),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| SCond::And(Box::new(a), Box::new(b))),
                (inner.clone(), inner).prop_map(|(a, b)| SCond::Or(Box::new(a), Box::new(b))),
            ]
        })
        .boxed();
    let early_int = prop::option::weighted(0.2, (cond.clone(), int.clone()));
    let lam = (any::<bool>(), early_int, int.clone()).prop_map(|(effect, early, body)| SLam { effect, early, body }).boxed();
    let early_bool = prop::option::weighted(0.2, (cond.clone(), any::<bool>()));
    let pred = (any::<bool>(), early_bool, cond.clone()).prop_map(|(effect, early, body)| SPred { effect, early, body }).boxed();
    let lam2 = (prop::bool::weighted(0.3), int.clone()).prop_map(|(effect, body)| SLam2 { effect, body }).boxed();
    let filter_map = (any::<bool>(), cond.clone(), int.clone()).prop_map(|(effect, cond, value)| SFilterMap { effect, cond, value });
    let lo = prop_oneof![lit().prop_map(SLo::Lit), any::<u8>().prop_map(SLo::Var)];
    let boundary = || prop::sample::select(BOUNDARY.as_slice());
    let src = prop_oneof![
        3 => any::<u8>().prop_map(SSrc::List),
        1 => Just(SSrc::Log),
        // Non-empty so element type can be inferred when calling methods on the literal.
        2 => prop::collection::vec(int.clone(), 1..5).prop_map(SSrc::Lit),
        3 => (lo, -2i8..7, any::<bool>()).prop_map(|(lo, width, inclusive)| SSrc::Range { lo, width, inclusive }),
        1 => any::<u8>().prop_map(SSrc::RangeVar),
        1 => (boundary(), boundary(), any::<bool>()).prop_map(|(a, b, i)| SSrc::Wide(a, b, i)),
    ]
    .boxed();
    let eager = prop_oneof![
        3 => lam.clone().prop_map(SEager::Map),
        3 => pred.clone().prop_map(SEager::Filter),
        1 => filter_map.clone().prop_map(SEager::FilterMap),
        1 => pred.clone().prop_map(SEager::TakeWhile),
        1 => pred.clone().prop_map(SEager::SkipWhile),
        1 => int.clone().prop_map(SEager::Take),
        1 => int.clone().prop_map(SEager::Skip),
        1 => Just(SEager::Reversed),
        1 => Just(SEager::Sorted),
    ];
    let adapter = prop_oneof![
        3 => lam.clone().prop_map(SAdapter::Map),
        3 => pred.clone().prop_map(SAdapter::Filter),
        1 => filter_map.prop_map(SAdapter::FilterMap),
        1 => int.clone().prop_map(SAdapter::Take),
        1 => int.clone().prop_map(SAdapter::Skip),
        1 => pred.clone().prop_map(SAdapter::TakeWhile),
        1 => pred.clone().prop_map(SAdapter::SkipWhile),
        1 => Just(SAdapter::Inspect),
        1 => lam2.clone().prop_map(SAdapter::EnumerateMap),
        1 => (0i64..4).prop_map(SAdapter::StepBy),
    ];
    let comp = (prop::option::of(cond.clone()), prop::option::of((any::<bool>(), int.clone())))
        .prop_map(|(filter, map)| SPipe::Comp { filter, map });
    let pipe = prop_oneof![
        2 => Just(SPipe::None),
        3 => prop::collection::vec(eager, 1..3).prop_map(SPipe::Eager),
        3 => prop::collection::vec(adapter, 0..4).prop_map(SPipe::Lazy),
        1 => comp,
    ];
    let seq = (src.clone(), pipe).prop_map(|(src, pipe)| SSeq { src, pipe }).boxed();
    let test = prop_oneof![
        4 => cond.clone(),
        1 => (src.clone(), int.clone()).prop_map(|(s, e)| SCond::Contains(s, e)),
        1 => (seq.clone(), pred.clone()).prop_map(|(s, p)| SCond::Any(Box::new(s), Box::new(p))),
        1 => (seq.clone(), pred.clone()).prop_map(|(s, p)| SCond::All(Box::new(s), Box::new(p))),
    ]
    .boxed();
    Exprs { int, cond, test, lam, pred, lam2, seq, src }
}

fn agg(e: &Exprs) -> BoxedStrategy<SAgg> {
    let d = || -3i64..3;
    prop_oneof![
        2 => Just(SAgg::Len),
        2 => Just(SAgg::Sum),
        1 => Just(SAgg::Product),
        2 => e.pred.clone().prop_map(SAgg::Count),
        2 => (e.int.clone(), e.lam2.clone()).prop_map(|(i, f)| SAgg::Fold(i, f)),
        1 => (e.lam2.clone(), d()).prop_map(|(f, d)| SAgg::Reduce(f, d)),
        1 => d().prop_map(SAgg::Min),
        1 => d().prop_map(SAgg::Max),
        1 => d().prop_map(SAgg::First),
        1 => d().prop_map(SAgg::Last),
        1 => (e.pred.clone(), d()).prop_map(|(p, d)| SAgg::Find(p, d)),
        1 => (e.pred.clone(), d()).prop_map(|(p, d)| SAgg::FindIndex(p, d)),
    ]
    .boxed()
}

fn stmts(e: &Exprs) -> BoxedStrategy<Vec<SStmt>> {
    let simple = prop_oneof![
        3 => (any::<bool>(), e.int.clone()).prop_map(|(mutable, value)| SStmt::Let { mutable, value }),
        2 => e.seq.clone().prop_map(SStmt::LetList),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(l, copy)| SStmt::LetAlias(l, copy)),
        1 => e.src.clone().prop_map(SStmt::LetRange),
        1 => e.lam.clone().prop_map(SStmt::LetLam),
        1 => e.pred.clone().prop_map(SStmt::LetPred),
        3 => e.int.clone().prop_map(SStmt::PrintInt),
        1 => e.test.clone().prop_map(SStmt::PrintBool),
        2 => e.seq.clone().prop_map(SStmt::PrintList),
        2 => (any::<u8>(), any::<bool>(), e.int.clone()).prop_map(|(v, add, x)| SStmt::Assign(v, add, x)),
        2 => (any::<u8>(), e.int.clone()).prop_map(|(l, x)| SStmt::Push(l, x)),
        1 => any::<u8>().prop_map(SStmt::Pop),
        1 => (e.cond.clone(), any::<u8>()).prop_map(|(c, d)| SStmt::Break(c, d)),
        1 => (e.cond.clone(), any::<u8>()).prop_map(|(c, d)| SStmt::Continue(c, d)),
    ];
    let (cond, seq) = (e.test.clone(), e.seq.clone());
    let stmt = simple.prop_recursive(2, 16, 4, move |inner| {
        let body = prop::collection::vec(inner, 1..4);
        prop_oneof![
            1 => (cond.clone(), body.clone(), prop::option::of(body.clone())).prop_map(|(c, t, f)| SStmt::If(c, t, f.unwrap_or_default())),
            3 => (seq.clone(), body.clone()).prop_map(|(s, b)| SStmt::For(s, b)),
            1 => (any::<u8>(), body).prop_map(|(l, b)| SStmt::ForEnumerate(l, b)),
        ]
    });
    prop::collection::vec(stmt, 1..8).boxed()
}

fn program() -> impl Strategy<Value = SProgram> {
    let e = exprs();
    let fns = prop::collection::vec((any::<bool>(), e.int.clone()), 0..3);
    (fns, stmts(&e)).prop_map(|(fns, stmts)| SProgram { fns, stmts })
}

#[derive(Debug, Clone)]
enum E {
    Lit(i64),
    Name(String),
    Bin(Bin, Box<E>, Box<E>),
    Neg(Box<E>),
    If(Box<C>, Box<E>, Box<E>),
    Call(String, Box<E>, Box<E>),
    Apply(String, Box<E>),
    Agg(Box<Seq>, Box<Agg>),
    Match(Box<E>, Vec<(i64, i64, E)>, Box<E>),
    Index(String, Box<E>),
}

#[derive(Debug, Clone)]
enum C {
    Cmp(Cmp, E, E),
    Even(E),
    Not(Box<C>),
    And(Box<C>, Box<C>),
    Or(Box<C>, Box<C>),
    Contains(Src, E),
    Any(Box<Seq>, Box<Pred>),
    All(Box<Seq>, Box<Pred>),
    Test(String, E),
}

#[derive(Debug, Clone)]
struct Lam {
    param: String,
    effect: bool,
    early: Option<(C, E)>,
    body: E,
}

#[derive(Debug, Clone)]
struct Pred {
    param: String,
    effect: bool,
    early: Option<(C, bool)>,
    body: C,
}

#[derive(Debug, Clone)]
struct FilterMap {
    param: String,
    effect: bool,
    cond: C,
    value: E,
}

#[derive(Debug, Clone)]
struct Lam2 {
    a: String,
    b: String,
    effect: bool,
    body: E,
}

#[derive(Debug, Clone)]
enum Src {
    List(String),
    Lit(Vec<E>),
    Range(E, E, bool),
    RangeVar(String),
}

#[derive(Debug, Clone)]
enum Pipe {
    None,
    Eager(Vec<Eager>),
    Lazy(Vec<Adapter>),
    Comp(Box<Comp>),
}

#[derive(Debug, Clone)]
struct Comp {
    param: String,
    filter: Option<C>,
    map: Option<(bool, E)>,
}

#[derive(Debug, Clone)]
struct Seq {
    src: Src,
    pipe: Pipe,
}

#[derive(Debug, Clone)]
enum Eager {
    Map(Lam),
    Filter(Pred),
    FilterMap(FilterMap),
    TakeWhile(Pred),
    SkipWhile(Pred),
    Take(E),
    Skip(E),
    Reversed,
    Sorted,
}

#[derive(Debug, Clone)]
enum Adapter {
    Map(Lam),
    Filter(Pred),
    FilterMap(FilterMap),
    Take(E),
    Skip(E),
    TakeWhile(Pred),
    SkipWhile(Pred),
    Inspect(String),
    EnumerateMap(String, Lam2),
    StepBy(i64),
}

#[derive(Debug, Clone)]
enum Agg {
    Len,
    Sum,
    Product,
    Count(Pred),
    Fold(E, Lam2),
    Reduce(Lam2, i64),
    Min(i64),
    Max(i64),
    First(i64),
    Last(i64),
    Find(Pred, i64),
    FindIndex(Pred, i64),
}

#[derive(Debug, Clone)]
enum Stmt {
    Let(String, bool, E),
    LetList(String, Seq),
    LetAlias(String, String, bool),
    LetRange(String, Src),
    LetLam(String, Lam),
    LetPred(String, Pred),
    PrintInt(E),
    PrintBool(C),
    PrintList(Seq),
    Assign(String, bool, E),
    Push(String, E),
    Pop(String),
    If(C, Vec<Stmt>, Vec<Stmt>),
    For { label: Option<String>, var: String, seq: Seq, body: Vec<Stmt> },
    ForEnumerate { label: Option<String>, i: String, x: String, list: String, body: Vec<Stmt> },
    Break(C, Option<String>),
    Continue(C, Option<String>),
}

#[derive(Debug, Clone)]
struct Function {
    name: String,
    a: String,
    b: String,
    effect: bool,
    body: E,
}

#[derive(Debug, Clone)]
struct Program {
    fns: Vec<Function>,
    stmts: Vec<Stmt>,
}

#[derive(Clone)]
struct Scope {
    ints: Vec<String>,
    vars: Vec<String>,
    lists: Vec<String>,
    ranges: Vec<String>,
    lams: Vec<String>,
    preds: Vec<String>,
    fns: Vec<String>,
}

impl Default for Scope {
    fn default() -> Self {
        let none = Vec::new;
        Scope { ints: none(), vars: none(), lists: vec!["log".into()], ranges: none(), lams: none(), preds: none(), fns: none() }
    }
}

#[derive(Default)]
struct Resolver {
    scope: Scope,
    /// `(label, is_referenced)` for each enclosing loop.
    loops: Vec<(String, bool)>,
    next: usize,
}

fn pick(names: &[String], i: u8) -> Option<String> {
    (!names.is_empty()).then(|| names[i as usize % names.len()].clone())
}

fn constant_time(a: &SAgg) -> bool {
    matches!(a, SAgg::Len | SAgg::Sum | SAgg::Product | SAgg::Min(_) | SAgg::Max(_) | SAgg::First(_) | SAgg::Last(_))
}

impl Resolver {
    fn fresh(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn with_ints<T>(&mut self, names: &[&str], f: impl FnOnce(&mut Self) -> T) -> T {
        let n = self.scope.ints.len();
        self.scope.ints.extend(names.iter().map(|s| s.to_string()));
        let r = f(self);
        self.scope.ints.truncate(n);
        r
    }

    fn int(&mut self, e: &SInt) -> E {
        match e {
            SInt::Lit(v) => E::Lit(*v),
            SInt::Var(i) => pick(&self.scope.ints, *i).map_or(E::Lit(*i as i64 % 7), E::Name),
            SInt::Bin(o, a, b) => E::Bin(*o, Box::new(self.int(a)), Box::new(self.int(b))),
            SInt::Neg(a) => E::Neg(Box::new(self.int(a))),
            SInt::If(c, a, b) => E::If(Box::new(self.cond(c)), Box::new(self.int(a)), Box::new(self.int(b))),
            SInt::Call(f, a, b) => match pick(&self.scope.fns, *f) {
                Some(f) => E::Call(f, Box::new(self.int(a)), Box::new(self.int(b))),
                None => E::Bin(Bin::Add, Box::new(self.int(a)), Box::new(self.int(b))),
            },
            SInt::Apply(g, a) => match pick(&self.scope.lams, *g) {
                Some(g) => E::Apply(g, Box::new(self.int(a))),
                None => self.int(a),
            },
            SInt::Agg(s, a) => E::Agg(Box::new(self.seq(s, constant_time(a))), Box::new(self.agg(a))),
            SInt::Match(s, arms, d) => {
                let s = self.int(s);
                // Every arm must be reachable.
                let mut kept: Vec<(i64, i64, E)> = Vec::new();
                for (lo, w, e) in arms {
                    let hi = lo + *w as i64;
                    if kept.iter().all(|&(a, b, _)| hi < a || b < *lo) {
                        let e = self.int(e);
                        kept.push((*lo, hi, e));
                    }
                }
                E::Match(Box::new(s), kept, Box::new(self.int(d)))
            }
            SInt::Index(l, i) => E::Index(pick(&self.scope.lists, *l).unwrap(), Box::new(self.int(i))),
        }
    }

    fn cond(&mut self, c: &SCond) -> C {
        match c {
            SCond::Cmp(o, a, b) => C::Cmp(*o, self.int(a), self.int(b)),
            SCond::Even(a) => C::Even(self.int(a)),
            SCond::Not(a) => C::Not(Box::new(self.cond(a))),
            SCond::And(a, b) => C::And(Box::new(self.cond(a)), Box::new(self.cond(b))),
            SCond::Or(a, b) => C::Or(Box::new(self.cond(a)), Box::new(self.cond(b))),
            SCond::Contains(s, e) => C::Contains(self.src(s, true), self.int(e)),
            SCond::Any(s, p) => C::Any(Box::new(self.seq(s, false)), Box::new(self.pred(p))),
            SCond::All(s, p) => C::All(Box::new(self.seq(s, false)), Box::new(self.pred(p))),
            SCond::Test(p, e) => match pick(&self.scope.preds, *p) {
                Some(p) => C::Test(p, self.int(e)),
                None => C::Even(self.int(e)),
            },
        }
    }

    fn lam(&mut self, l: &SLam) -> Lam {
        let param = self.fresh("x");
        self.with_ints(&[&param], |r| Lam {
            effect: l.effect,
            early: l.early.as_ref().map(|(c, e)| (r.cond(c), r.int(e))),
            body: r.int(&l.body),
            param: param.clone(),
        })
    }

    fn pred(&mut self, p: &SPred) -> Pred {
        let param = self.fresh("x");
        self.with_ints(&[&param], |r| Pred {
            effect: p.effect,
            early: p.early.as_ref().map(|(c, b)| (r.cond(c), *b)),
            body: r.cond(&p.body),
            param: param.clone(),
        })
    }

    fn filter_map(&mut self, f: &SFilterMap) -> FilterMap {
        let param = self.fresh("x");
        self.with_ints(&[&param], |r| FilterMap { effect: f.effect, cond: r.cond(&f.cond), value: r.int(&f.value), param: param.clone() })
    }

    fn lam2(&mut self, l: &SLam2, a: String, b: String) -> Lam2 {
        let body = self.with_ints(&[&a, &b], |r| r.int(&l.body));
        Lam2 { a, b, effect: l.effect, body }
    }

    fn fold_lam(&mut self, l: &SLam2) -> Lam2 {
        let (a, b) = (self.fresh("a"), self.fresh("x"));
        self.lam2(l, a, b)
    }

    fn src(&mut self, s: &SSrc, wide: bool) -> Src {
        match s {
            SSrc::List(l) => Src::List(pick(&self.scope.lists, *l).unwrap()),
            SSrc::Log => Src::List("log".into()),
            SSrc::Lit(xs) => Src::Lit(xs.iter().map(|x| self.int(x)).collect()),
            SSrc::Range { lo, width, inclusive } => {
                let k = *width as i64;
                match lo {
                    SLo::Var(v) if !self.scope.ints.is_empty() => {
                        let lo = E::Name(pick(&self.scope.ints, *v).unwrap());
                        Src::Range(lo.clone(), E::Bin(Bin::Add, Box::new(lo), Box::new(E::Lit(k))), *inclusive)
                    }
                    SLo::Var(v) => Src::Range(E::Lit(*v as i64 % 5), E::Lit(*v as i64 % 5 + k), *inclusive),
                    SLo::Lit(lo) => Src::Range(E::Lit(*lo), E::Lit(lo.saturating_add(k)), *inclusive),
                }
            }
            SSrc::RangeVar(r) => match pick(&self.scope.ranges, *r) {
                Some(r) => Src::RangeVar(r),
                None => Src::Range(E::Lit(0), E::Lit(*r as i64 % 6), false),
            },
            SSrc::Wide(a, b, i) if wide => Src::Range(E::Lit(*a), E::Lit(*b), *i),
            SSrc::Wide(a, _, i) => Src::Range(E::Lit(*a), E::Lit(a.saturating_add(3)), *i),
        }
    }

    fn seq(&mut self, s: &SSeq, wide: bool) -> Seq {
        let src = self.src(&s.src, wide && matches!(s.pipe, SPipe::None));
        let pipe = match &s.pipe {
            SPipe::None => Pipe::None,
            SPipe::Eager(ops) => Pipe::Eager(
                ops.iter()
                    .map(|op| match op {
                        SEager::Map(l) => Eager::Map(self.lam(l)),
                        SEager::Filter(p) => Eager::Filter(self.pred(p)),
                        SEager::FilterMap(f) => Eager::FilterMap(self.filter_map(f)),
                        SEager::TakeWhile(p) => Eager::TakeWhile(self.pred(p)),
                        SEager::SkipWhile(p) => Eager::SkipWhile(self.pred(p)),
                        SEager::Take(n) => Eager::Take(self.int(n)),
                        SEager::Skip(n) => Eager::Skip(self.int(n)),
                        SEager::Reversed => Eager::Reversed,
                        SEager::Sorted => Eager::Sorted,
                    })
                    .collect(),
            ),
            SPipe::Lazy(ops) => Pipe::Lazy(
                ops.iter()
                    .map(|op| match op {
                        SAdapter::Map(l) => Adapter::Map(self.lam(l)),
                        SAdapter::Filter(p) => Adapter::Filter(self.pred(p)),
                        SAdapter::FilterMap(f) => Adapter::FilterMap(self.filter_map(f)),
                        SAdapter::Take(n) => Adapter::Take(self.int(n)),
                        SAdapter::Skip(n) => Adapter::Skip(self.int(n)),
                        SAdapter::TakeWhile(p) => Adapter::TakeWhile(self.pred(p)),
                        SAdapter::SkipWhile(p) => Adapter::SkipWhile(self.pred(p)),
                        SAdapter::Inspect => Adapter::Inspect(self.fresh("x")),
                        SAdapter::EnumerateMap(l) => {
                            let p = self.fresh("p");
                            let lam = self.lam2(l, format!("{p}.0"), format!("{p}.1"));
                            Adapter::EnumerateMap(p, lam)
                        }
                        SAdapter::StepBy(k) => Adapter::StepBy(*k),
                    })
                    .collect(),
            ),
            SPipe::Comp { filter, map } => {
                let param = self.fresh("x");
                self.with_ints(&[&param], |r| {
                    Pipe::Comp(Box::new(Comp {
                        filter: filter.as_ref().map(|c| r.cond(c)),
                        map: map.as_ref().map(|(effect, e)| (*effect, r.int(e))),
                        param: param.clone(),
                    }))
                })
            }
        };
        Seq { src, pipe }
    }

    fn agg(&mut self, a: &SAgg) -> Agg {
        match a {
            SAgg::Len => Agg::Len,
            SAgg::Sum => Agg::Sum,
            SAgg::Product => Agg::Product,
            SAgg::Count(p) => Agg::Count(self.pred(p)),
            SAgg::Fold(i, f) => Agg::Fold(self.int(i), self.fold_lam(f)),
            SAgg::Reduce(f, d) => Agg::Reduce(self.fold_lam(f), *d),
            SAgg::Min(d) => Agg::Min(*d),
            SAgg::Max(d) => Agg::Max(*d),
            SAgg::First(d) => Agg::First(*d),
            SAgg::Last(d) => Agg::Last(*d),
            SAgg::Find(p, d) => Agg::Find(self.pred(p), *d),
            SAgg::FindIndex(p, d) => Agg::FindIndex(self.pred(p), *d),
        }
    }

    fn block(&mut self, ss: &[SStmt]) -> Vec<Stmt> {
        let saved = self.scope.clone();
        let out = ss.iter().filter_map(|s| self.stmt(s)).collect();
        self.scope = saved;
        out
    }

    fn loop_body(&mut self, vars: &[&str], body: &[SStmt]) -> (Option<String>, Vec<Stmt>) {
        let label = self.fresh("o");
        self.loops.push((label, false));
        let body = self.with_ints(vars, |r| r.block(body));
        let (label, used) = self.loops.pop().unwrap();
        (used.then_some(label), body)
    }

    fn declare(&mut self, prefix: &str, kind: fn(&mut Scope) -> &mut Vec<String>) -> String {
        let name = self.fresh(prefix);
        kind(&mut self.scope).push(name.clone());
        name
    }

    fn stmt(&mut self, s: &SStmt) -> Option<Stmt> {
        Some(match s {
            SStmt::Let { mutable, value } => {
                let value = self.int(value);
                let name = self.declare("v", |s| &mut s.ints);
                if *mutable {
                    self.scope.vars.push(name.clone());
                }
                Stmt::Let(name, *mutable, value)
            }
            SStmt::LetList(seq) => {
                let seq = self.seq(seq, false);
                Stmt::LetList(self.declare("l", |s| &mut s.lists), seq)
            }
            SStmt::LetAlias(l, copy) => {
                let of = pick(&self.scope.lists, *l)?;
                Stmt::LetAlias(self.declare("l", |s| &mut s.lists), of, *copy)
            }
            SStmt::LetRange(src) => {
                let src = match self.src(src, false) {
                    r @ Src::Range(..) => r,
                    _ => Src::Range(E::Lit(-2), E::Lit(3), true),
                };
                Stmt::LetRange(self.declare("r", |s| &mut s.ranges), src)
            }
            SStmt::LetLam(l) => {
                let lam = self.lam(l);
                Stmt::LetLam(self.declare("g", |s| &mut s.lams), lam)
            }
            SStmt::LetPred(p) => {
                let pred = self.pred(p);
                Stmt::LetPred(self.declare("q", |s| &mut s.preds), pred)
            }
            SStmt::PrintInt(e) => Stmt::PrintInt(self.int(e)),
            SStmt::PrintBool(c) => Stmt::PrintBool(self.cond(c)),
            SStmt::PrintList(seq) => Stmt::PrintList(self.seq(seq, false)),
            SStmt::Assign(v, add, e) => {
                let v = pick(&self.scope.vars, *v)?;
                Stmt::Assign(v, *add, self.int(e))
            }
            SStmt::Push(l, e) => {
                let l = pick(&self.scope.lists, *l).unwrap();
                Stmt::Push(l, self.int(e))
            }
            SStmt::Pop(l) => Stmt::Pop(pick(&self.scope.lists, *l)?),
            SStmt::If(c, t, f) => {
                let c = self.cond(c);
                Stmt::If(c, self.block(t), self.block(f))
            }
            SStmt::For(seq, body) => {
                let seq = self.seq(seq, false);
                let var = self.fresh("i");
                let (label, body) = self.loop_body(&[&var], body);
                Stmt::For { label, var, seq, body }
            }
            SStmt::ForEnumerate(l, body) => {
                let list = pick(&self.scope.lists, *l)?;
                let (i, x) = (self.fresh("i"), self.fresh("x"));
                let (label, body) = self.loop_body(&[&i, &x], body);
                Stmt::ForEnumerate { label, i, x, list, body }
            }
            SStmt::Break(c, d) | SStmt::Continue(c, d) => {
                if self.loops.is_empty() {
                    return None;
                }
                let c = self.cond(c);
                let depth = *d as usize % self.loops.len();
                let label = (depth > 0).then(|| {
                    let n = self.loops.len() - 1 - depth;
                    self.loops[n].1 = true;
                    self.loops[n].0.clone()
                });
                if matches!(s, SStmt::Break(..)) { Stmt::Break(c, label) } else { Stmt::Continue(c, label) }
            }
        })
    }

    fn program(p: &SProgram) -> Program {
        let mut r = Resolver::default();
        let mut fns = Vec::new();
        for (effect, body) in &p.fns {
            let (name, a, b) = (r.fresh("f"), r.fresh("a"), r.fresh("b"));
            let body = r.with_ints(&[&a, &b], |r| r.int(body));
            r.scope.fns.push(name.clone());
            fns.push(Function { name, a, b, effect: *effect, body });
        }
        let stmts = r.block(&p.stmts);
        Program { fns, stmts }
    }
}

// ---------------------------------------------------------------------------
// Printing as Sand.

fn show_e(e: &E) -> String {
    match e {
        E::Lit(v) => int_lit(*v),
        E::Name(n) => n.clone(),
        E::Bin(Bin::Min, a, b) => format!("({}).min({})", show_e(a), show_e(b)),
        E::Bin(Bin::Max, a, b) => format!("({}).max({})", show_e(a), show_e(b)),
        E::Bin(o, a, b) => {
            let op = match o {
                Bin::Add => "+",
                Bin::Sub => "-",
                Bin::Mul => "*",
                Bin::Div => "/",
                _ => "%",
            };
            format!("({} {op} {})", show_e(a), show_e(b))
        }
        E::Neg(a) => format!("(-{})", show_e(a)),
        E::If(c, a, b) => format!("(if {} {{ {} }} else {{ {} }})", show_c(c), show_e(a), show_e(b)),
        E::Call(f, a, b) => format!("{f}({}, {})", show_e(a), show_e(b)),
        E::Apply(g, a) => format!("{g}({})", show_e(a)),
        E::Agg(s, a) => show_agg(s, a),
        E::Match(s, arms, d) => {
            let mut out = format!("(match {} {{ ", show_e(s));
            for (lo, hi, e) in arms {
                let pat = if lo == hi { lo.to_string() } else { format!("{lo}..={hi}") };
                out.push_str(&format!("{pat} => {}, ", show_e(e)));
            }
            out.push_str(&format!("_ => {} }})", show_e(d)));
            out
        }
        E::Index(l, i) => format!("{l}[{}]", show_e(i)),
    }
}

fn show_c(c: &C) -> String {
    match c {
        C::Cmp(o, a, b) => {
            let op = match o {
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Eq => "==",
                Cmp::Ne => "!=",
            };
            format!("({} {op} {})", show_e(a), show_e(b))
        }
        C::Even(a) => format!("({} % 2 == 0)", show_e(a)),
        C::Not(a) => format!("(not {})", show_c(a)),
        C::And(a, b) => format!("({} and {})", show_c(a), show_c(b)),
        C::Or(a, b) => format!("({} or {})", show_c(a), show_c(b)),
        C::Contains(s, e) => format!("{}.contains({})", show_src(s, true), show_e(e)),
        C::Any(s, p) => format!("{}.any({})", show_recv(s), show_pred(p)),
        C::All(s, p) => format!("{}.all({})", show_recv(s), show_pred(p)),
        C::Test(p, e) => format!("{p}({})", show_e(e)),
    }
}

fn show_body(pushed: Option<&str>, early: Option<(String, String)>, tail: String) -> String {
    if pushed.is_none() && early.is_none() {
        return tail;
    }
    let mut out = String::from("{ ");
    if let Some(x) = pushed {
        out.push_str(&format!("push(log, {x}); "));
    }
    if let Some((c, v)) = early {
        out.push_str(&format!("if {c} {{ return {v} }}; "));
    }
    out.push_str(&tail);
    out.push_str(" }");
    out
}

fn show_lam(l: &Lam) -> String {
    let early = l.early.as_ref().map(|(c, e)| (show_c(c), show_e(e)));
    format!("{} => {}", l.param, show_body(l.effect.then_some(&l.param), early, show_e(&l.body)))
}

fn show_pred(p: &Pred) -> String {
    let early = p.early.as_ref().map(|(c, b)| (show_c(c), b.to_string()));
    format!("{} => {}", p.param, show_body(p.effect.then_some(&p.param), early, show_c(&p.body)))
}

fn show_filter_map(f: &FilterMap) -> String {
    let tail = format!("if {} {{ Some({}) }} else {{ None }}", show_c(&f.cond), show_e(&f.value));
    format!("{} => {}", f.param, show_body(f.effect.then_some(&f.param), None, tail))
}

fn show_lam2(l: &Lam2, params: &str) -> String {
    format!("{params} => {}", show_body(l.effect.then_some(&l.a), None, show_e(&l.body)))
}

fn show_src(s: &Src, paren: bool) -> String {
    match s {
        Src::List(l) | Src::RangeVar(l) => l.clone(),
        Src::Lit(xs) => format!("[{}]", xs.iter().map(show_e).collect::<Vec<_>>().join(", ")),
        Src::Range(lo, hi, incl) => {
            let r = format!("{}..{}{}", show_e(lo), if *incl { "=" } else { "" }, show_e(hi));
            if paren { format!("({r})") } else { r }
        }
    }
}

fn is_range(s: &Src) -> bool {
    matches!(s, Src::Range(..) | Src::RangeVar(_))
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    List,
    Range,
    Iter,
}

fn kind(s: &Seq) -> Kind {
    match &s.pipe {
        Pipe::Lazy(_) => Kind::Iter,
        Pipe::None if is_range(&s.src) => Kind::Range,
        _ => Kind::List,
    }
}

fn show_recv(s: &Seq) -> String {
    let mut out = show_src(&s.src, true);
    match &s.pipe {
        Pipe::None => {}
        Pipe::Eager(ops) => {
            let mut range = is_range(&s.src);
            for op in ops {
                if range && matches!(op, Eager::Take(_) | Eager::Skip(_) | Eager::Sorted) {
                    out.push_str(".to_list()");
                }
                range = false;
                out.push_str(&match op {
                    Eager::Map(l) => format!(".map({})", show_lam(l)),
                    Eager::Filter(p) => format!(".filter({})", show_pred(p)),
                    Eager::FilterMap(f) => format!(".filter_map({})", show_filter_map(f)),
                    Eager::TakeWhile(p) => format!(".take_while({})", show_pred(p)),
                    Eager::SkipWhile(p) => format!(".skip_while({})", show_pred(p)),
                    Eager::Take(n) => format!(".take({})", show_e(n)),
                    Eager::Skip(n) => format!(".skip({})", show_e(n)),
                    Eager::Reversed => ".reversed()".into(),
                    Eager::Sorted => ".sorted()".into(),
                });
            }
        }
        Pipe::Lazy(ops) => {
            out.push_str(".iter()");
            for op in ops {
                out.push_str(&match op {
                    Adapter::Map(l) => format!(".map({})", show_lam(l)),
                    Adapter::Filter(p) => format!(".filter({})", show_pred(p)),
                    Adapter::FilterMap(f) => format!(".filter_map({})", show_filter_map(f)),
                    Adapter::Take(n) => format!(".take({})", show_e(n)),
                    Adapter::Skip(n) => format!(".skip({})", show_e(n)),
                    Adapter::TakeWhile(p) => format!(".take_while({})", show_pred(p)),
                    Adapter::SkipWhile(p) => format!(".skip_while({})", show_pred(p)),
                    Adapter::Inspect(x) => format!(".inspect({x} => push(log, {x}))"),
                    Adapter::EnumerateMap(p, l) => format!(".enumerate().map({})", show_lam2(l, p)),
                    Adapter::StepBy(k) => format!(".step_by({k})"),
                });
            }
        }
        Pipe::Comp(comp) => {
            let Comp { param, filter, map } = &**comp;
            let value = match map {
                Some((effect, e)) => show_body(effect.then_some(param), None, show_e(e)),
                None => param.clone(),
            };
            let filter = filter.as_ref().map_or(String::new(), |c| format!(" if {}", show_c(c)));
            out = format!("[{value} for {param} in {}{filter}]", show_src(&s.src, false));
        }
    }
    out
}

fn show_list(s: &Seq) -> String {
    match kind(s) {
        Kind::List => show_recv(s),
        Kind::Range | Kind::Iter => format!("{}.to_list()", show_recv(s)),
    }
}

fn show_agg(s: &Seq, a: &Agg) -> String {
    let recv = show_recv(s);
    let iter = kind(s) == Kind::Iter;
    let or = |call: String, d: i64| format!("({recv}{call} ?? {})", int_lit(d));
    let lam2 = |f: &Lam2| show_lam2(f, &format!("({}, {})", f.a, f.b));
    match a {
        Agg::Len if iter => format!("{recv}.count()"),
        Agg::Len => format!("{recv}.len()"),
        Agg::Sum => format!("{recv}.sum()"),
        Agg::Product => format!("{recv}.product()"),
        Agg::Count(p) if iter => format!("{recv}.filter({}).count()", show_pred(p)),
        Agg::Count(p) => format!("{recv}.count({})", show_pred(p)),
        Agg::Fold(init, f) => format!("{recv}.fold({}, {})", show_e(init), lam2(f)),
        Agg::Reduce(f, d) => or(format!(".reduce({})", lam2(f)), *d),
        Agg::Min(d) => or(".min()".into(), *d),
        Agg::Max(d) => or(".max()".into(), *d),
        Agg::First(d) => or(".first()".into(), *d),
        Agg::Last(d) => or(".last()".into(), *d),
        Agg::Find(p, d) => or(format!(".find({})", show_pred(p)), *d),
        Agg::FindIndex(p, d) => or(format!(".find_index({})", show_pred(p)), *d),
    }
}

fn show_label(label: &Option<String>) -> String {
    label.as_ref().map_or(String::new(), |l| format!("@{l} "))
}

fn show_stmts(ss: &[Stmt], indent: usize, out: &mut String) {
    let pad = "    ".repeat(indent);
    for s in ss {
        let line = match s {
            Stmt::Let(n, mutable, e) => format!("{} {n} = {}", if *mutable { "var" } else { "let" }, show_e(e)),
            Stmt::LetList(n, seq) => format!("let {n}: [int] = {}", show_list(seq)),
            Stmt::LetAlias(n, of, copy) => format!("let {n} = {of}{}", if *copy { ".copy()" } else { "" }),
            Stmt::LetRange(n, src) => format!("let {n} = {}", show_src(src, false)),
            Stmt::LetLam(n, l) => format!("let {n}: fn(int) -> int = {}", show_lam(l)),
            Stmt::LetPred(n, p) => format!("let {n}: fn(int) -> bool = {}", show_pred(p)),
            Stmt::PrintInt(e) => format!("print({})", show_e(e)),
            Stmt::PrintBool(c) => format!("print({})", show_c(c)),
            Stmt::PrintList(seq) => format!("print({})", show_list(seq)),
            Stmt::Assign(v, add, e) => format!("{v} {}= {}", if *add { "+" } else { "" }, show_e(e)),
            Stmt::Push(l, e) => format!("push({l}, {})", show_e(e)),
            Stmt::Pop(l) => format!("let _ = {l}.pop()"),
            Stmt::If(c, t, f) => {
                out.push_str(&format!("{pad}if {} {{\n", show_c(c)));
                show_stmts(t, indent + 1, out);
                if !f.is_empty() {
                    out.push_str(&format!("{pad}}} else {{\n"));
                    show_stmts(f, indent + 1, out);
                }
                out.push_str(&format!("{pad}}}\n"));
                continue;
            }
            Stmt::For { label, var, seq, body } => {
                let iter = match seq.pipe {
                    Pipe::None => show_src(&seq.src, false),
                    _ => show_recv(seq),
                };
                out.push_str(&format!("{pad}{}for {var} in {iter} {{\n", show_label(label)));
                show_stmts(body, indent + 1, out);
                out.push_str(&format!("{pad}}}\n"));
                continue;
            }
            Stmt::ForEnumerate { label, i, x, list, body } => {
                out.push_str(&format!("{pad}{}for ({i}, {x}) in {list}.enumerate() {{\n", show_label(label)));
                show_stmts(body, indent + 1, out);
                out.push_str(&format!("{pad}}}\n"));
                continue;
            }
            Stmt::Break(c, l) | Stmt::Continue(c, l) => {
                let word = if matches!(s, Stmt::Break(..)) { "break" } else { "continue" };
                let label = l.as_ref().map_or(String::new(), |l| format!(" @{l}"));
                format!("if {} {{ {word}{label} }}", show_c(c))
            }
        };
        out.push_str(&format!("{pad}{line}\n"));
    }
}

fn show_program(p: &Program) -> String {
    let mut out =
        format!("let log: [int] = []\nfn push(xs: [int], x: int) {{\n    if xs.len() < {CAP} {{\n        xs.push(x)\n    }}\n}}\n");
    for f in &p.fns {
        let push = if f.effect { format!("    push(log, {})\n", f.a) } else { String::new() };
        out.push_str(&format!("fn {}({}: int, {}: int) -> int {{\n{push}    {}\n}}\n", f.name, f.a, f.b, show_e(&f.body)));
    }
    show_stmts(&p.stmts, 0, &mut out);
    out.push_str("print(log)\n");
    out
}

const CAP: usize = 100;

type Fail = &'static str;
const OVERFLOW: Fail = "integer overflow";

#[derive(Clone, Copy)]
enum Val<'p> {
    Int(i64),
    List(usize),
    Range(i64, i64, bool),
    Lam(&'p Lam),
    Pred(&'p Pred),
}

enum Flow {
    Next,
    Break(Option<String>),
    Continue(Option<String>),
}

enum Recv<'p> {
    List(usize),
    Range(i64, i64, bool),
    Iter(Stream<'p>),
}

fn bounds(a: i64, b: i64, inclusive: bool) -> Option<(i64, i64)> {
    let last = if inclusive { b } else { b.checked_sub(1)? };
    (a <= last).then_some((a, last))
}

fn floor_div(a: i64, b: i64) -> Result<i64, Fail> {
    if b == 0 {
        return Err("division by zero");
    }
    let q = a.checked_div(b).ok_or(OVERFLOW)?;
    Ok(if a % b != 0 && ((a < 0) != (b < 0)) { q - 1 } else { q })
}

fn floor_mod(a: i64, b: i64) -> Result<i64, Fail> {
    if b == 0 {
        return Err("division by zero");
    }
    let r = a.checked_rem(b).unwrap_or(0);
    Ok(if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r })
}

fn exact(n: i128) -> Result<i64, Fail> {
    i64::try_from(n).map_err(|_| OVERFLOW)
}

struct Model<'p> {
    env: HashMap<String, Val<'p>>,
    fns: HashMap<&'p str, &'p Function>,
    lists: Vec<Vec<i64>>,
    out: String,
    steps: u64,
}

const MAX_STEPS: u64 = 200_000;
const GC_STRESS_STEPS: u64 = 2_000;
const TOO_LONG: Fail = "(the model gave up)";

#[derive(Clone, Copy)]
enum Source {
    List { id: usize, i: usize, n: usize },
    Range(Option<(i64, i64)>),
}

#[derive(Clone, Copy)]
enum Stage<'p> {
    Map(&'p Lam),
    Filter(&'p Pred),
    FilterMap(&'p FilterMap),
    Take(i64),
    Skip(i64),
    TakeWhile(&'p Pred, bool),
    SkipWhile(&'p Pred, bool),
    Inspect,
    Enumerate(&'p Lam2, i64),
    StepBy(i64, bool),
}

struct Stream<'p> {
    src: Source,
    stages: Vec<Stage<'p>>,
}

impl<'p> Stream<'p> {
    fn next(&mut self, m: &mut Model<'p>) -> Result<Option<i64>, Fail> {
        self.pull(m, self.stages.len())
    }

    fn pull(&mut self, m: &mut Model<'p>, k: usize) -> Result<Option<i64>, Fail> {
        if k == 0 {
            m.step()?;
            return Ok(match &mut self.src {
                Source::List { id, i, n } => match m.lists[*id].get(*i) {
                    Some(&x) if *i < *n => {
                        *i += 1;
                        Some(x)
                    }
                    _ => {
                        *i = *n;
                        None
                    }
                },
                Source::Range(state) => state.take().map(|(x, last)| {
                    if x < last {
                        *state = Some((x + 1, last));
                    }
                    x
                }),
            });
        }
        let below = k - 1;
        Ok(match self.stages[below] {
            Stage::Map(l) => match self.pull(m, below)? {
                Some(x) => Some(m.call_lam(l, x)?),
                None => None,
            },
            Stage::Filter(p) => loop {
                match self.pull(m, below)? {
                    Some(x) if !m.call_pred(p, x)? => {}
                    r => break r,
                }
            },
            Stage::FilterMap(f) => loop {
                match self.pull(m, below)? {
                    Some(x) => {
                        if let Some(y) = m.call_filter_map(f, x)? {
                            break Some(y);
                        }
                    }
                    None => break None,
                }
            },
            Stage::Take(left) => {
                if left <= 0 {
                    return Ok(None);
                }
                self.stages[below] = Stage::Take(left - 1);
                self.pull(m, below)?
            }
            Stage::Skip(left) => {
                for rest in (0..left.max(0)).rev() {
                    self.stages[below] = Stage::Skip(rest);
                    if self.pull(m, below)?.is_none() {
                        return Ok(None);
                    }
                }
                self.pull(m, below)?
            }
            Stage::TakeWhile(p, done) => {
                if done {
                    return Ok(None);
                }
                match self.pull(m, below)? {
                    Some(x) if m.call_pred(p, x)? => Some(x),
                    _ => {
                        self.stages[below] = Stage::TakeWhile(p, true);
                        None
                    }
                }
            }
            Stage::SkipWhile(p, skipping) => {
                if !skipping {
                    return self.pull(m, below);
                }
                self.stages[below] = Stage::SkipWhile(p, false);
                loop {
                    match self.pull(m, below)? {
                        Some(x) if m.call_pred(p, x)? => {}
                        r => break r,
                    }
                }
            }
            Stage::Inspect => {
                let r = self.pull(m, below)?;
                if let Some(x) = r {
                    m.push(0, x);
                }
                r
            }
            Stage::Enumerate(l, i) => match self.pull(m, below)? {
                Some(x) => {
                    self.stages[below] = Stage::Enumerate(l, i + 1);
                    Some(m.call_lam2(l, i, x)?)
                }
                None => None,
            },
            Stage::StepBy(step, first) => {
                if first {
                    self.stages[below] = Stage::StepBy(step, false);
                } else {
                    for _ in 1..step {
                        if self.pull(m, below)?.is_none() {
                            return Ok(None);
                        }
                    }
                }
                self.pull(m, below)?
            }
        })
    }
}

impl<'p> Model<'p> {
    /// Evaluates `p` and returns `(Run, steps)`, or `None` if execution
    /// exceeds `MAX_STEPS`.
    fn run(p: &'p Program) -> Option<(Run, u64)> {
        let mut m = Model {
            env: HashMap::new(),
            fns: p.fns.iter().map(|f| (f.name.as_str(), f)).collect(),
            lists: vec![vec![]],
            out: String::new(),
            steps: 0,
        };
        m.env.insert("log".into(), Val::List(0));
        let failure = match m.block(&p.stmts) {
            Ok(_) => {
                m.out.push_str(&format!("{}\n", list_str(&m.lists[0])));
                None
            }
            Err(f) => Some(f.to_string()),
        };
        if failure.as_deref() == Some(TOO_LONG) {
            return None;
        }
        Some((Run { stdout: m.out, failure }, m.steps))
    }

    fn step(&mut self) -> Result<(), Fail> {
        self.steps += 1;
        if self.steps > MAX_STEPS { Err(TOO_LONG) } else { Ok(()) }
    }

    fn get(&self, name: &str) -> Val<'p> {
        *self.env.get(name).unwrap_or_else(|| panic!("unbound {name}"))
    }

    fn int_of(&self, name: &str) -> i64 {
        match self.get(name) {
            Val::Int(v) => v,
            _ => panic!("{name} is not an int"),
        }
    }

    fn list_of(&self, name: &str) -> usize {
        match self.get(name) {
            Val::List(id) => id,
            _ => panic!("{name} is not a list"),
        }
    }

    fn push(&mut self, id: usize, x: i64) {
        if self.lists[id].len() < CAP {
            self.lists[id].push(x);
        }
    }

    fn new_list(&mut self, xs: Vec<i64>) -> usize {
        self.lists.push(xs);
        self.lists.len() - 1
    }

    fn with<T>(&mut self, bindings: &[(&str, i64)], f: impl FnOnce(&mut Self) -> Result<T, Fail>) -> Result<T, Fail> {
        self.step()?;
        let saved: Vec<_> = bindings.iter().map(|(n, v)| (n.to_string(), self.env.insert(n.to_string(), Val::Int(*v)))).collect();
        let r = f(self);
        for (n, old) in saved {
            match old {
                Some(v) => self.env.insert(n, v),
                None => self.env.remove(&n),
            };
        }
        r
    }

    fn call_lam(&mut self, l: &'p Lam, x: i64) -> Result<i64, Fail> {
        self.with(&[(&l.param, x)], |m| {
            if l.effect {
                m.push(0, x);
            }
            if let Some((c, e)) = &l.early
                && m.cond(c)?
            {
                return m.int(e);
            }
            m.int(&l.body)
        })
    }

    fn call_pred(&mut self, p: &'p Pred, x: i64) -> Result<bool, Fail> {
        self.with(&[(&p.param, x)], |m| {
            if p.effect {
                m.push(0, x);
            }
            if let Some((c, b)) = &p.early
                && m.cond(c)?
            {
                return Ok(*b);
            }
            m.cond(&p.body)
        })
    }

    fn call_filter_map(&mut self, f: &'p FilterMap, x: i64) -> Result<Option<i64>, Fail> {
        self.with(&[(&f.param, x)], |m| {
            if f.effect {
                m.push(0, x);
            }
            if m.cond(&f.cond)? { m.int(&f.value).map(Some) } else { Ok(None) }
        })
    }

    fn call_lam2(&mut self, l: &'p Lam2, a: i64, b: i64) -> Result<i64, Fail> {
        self.with(&[(&l.a, a), (&l.b, b)], |m| {
            if l.effect {
                m.push(0, a);
            }
            m.int(&l.body)
        })
    }

    fn int(&mut self, e: &'p E) -> Result<i64, Fail> {
        Ok(match e {
            E::Lit(v) => *v,
            E::Name(n) => self.int_of(n),
            E::Bin(o, a, b) => {
                let (a, b) = (self.int(a)?, self.int(b)?);
                match o {
                    Bin::Add => a.checked_add(b).ok_or(OVERFLOW)?,
                    Bin::Sub => a.checked_sub(b).ok_or(OVERFLOW)?,
                    Bin::Mul => a.checked_mul(b).ok_or(OVERFLOW)?,
                    Bin::Div => floor_div(a, b)?,
                    Bin::Mod => floor_mod(a, b)?,
                    Bin::Min => a.min(b),
                    Bin::Max => a.max(b),
                }
            }
            E::Neg(a) => self.int(a)?.checked_neg().ok_or(OVERFLOW)?,
            E::If(c, a, b) => {
                if self.cond(c)? {
                    self.int(a)?
                } else {
                    self.int(b)?
                }
            }
            E::Call(f, a, b) => {
                let (a, b) = (self.int(a)?, self.int(b)?);
                let f = self.fns[f.as_str()];
                self.with(&[(&f.a, a), (&f.b, b)], |m| {
                    if f.effect {
                        m.push(0, a);
                    }
                    m.int(&f.body)
                })?
            }
            E::Apply(g, a) => {
                let Val::Lam(l) = self.get(g) else { panic!("{g} is not a lambda") };
                let a = self.int(a)?;
                self.call_lam(l, a)?
            }
            E::Agg(s, a) => self.agg(s, a)?,
            E::Match(s, arms, d) => {
                let v = self.int(s)?;
                match arms.iter().find(|(lo, hi, _)| *lo <= v && v <= *hi) {
                    Some((_, _, e)) => self.int(e)?,
                    None => self.int(d)?,
                }
            }
            E::Index(l, i) => {
                let id = self.list_of(l);
                let i = self.int(i)?;
                *usize::try_from(i).ok().and_then(|i| self.lists[id].get(i)).ok_or("index out of bounds")?
            }
        })
    }

    fn cond(&mut self, c: &'p C) -> Result<bool, Fail> {
        Ok(match c {
            C::Cmp(o, a, b) => {
                let (a, b) = (self.int(a)?, self.int(b)?);
                match o {
                    Cmp::Lt => a < b,
                    Cmp::Le => a <= b,
                    Cmp::Eq => a == b,
                    Cmp::Ne => a != b,
                }
            }
            C::Even(a) => self.int(a)?.rem_euclid(2) == 0,
            C::Not(a) => !self.cond(a)?,
            C::And(a, b) => self.cond(a)? && self.cond(b)?,
            C::Or(a, b) => self.cond(a)? || self.cond(b)?,
            C::Contains(s, e) => {
                let r = self.src(s)?;
                let v = self.int(e)?;
                match r {
                    Recv::List(id) => self.lists[id].contains(&v),
                    Recv::Range(a, b, i) => bounds(a, b, i).is_some_and(|(a, b)| a <= v && v <= b),
                    Recv::Iter(_) => unreachable!(),
                }
            }
            C::Any(s, p) | C::All(s, p) => {
                let any = matches!(c, C::Any(..));
                let mut r = self.recv(s)?;
                let mut found = false;
                self.each(&mut r, &mut |m, x| {
                    if m.call_pred(p, x)? == any {
                        found = true;
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                if any { found } else { !found }
            }
            C::Test(q, e) => {
                let Val::Pred(p) = self.get(q) else { panic!("{q} is not a predicate") };
                let v = self.int(e)?;
                self.call_pred(p, v)?
            }
        })
    }

    fn src(&mut self, s: &'p Src) -> Result<Recv<'p>, Fail> {
        Ok(match s {
            Src::List(l) => Recv::List(self.list_of(l)),
            Src::Lit(xs) => {
                let mut v = Vec::new();
                for x in xs {
                    v.push(self.int(x)?);
                }
                Recv::List(self.new_list(v))
            }
            Src::Range(lo, hi, i) => {
                let lo = self.int(lo)?;
                Recv::Range(lo, self.int(hi)?, *i)
            }
            Src::RangeVar(r) => match self.get(r) {
                Val::Range(a, b, i) => Recv::Range(a, b, i),
                _ => panic!("{r} is not a range"),
            },
        })
    }

    /// Calls `f` on each element of `r` until `f` returns `false`. For lists,
    /// iterates up to the initial list length and stops early if the list
    /// shrinks.
    fn each(&mut self, r: &mut Recv<'p>, f: &mut dyn FnMut(&mut Self, i64) -> Result<bool, Fail>) -> Result<(), Fail> {
        match r {
            Recv::List(id) => {
                let id = *id;
                let n = self.lists[id].len();
                for i in 0..n {
                    self.step()?;
                    let Some(&x) = self.lists[id].get(i) else { break };
                    if !f(self, x)? {
                        break;
                    }
                }
            }
            Recv::Range(a, b, incl) => {
                if let Some((first, last)) = bounds(*a, *b, *incl) {
                    for x in first..=last {
                        self.step()?;
                        if !f(self, x)? {
                            break;
                        }
                    }
                }
            }
            Recv::Iter(s) => {
                while let Some(x) = s.next(self)? {
                    if !f(self, x)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn collect(&mut self, r: &mut Recv<'p>) -> Result<Vec<i64>, Fail> {
        let mut out = Vec::new();
        self.each(r, &mut |_, x| {
            out.push(x);
            Ok(true)
        })?;
        Ok(out)
    }

    fn recv(&mut self, s: &'p Seq) -> Result<Recv<'p>, Fail> {
        let mut r = self.src(&s.src)?;
        match &s.pipe {
            Pipe::None => {}
            Pipe::Eager(ops) => {
                for op in ops {
                    let mut out = Vec::new();
                    match op {
                        Eager::Map(l) => self.each(&mut r, &mut |m, x| {
                            out.push(m.call_lam(l, x)?);
                            Ok(true)
                        })?,
                        Eager::Filter(p) => self.each(&mut r, &mut |m, x| {
                            if m.call_pred(p, x)? {
                                out.push(x);
                            }
                            Ok(true)
                        })?,
                        Eager::FilterMap(f) => self.each(&mut r, &mut |m, x| {
                            out.extend(m.call_filter_map(f, x)?);
                            Ok(true)
                        })?,
                        Eager::TakeWhile(p) => self.each(&mut r, &mut |m, x| {
                            let more = m.call_pred(p, x)?;
                            if more {
                                out.push(x);
                            }
                            Ok(more)
                        })?,
                        Eager::SkipWhile(p) => {
                            let mut skipping = true;
                            self.each(&mut r, &mut |m, x| {
                                if skipping && m.call_pred(p, x)? {
                                    return Ok(true);
                                }
                                skipping = false;
                                out.push(x);
                                Ok(true)
                            })?
                        }
                        Eager::Take(n) | Eager::Skip(n) => {
                            let n = self.int(n)?;
                            let xs = self.collect(&mut r)?;
                            let n = n.clamp(0, xs.len() as i64) as usize;
                            out = if matches!(op, Eager::Take(_)) { xs[..n].to_vec() } else { xs[n..].to_vec() };
                        }
                        Eager::Reversed => {
                            out = self.collect(&mut r)?;
                            out.reverse();
                        }
                        Eager::Sorted => {
                            out = self.collect(&mut r)?;
                            out.sort();
                        }
                    }
                    r = Recv::List(self.new_list(out));
                }
            }
            Pipe::Lazy(ops) => {
                let src = match r {
                    Recv::List(id) => Source::List { id, i: 0, n: self.lists[id].len() },
                    Recv::Range(a, b, i) => Source::Range(bounds(a, b, i)),
                    Recv::Iter(_) => unreachable!(),
                };
                let mut stages = Vec::new();
                for op in ops {
                    stages.push(match op {
                        Adapter::Map(l) => Stage::Map(l),
                        Adapter::Filter(p) => Stage::Filter(p),
                        Adapter::FilterMap(f) => Stage::FilterMap(f),
                        Adapter::Take(n) => Stage::Take(self.int(n)?),
                        Adapter::Skip(n) => Stage::Skip(self.int(n)?),
                        Adapter::TakeWhile(p) => Stage::TakeWhile(p, false),
                        Adapter::SkipWhile(p) => Stage::SkipWhile(p, true),
                        Adapter::Inspect(_) => Stage::Inspect,
                        Adapter::EnumerateMap(_, l) => Stage::Enumerate(l, 0),
                        Adapter::StepBy(k) if *k <= 0 => return Err("step must be positive"),
                        Adapter::StepBy(k) => Stage::StepBy(*k, true),
                    });
                }
                r = Recv::Iter(Stream { src, stages });
            }
            Pipe::Comp(comp) => {
                let Comp { param, filter, map } = &**comp;
                let mut out = Vec::new();
                self.each(&mut r, &mut |m, x| {
                    m.with(&[(param, x)], |m| {
                        if let Some(c) = filter
                            && !m.cond(c)?
                        {
                            return Ok(true);
                        }
                        out.push(match map {
                            Some((effect, e)) => {
                                if *effect {
                                    m.push(0, x);
                                }
                                m.int(e)?
                            }
                            None => x,
                        });
                        Ok(true)
                    })
                })?;
                r = Recv::List(self.new_list(out));
            }
        }
        Ok(r)
    }

    fn list(&mut self, s: &'p Seq) -> Result<usize, Fail> {
        match self.recv(s)? {
            Recv::List(id) => Ok(id),
            mut r => {
                let xs = self.collect(&mut r)?;
                Ok(self.new_list(xs))
            }
        }
    }

    fn agg(&mut self, s: &'p Seq, a: &'p Agg) -> Result<i64, Fail> {
        let mut r = self.recv(s)?;
        if let Recv::Range(lo, hi, incl) = r {
            let b = bounds(lo, hi, incl);
            let len = b.map_or(0, |(a, b)| b as i128 - a as i128 + 1);
            match a {
                Agg::Len => return exact(len),
                Agg::Sum => return exact(b.map_or(0, |(a, b)| (a as i128 + b as i128) * len / 2)),
                Agg::Product => {
                    return match b {
                        None => Ok(1),
                        Some((a, b)) if a <= 0 && 0 <= b => Ok(0),
                        Some(_) if len > 64 => Err(OVERFLOW),
                        Some((a, b)) => (a..=b).try_fold(1i64, |acc, x| acc.checked_mul(x)).ok_or(OVERFLOW),
                    };
                }
                Agg::Min(d) | Agg::First(d) => return Ok(b.map_or(*d, |b| b.0)),
                Agg::Max(d) | Agg::Last(d) => return Ok(b.map_or(*d, |b| b.1)),
                _ => {}
            }
        }
        Ok(match a {
            Agg::Len => self.collect(&mut r)?.len() as i64,
            Agg::Sum | Agg::Product => {
                let mut acc = if matches!(a, Agg::Sum) { 0i64 } else { 1 };
                self.each(&mut r, &mut |_, x| {
                    acc = if matches!(a, Agg::Sum) { acc.checked_add(x) } else { acc.checked_mul(x) }.ok_or(OVERFLOW)?;
                    Ok(true)
                })?;
                acc
            }
            Agg::Count(p) => {
                let mut n = 0;
                self.each(&mut r, &mut |m, x| {
                    n += m.call_pred(p, x)? as i64;
                    Ok(true)
                })?;
                n
            }
            Agg::Fold(init, f) => {
                let mut acc = self.int(init)?;
                self.each(&mut r, &mut |m, x| {
                    acc = m.call_lam2(f, acc, x)?;
                    Ok(true)
                })?;
                acc
            }
            Agg::Reduce(f, d) => {
                let mut acc: Option<i64> = None;
                self.each(&mut r, &mut |m, x| {
                    acc = Some(match acc {
                        None => x,
                        Some(a) => m.call_lam2(f, a, x)?,
                    });
                    Ok(true)
                })?;
                acc.unwrap_or(*d)
            }
            Agg::Min(d) => self.collect(&mut r)?.iter().copied().reduce(|a, x| if x < a { x } else { a }).unwrap_or(*d),
            Agg::Max(d) => self.collect(&mut r)?.iter().copied().reduce(|a, x| if x > a { x } else { a }).unwrap_or(*d),
            Agg::First(d) => {
                let mut first = None;
                self.each(&mut r, &mut |_, x| {
                    first = Some(x);
                    Ok(false)
                })?;
                first.unwrap_or(*d)
            }
            Agg::Last(d) => self.collect(&mut r)?.last().copied().unwrap_or(*d),
            Agg::Find(p, d) => {
                let mut found = None;
                self.each(&mut r, &mut |m, x| {
                    if m.call_pred(p, x)? {
                        found = Some(x);
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                found.unwrap_or(*d)
            }
            Agg::FindIndex(p, d) => {
                let (mut i, mut found) = (0, None);
                self.each(&mut r, &mut |m, x| {
                    if m.call_pred(p, x)? {
                        found = Some(i);
                        return Ok(false);
                    }
                    i += 1;
                    Ok(true)
                })?;
                found.unwrap_or(*d)
            }
        })
    }

    fn block(&mut self, ss: &'p [Stmt]) -> Result<Flow, Fail> {
        for s in ss {
            match self.stmt(s)? {
                Flow::Next => {}
                flow => return Ok(flow),
            }
        }
        Ok(Flow::Next)
    }

    fn iteration(&mut self, label: &Option<String>, body: &'p [Stmt]) -> Result<Result<bool, Flow>, Fail> {
        Ok(match self.block(body)? {
            Flow::Next => Ok(true),
            Flow::Break(l) if l.is_none() || l == *label => Ok(false),
            Flow::Continue(l) if l.is_none() || l == *label => Ok(true),
            flow => Err(flow),
        })
    }

    fn stmt(&mut self, s: &'p Stmt) -> Result<Flow, Fail> {
        match s {
            Stmt::Let(n, _, e) => {
                let v = self.int(e)?;
                self.env.insert(n.clone(), Val::Int(v));
            }
            Stmt::LetList(n, seq) => {
                let id = self.list(seq)?;
                self.env.insert(n.clone(), Val::List(id));
            }
            Stmt::LetAlias(n, of, copy) => {
                let id = self.list_of(of);
                let id = if *copy { self.new_list(self.lists[id].clone()) } else { id };
                self.env.insert(n.clone(), Val::List(id));
            }
            Stmt::LetRange(n, src) => {
                let Recv::Range(a, b, i) = self.src(src)? else { unreachable!() };
                self.env.insert(n.clone(), Val::Range(a, b, i));
            }
            Stmt::LetLam(n, l) => {
                self.env.insert(n.clone(), Val::Lam(l));
            }
            Stmt::LetPred(n, p) => {
                self.env.insert(n.clone(), Val::Pred(p));
            }
            Stmt::PrintInt(e) => {
                let v = self.int(e)?;
                self.out.push_str(&format!("{v}\n"));
            }
            Stmt::PrintBool(c) => {
                let v = self.cond(c)?;
                self.out.push_str(&format!("{v}\n"));
            }
            Stmt::PrintList(seq) => {
                let id = self.list(seq)?;
                self.out.push_str(&format!("{}\n", list_str(&self.lists[id])));
            }
            Stmt::Assign(v, add, e) => {
                let x = self.int(e)?;
                let x = if *add { self.int_of(v).checked_add(x).ok_or(OVERFLOW)? } else { x };
                self.env.insert(v.clone(), Val::Int(x));
            }
            Stmt::Push(l, e) => {
                let id = self.list_of(l);
                let x = self.int(e)?;
                self.push(id, x);
            }
            Stmt::Pop(l) => {
                let id = self.list_of(l);
                self.lists[id].pop();
            }
            Stmt::If(c, t, f) => return if self.cond(c)? { self.block(t) } else { self.block(f) },
            Stmt::For { label, var, seq, body } => {
                let mut r = self.recv(seq)?;
                let mut leave = None;
                self.each(&mut r, &mut |m, x| {
                    m.env.insert(var.clone(), Val::Int(x));
                    match m.iteration(label, body)? {
                        Ok(more) => Ok(more),
                        Err(flow) => {
                            leave = Some(flow);
                            Ok(false)
                        }
                    }
                })?;
                if let Some(flow) = leave {
                    return Ok(flow);
                }
            }
            Stmt::ForEnumerate { label, i, x, list, body } => {
                let pairs = self.lists[self.list_of(list)].clone();
                for (k, v) in pairs.into_iter().enumerate() {
                    self.step()?;
                    self.env.insert(i.clone(), Val::Int(k as i64));
                    self.env.insert(x.clone(), Val::Int(v));
                    match self.iteration(label, body)? {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(flow) => return Ok(flow),
                    }
                }
            }
            Stmt::Break(c, l) => {
                if self.cond(c)? {
                    return Ok(Flow::Break(l.clone()));
                }
            }
            Stmt::Continue(c, l) => {
                if self.cond(c)? {
                    return Ok(Flow::Continue(l.clone()));
                }
            }
        }
        Ok(Flow::Next)
    }
}

fn matches(got: &Run, want: &Run) -> bool {
    got.stdout == want.stdout
        && match (&got.failure, &want.failure) {
            (None, None) => true,
            (Some(g), Some(w)) => g.contains(w.as_str()),
            _ => false,
        }
}

proptest! {
    #[test]
    fn generated_programs_match_the_model(p in program()) {
        let p = Resolver::program(&p);
        let src = show_program(&p);
        let Some((want, steps)) = Model::run(&p) else { return Ok(()) };
        let modes: &[Mode] = if steps <= GC_STRESS_STEPS {
            &[Mode::Optimized, Mode::Unoptimized, Mode::GcStress]
        } else {
            &[Mode::Optimized, Mode::Unoptimized]
        };
        let got = run_modes(&src, modes)?;
        prop_assert!(matches(&got, &want), "program:\n{src}\nexpected: {want:?}\ngot: {got:?}");
    }

    #[test]
    fn formatter_is_idempotent_and_preserves_meaning(p in program()) {
        let p = Resolver::program(&p);
        let src = show_program(&p);
        let formatted = sandlang::fmt::format_source(&src).unwrap();
        let again = sandlang::fmt::format_source(&formatted).unwrap();
        if let Some((i, (a, b))) = formatted.lines().zip(again.lines()).enumerate().find(|(_, (a, b))| a != b) {
            prop_assert!(false, "formatting again changes line {}:\n{a}\n{b}\nprogram:\n{src}\nformatted:\n{formatted}", i + 1);
        }
        prop_assert_eq!(&again, &formatted);
        let Some((want, _)) = Model::run(&p) else { return Ok(()) };
        let got = super::run(&formatted)?;
        prop_assert!(matches(&got, &want), "formatted:\n{formatted}\nexpected: {want:?}\ngot: {got:?}");
    }
}
