//! Fusion of lazy iterator pipelines (spec §12.13).
//!
//! An `Iter` pipeline such as `xs.iter().map(f).filter(g)` costs a closure
//! call per adapter and element (the source's `next`, `map`'s and
//! `filter`'s), and every element is wrapped in `Some` at each level. When
//! the whole pipeline is visible where it is consumed — as the iterable of
//! a `for` loop or comprehension, or as the receiver of a consumer such as
//! `to_list`, `fold` or `sum` — the checker lowers it to a single loop that
//! calls only the adapters' functions (`f` and `g`):
//!
//! ```text
//! for y in xs.iter().map(f).filter(g) { body }
//! ==>
//! { let src = xs; let a = f; let b = g
//!   for x in src { let y = a(x); if not b(y) { continue }; body } }
//! ```
//!
//! The lowering keeps the lazy semantics exactly: the source and the
//! adapters' arguments are evaluated once, in order, before the loop; each
//! adapter's function runs for the same elements and in the same order; and
//! the source is pulled the same number of times (`take` never pulls past
//! its count). The one difference: a pipeline stops at the first `None` of
//! its source, where a lazy `skip` would call an exhausted source again.
//! A list source becomes a native list loop (which visits the indices below
//! the length the list had when the loop started, where `[T].iter` reads it
//! when called) only if evaluating the adapters' and the consumer's arguments
//! cannot run code; otherwise the list's `iter()` is called as written.
//!
//! The eager methods of lists and ranges that take a function (`xs.map(f)`,
//! `filter`, `fold`, `any`, ...) are such pipelines too (`xs.map(f)` is
//! `xs.iter().map(f).to_list()`), and are lowered the same way. Lambda
//! literals passed to fused adapters and consumers are inlined.

use super::expr::mk;
use super::{Checker, Ctor};
use crate::diag::Span;
use crate::hir::*;
use crate::types::{FnId, Type};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adapter {
    Map,
    Filter,
    FilterMap,
    Enumerate,
    Inspect,
    Take,
    Skip,
    TakeWhile,
    SkipWhile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Consumer {
    ToList,
    Fold,
    Each,
    Count,
    Any,
    All,
    Find,
    Sum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Adapter(Adapter),
    Consumer(Consumer),
    ListIter,
    RangeIter,
    /// An eager list or range method (`xs.map(f)`), which is a pipeline of
    /// at most one adapter over the list or range, and a consumer.
    Eager {
        range: bool,
        stage: Option<Adapter>,
        consumer: Consumer,
    },
}

/// The prelude functions that take part in fusion, by id.
#[derive(Clone, Debug, Default)]
pub(crate) struct FuseFns {
    roles: HashMap<FnId, Role>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    List,
    Range,
    Iter,
}

struct Stage {
    kind: Adapter,
    arg: Option<HExpr>,
    /// The element type after this stage.
    out: Type,
}

struct Chain {
    kind: SourceKind,
    source: HExpr,
    /// The element type of the source.
    elem: Type,
    /// Adapters, from the source outwards.
    stages: Vec<Stage>,
}

/// A function called by the fused loop: a temporary holding its value, or
/// a lambda literal inlined at its single call site.
enum Func {
    Tmp(LocalId),
    Inline(Box<HLambda>),
}

/// The run-time state of a stage in the fused loop.
struct StageVars {
    kind: Adapter,
    out: Type,
    /// The adapter's function.
    func: Option<Func>,
    /// `take`/`skip`: elements left; `enumerate`: the counter.
    count: Option<LocalId>,
    /// `take`: a new pull starts at the next element; `skip_while`: still skipping.
    flag: Option<LocalId>,
}

/// Whether evaluating `e` cannot run code (so it may be moved after the
/// source list's length is read).
fn is_inert(e: &HExpr) -> bool {
    matches!(
        e.kind,
        HKind::Lambda(_)
            | HKind::Local(_)
            | HKind::Global(_)
            | HKind::FnValue(_)
            | HKind::VariantFn { .. }
            | HKind::Int(_)
            | HKind::Float(_)
            | HKind::Bool(_)
            | HKind::Str(_)
            | HKind::Unit
    )
}

fn arity(kind: Adapter) -> usize {
    match kind {
        Adapter::Enumerate => 1,
        _ => 2,
    }
}

impl Checker<'_> {
    fn fuse_role(&mut self, f: FnId) -> Option<Role> {
        if self.fuse_fns.is_none() {
            let found = self.find_fuse_fns();
            if found.roles.is_empty() {
                return None;
            }
            self.fuse_fns = Some(found);
        }
        self.fuse_fns.as_ref().and_then(|t| t.roles.get(&f).copied())
    }

    fn find_fuse_fns(&self) -> FuseFns {
        let mut roles = HashMap::new();
        let (Some(iter), Some(prelude)) = (self.iter_struct, self.prelude) else { return FuseFns { roles } };
        for (id, fi) in self.fns.iter().enumerate() {
            if fi.module != prelude || !fi.has_self {
                continue;
            }
            let eager = |range: bool, stage: Option<Adapter>, consumer: Consumer| Role::Eager { range, stage, consumer };
            let role = match (&fi.owner, fi.name.as_str()) {
                (Some(Ctor::List), "iter") => Role::ListIter,
                (Some(Ctor::Range), "iter") => Role::RangeIter,
                (Some(Ctor::List | Ctor::Range), name) => {
                    let range = fi.owner == Some(Ctor::Range);
                    match name {
                        "map" => eager(range, Some(Adapter::Map), Consumer::ToList),
                        "filter" => eager(range, Some(Adapter::Filter), Consumer::ToList),
                        "each" => eager(range, None, Consumer::Each),
                        _ if range => continue,
                        "filter_map" => eager(range, Some(Adapter::FilterMap), Consumer::ToList),
                        "take_while" => eager(range, Some(Adapter::TakeWhile), Consumer::ToList),
                        "count" => eager(range, Some(Adapter::Filter), Consumer::Count),
                        "fold" => eager(range, None, Consumer::Fold),
                        "any" => eager(range, None, Consumer::Any),
                        "all" => eager(range, None, Consumer::All),
                        "find" => eager(range, None, Consumer::Find),
                        _ => continue,
                    }
                }
                (Some(Ctor::Def(d)), name) if *d == iter => match name {
                    "map" => Role::Adapter(Adapter::Map),
                    "filter" => Role::Adapter(Adapter::Filter),
                    "filter_map" => Role::Adapter(Adapter::FilterMap),
                    "enumerate" => Role::Adapter(Adapter::Enumerate),
                    "inspect" => Role::Adapter(Adapter::Inspect),
                    "take" => Role::Adapter(Adapter::Take),
                    "skip" => Role::Adapter(Adapter::Skip),
                    "take_while" => Role::Adapter(Adapter::TakeWhile),
                    "skip_while" => Role::Adapter(Adapter::SkipWhile),
                    "to_list" => Role::Consumer(Consumer::ToList),
                    "fold" => Role::Consumer(Consumer::Fold),
                    "each" => Role::Consumer(Consumer::Each),
                    "count" => Role::Consumer(Consumer::Count),
                    "any" => Role::Consumer(Consumer::Any),
                    "all" => Role::Consumer(Consumer::All),
                    "find" => Role::Consumer(Consumer::Find),
                    "sum" => Role::Consumer(Consumer::Sum),
                    _ => continue,
                },
                _ => continue,
            };
            roles.insert(id as FnId, role);
        }
        FuseFns { roles }
    }

    /// The element type of an `Iter<T>` type.
    fn iter_elem(&self, t: &Type) -> Option<Type> {
        match self.resolve(t) {
            Type::Named(d, args) if Some(d) == self.iter_struct && args.len() == 1 => Some(args[0].clone()),
            _ => None,
        }
    }

    /// The adapter `e` calls, if it is a fusible adapter call.
    fn adapter_of(&mut self, e: &HExpr) -> Option<Adapter> {
        let HKind::Call { func, args } = &e.kind else { return None };
        let Some(Role::Adapter(kind)) = self.fuse_role(*func) else { return None };
        let fits = args.len() == arity(kind) && args.iter().all(|a| matches!(a, HArg::Expr(_)));
        (fits && self.iter_elem(&e.ty).is_some()).then_some(kind)
    }

    /// The source kind of a pipeline whose adapters were peeled off,
    /// leaving `e`; `inert` tells whether the adapters' arguments are inert.
    fn source_of(&mut self, e: &HExpr, inert: bool) -> Option<SourceKind> {
        if let HKind::Call { func, args } = &e.kind
            && let [HArg::Expr(_)] = args.as_slice()
        {
            match self.fuse_role(*func) {
                Some(Role::ListIter) if inert => return Some(SourceKind::List),
                Some(Role::RangeIter) => return Some(SourceKind::Range),
                _ => {}
            }
        }
        self.iter_elem(&e.ty).map(|_| SourceKind::Iter)
    }

    /// The number of adapters and the source kind of `e`, if it is a
    /// fusible pipeline: one with adapters, or a list or range source.
    /// `inert` tells whether the arguments evaluated after the pipeline (a
    /// consumer's) are inert.
    fn chain_shape(&mut self, e: &HExpr, inert: bool) -> Option<(usize, SourceKind)> {
        let mut depth = 0;
        let mut inert = inert;
        let mut cur = e;
        while self.adapter_of(cur).is_some() {
            let HKind::Call { args, .. } = &cur.kind else { unreachable!() };
            if let Some(HArg::Expr(a)) = args.get(1) {
                inert &= is_inert(a);
            }
            let HArg::Expr(recv) = &args[0] else { unreachable!() };
            cur = recv;
            depth += 1;
        }
        let kind = self.source_of(cur, inert)?;
        (depth > 0 || kind != SourceKind::Iter).then_some((depth, kind))
    }

    /// Splits a fusible pipeline of shape `chain_shape(&e)` into its source
    /// and adapters.
    fn take_chain(&mut self, e: HExpr, (depth, kind): (usize, SourceKind)) -> Chain {
        let mut stages = Vec::new();
        let mut cur = e;
        for _ in 0..depth {
            let kind = self.adapter_of(&cur).unwrap();
            let out = self.iter_elem(&cur.ty).unwrap();
            let HKind::Call { args, .. } = cur.kind else { unreachable!() };
            let mut args = args.into_iter().map(|a| match a {
                HArg::Expr(x) => x,
                HArg::Default { .. } => unreachable!(),
            });
            cur = args.next().unwrap();
            stages.push(Stage { kind, arg: args.next(), out });
        }
        stages.reverse();
        let (source, elem) = match kind {
            SourceKind::Iter => {
                let elem = self.iter_elem(&cur.ty).unwrap();
                (cur, elem)
            }
            SourceKind::List | SourceKind::Range => {
                let elem = self.iter_elem(&cur.ty).unwrap();
                let HKind::Call { args, .. } = cur.kind else { unreachable!() };
                let Some(HArg::Expr(recv)) = args.into_iter().next() else { unreachable!() };
                (recv, elem)
            }
        };
        Chain { kind, source, elem, stages }
    }

    /// The function `e` evaluates to, called once per element at one place
    /// of the fused loop. A lambda literal that cannot `return` (or use `?`)
    /// is inlined there: its locals become locals of the enclosing function.
    fn func(&mut self, e: HExpr, out: &mut Vec<HExpr>) -> Func {
        if let HKind::Lambda(lam) = &e.kind
            && lam.self_local.is_none()
        {
            let mut e = e;
            let HKind::Lambda(lam) = &mut e.kind else { unreachable!() };
            // (Walked in place: `e` is ours. Nested pipelines walk inner
            // bodies again, so the walk is charged.)
            let (mut returns, mut nodes) = (false, 0u64);
            lam.body.visit_mut(&mut |x| {
                nodes += 1;
                returns |= matches!(x.kind, HKind::Return(_) | HKind::Try { .. });
            });
            self.budget.charge(nodes);
            if !returns {
                let HKind::Lambda(lam) = e.kind else { unreachable!() };
                let owner = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
                // The lambda's locals were all made while it was checked, so
                // they come after the locals made before it (scanning all
                // the function's locals per lambda was quadratic).
                let first = self.fs.lambda_first_local.get(lam.id as usize - 1).map_or(0, |&l| l as usize).min(self.fs.locals.len());
                self.budget.charge((self.fs.locals.len() - first) as u64);
                for info in &mut self.fs.locals[first..] {
                    if info.owner == lam.id {
                        info.owner = owner;
                    }
                }
                return Func::Inline(lam);
            }
            return Func::Tmp(self.bind_tmp(e, out));
        }
        Func::Tmp(self.bind_tmp(e, out))
    }

    /// A call of `f` with `args`, whose result has type `ret`.
    fn apply(&mut self, f: Func, args: Vec<HExpr>, ret: Type, span: Span) -> HExpr {
        match f {
            Func::Tmp(l) => call_local(l, args, ret, span),
            Func::Inline(lam) => {
                let lam = *lam;
                let mut stmts: Vec<HExpr> = lam.params.iter().zip(args).map(|(&p, a)| let_(p, a.ty.clone(), a, span)).collect();
                stmts.push(lam.body);
                block_ty(stmts, ret, span)
            }
        }
    }

    /// `let <tmp> = e` (appended to `out`); returns the temporary.
    fn bind_tmp(&mut self, e: HExpr, out: &mut Vec<HExpr>) -> LocalId {
        let (ty, span) = (e.ty.clone(), e.span);
        let l = self.hidden_local(ty.clone(), span);
        out.push(let_(l, ty, e, span));
        l
    }

    /// The fused loop over `chain`: the statements evaluating the source and
    /// the adapters' arguments, then `extra`, then the loop, which runs the
    /// statements `body` returns for each element (given as a local).
    fn fused_loop(&mut self, chain: Chain, extra: Vec<HExpr>, span: Span, body: impl FnOnce(&mut Self, HExpr) -> Vec<HExpr>) -> Vec<HExpr> {
        let Chain { kind, source, elem, stages } = chain;
        let mut setup = Vec::new();
        let opt = self.option_of(elem.clone());
        // An `Iter` source is kept as its `next` function (an immutable field).
        let source = match kind {
            SourceKind::Iter => mk(HKind::Field { obj: Box::new(source), index: 0 }, Type::func(vec![], opt.clone()), span),
            _ => source,
        };
        let src_ty = source.ty.clone();
        let src = self.bind_tmp(source, &mut setup);
        let mut vars = Vec::new();
        for st in stages {
            let mut v = StageVars { kind: st.kind, out: st.out, func: None, count: None, flag: None };
            let arg = st.arg;
            match st.kind {
                Adapter::Map | Adapter::Filter | Adapter::FilterMap | Adapter::Inspect | Adapter::TakeWhile => {
                    v.func = arg.map(|a| self.func(a, &mut setup));
                }
                Adapter::SkipWhile => {
                    v.func = arg.map(|a| self.func(a, &mut setup));
                    v.flag = Some(self.bind_tmp(bool_(true, span), &mut setup));
                }
                Adapter::Enumerate => v.count = Some(self.bind_tmp(int(0, span), &mut setup)),
                Adapter::Take => {
                    v.count = arg.map(|a| self.bind_tmp(a, &mut setup));
                    v.flag = Some(self.bind_tmp(bool_(true, span), &mut setup));
                }
                Adapter::Skip => v.count = arg.map(|a| self.bind_tmp(a, &mut setup)),
            }
            vars.push(v);
        }
        setup.extend(extra);

        // A `take` checks its count when a pull of it starts: before the
        // source is pulled, outermost first.
        let mut top = Vec::new();
        for v in vars.iter().rev() {
            if let (Adapter::Take, Some(n), Some(pull)) = (v.kind, v.count, v.flag) {
                let stop = if_(cmp(CmpOp::Le, local(n, Type::Int, span), int(0, span)), break_(span), span);
                let start = block(vec![stop, add_assign(n, -1, span), assign(pull, bool_(false, span), span)], span);
                top.push(if_(local(pull, Type::Bool, span), start, span));
            }
        }
        let x0 = self.hidden_local(elem.clone(), span);
        let mut stmts = Vec::new();
        let mut cur = local(x0, elem.clone(), span);
        for mut v in vars {
            let mut f = v.func.take();
            let mut call = |s: &mut Self, args: Vec<HExpr>, ty: Type| s.apply(f.take().expect("adapter function"), args, ty, span);
            match v.kind {
                Adapter::Map => {
                    let x = self.hidden_local(v.out.clone(), span);
                    stmts.push(let_(x, v.out.clone(), call(self, vec![cur], v.out.clone()), span));
                    cur = local(x, v.out, span);
                }
                Adapter::Filter => stmts.push(if_(not(call(self, vec![cur.clone()], Type::Bool)), continue_(span), span)),
                Adapter::FilterMap => {
                    let x = self.hidden_local(v.out.clone(), span);
                    let opt = self.option_of(v.out.clone());
                    let pat = HPat {
                        kind: HPatKind::Variant { rt_id: self.some_rt(), args: vec![bind(x, v.out.clone(), span)] },
                        ty: opt.clone(),
                        span,
                    };
                    let init = call(self, vec![cur], opt);
                    stmts.push(mk(HKind::Let { pat, init: Box::new(init), else_: Some(Box::new(continue_(span))) }, Type::Unit, span));
                    cur = local(x, v.out, span);
                }
                Adapter::Enumerate => {
                    let n = v.count.unwrap();
                    let x = self.hidden_local(v.out.clone(), span);
                    let pair = mk(HKind::Tuple(vec![local(n, Type::Int, span), cur]), v.out.clone(), span);
                    stmts.push(let_(x, v.out.clone(), pair, span));
                    stmts.push(add_assign(n, 1, span));
                    cur = local(x, v.out, span);
                }
                Adapter::Inspect => stmts.push(call(self, vec![cur.clone()], Type::Unit)),
                Adapter::Take => stmts.push(assign(v.flag.unwrap(), bool_(true, span), span)),
                Adapter::Skip => {
                    let n = v.count.unwrap();
                    let skip = block(vec![add_assign(n, -1, span), continue_(span)], span);
                    stmts.push(if_(cmp(CmpOp::Gt, local(n, Type::Int, span), int(0, span)), skip, span));
                }
                Adapter::TakeWhile => stmts.push(if_(not(call(self, vec![cur.clone()], Type::Bool)), break_(span), span)),
                Adapter::SkipWhile => {
                    let skipping = v.flag.unwrap();
                    let skip = if_(call(self, vec![cur.clone()], Type::Bool), continue_(span), span);
                    let check = block(vec![skip, assign(skipping, bool_(false, span), span)], span);
                    stmts.push(if_(local(skipping, Type::Bool, span), check, span));
                }
            }
        }
        stmts.extend(body(self, cur));

        let lp = match kind {
            SourceKind::List | SourceKind::Range => {
                let iter_kind = if kind == SourceKind::List { IterKind::List } else { IterKind::Range };
                top.extend(stmts);
                let pat = bind(x0, elem, span);
                mk(
                    HKind::For { pat, iter: Box::new(local(src, src_ty, span)), kind: iter_kind, body: Box::new(block(top, span)) },
                    Type::Unit,
                    span,
                )
            }
            SourceKind::Iter => {
                let next = mk(HKind::CallValue { callee: Box::new(local(src, src_ty, span)), args: vec![] }, opt.clone(), span);
                let pat = HPat { kind: HPatKind::Variant { rt_id: self.some_rt(), args: vec![bind(x0, elem, span)] }, ty: opt, span };
                if top.is_empty() {
                    mk(HKind::WhileLet { pat, value: Box::new(next), body: Box::new(block(stmts, span)) }, Type::Unit, span)
                } else {
                    top.push(mk(HKind::Let { pat, init: Box::new(next), else_: Some(Box::new(break_(span))) }, Type::Unit, span));
                    top.extend(stmts);
                    mk(HKind::Loop(Box::new(block(top, span))), Type::Unit, span)
                }
            }
        };
        setup.push(lp);
        setup
    }

    fn some_rt(&self) -> u32 {
        self.types[self.wk.option as usize].variants()[0].rt_id
    }

    fn none_rt(&self) -> u32 {
        self.types[self.wk.option as usize].variants()[1].rt_id
    }

    /// Whether `iter` is a pipeline that `fuse_for` can fuse.
    pub(crate) fn can_fuse(&mut self, iter: &HExpr) -> bool {
        self.chain_shape(iter, true).is_some()
    }

    /// A `for` loop (or comprehension clause) over the fusible pipeline
    /// `iter` (see `can_fuse`), as a fused loop.
    pub(crate) fn fuse_for(&mut self, iter: HExpr, pat: HPat, body: HExpr, span: Span) -> HExpr {
        let shape = self.chain_shape(&iter, true).expect("a fusible pipeline");
        let chain = self.take_chain(iter, shape);
        let stmts = self
            .fused_loop(chain, vec![], span, |_, x| vec![mk(HKind::Let { pat, init: Box::new(x), else_: None }, Type::Unit, span), body]);
        block(stmts, span)
    }

    /// A call of an `Iter` consumer (`to_list`, `fold`, ...) on a fusible
    /// pipeline, as a fused loop; any other call is returned unchanged.
    pub(crate) fn fuse_consumer(&mut self, call: HExpr) -> HExpr {
        let HKind::Call { func, args } = &call.kind else { return call };
        let role = self.fuse_role(*func);
        let ndicts = self.fns[*func as usize].dicts.len();
        if !args.iter().all(|a| matches!(a, HArg::Expr(_))) || args.len() <= ndicts {
            return call;
        }
        let HArg::Expr(recv) = &args[ndicts] else { unreachable!() };
        let inert = args[ndicts + 1..].iter().all(|a| matches!(a, HArg::Expr(x) if is_inert(x)));
        let (c, shape) = match role {
            Some(Role::Consumer(c)) if let Some(shape) = self.chain_shape(recv, inert) => (c, shape),
            Some(Role::Eager { range, .. }) if range || matches!(self.resolve(&recv.ty), Type::List(_)) => return self.fuse_eager(call),
            _ => return call,
        };
        let num = match c {
            Consumer::Sum => match self.iter_elem(&recv.ty).map(|t| self.resolve(&t)) {
                Some(Type::Int) => int(0, call.span),
                Some(Type::Float) => mk(HKind::Float(0.0), Type::Float, call.span),
                _ => return call,
            },
            _ => mk(HKind::Unit, Type::Unit, call.span),
        };
        let (ty, span) = (call.ty.clone(), call.span);
        let mut args = plain_args(call, ndicts);
        let rest = args.split_off(1);
        let chain = self.take_chain(args.pop().unwrap(), shape);
        self.consume(c, chain, rest, num, ty, span)
    }

    /// An eager list or range method call (`xs.map(f)`) as a fused loop.
    fn fuse_eager(&mut self, call: HExpr) -> HExpr {
        let HKind::Call { func, .. } = &call.kind else { unreachable!() };
        let Some(Role::Eager { range, stage, consumer }) = self.fuse_role(*func) else { unreachable!() };
        let (ty, span) = (call.ty.clone(), call.span);
        let mut args = plain_args(call, 0);
        let rest = args.split_off(1);
        let source = args.pop().unwrap();
        let elem = match self.resolve(&source.ty) {
            Type::List(e) => *e,
            _ => Type::Int,
        };
        let mut rest = rest.into_iter();
        let stages = match stage {
            Some(kind) => {
                let out = match (kind, self.resolve(&ty)) {
                    (Adapter::Map | Adapter::FilterMap, Type::List(u)) => *u,
                    _ => elem.clone(),
                };
                vec![Stage { kind, arg: rest.next(), out }]
            }
            None => vec![],
        };
        let kind = if range { SourceKind::Range } else { SourceKind::List };
        let chain = Chain { kind, source, elem, stages };
        let unit = mk(HKind::Unit, Type::Unit, span);
        self.consume(consumer, chain, rest.collect(), unit, ty, span)
    }

    /// The fused loop of consumer `c` (with arguments `rest`) over `chain`;
    /// `num` is the zero of `sum`.
    fn consume(&mut self, c: Consumer, chain: Chain, rest: Vec<HExpr>, num: HExpr, ty: Type, span: Span) -> HExpr {
        let mut extra = Vec::new();
        let mut rest = rest.into_iter();
        let (result, stmts) = match c {
            Consumer::ToList => {
                let out = self.bind_tmp(mk(HKind::List(vec![]), ty.clone(), span), &mut extra);
                let stmts = self
                    .fused_loop(chain, extra, span, |_, x| vec![mk(HKind::PushLocal { list: out, value: Box::new(x) }, Type::Unit, span)]);
                (out, stmts)
            }
            Consumer::Fold => {
                let acc = self.bind_tmp(rest.next().unwrap(), &mut extra);
                let f = self.func(rest.next().unwrap(), &mut extra);
                let acc_ty = ty.clone();
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let v = s.apply(f, vec![local(acc, acc_ty.clone(), span), x], acc_ty, span);
                    vec![assign(acc, v, span)]
                });
                (acc, stmts)
            }
            Consumer::Each => {
                let f = self.func(rest.next().unwrap(), &mut extra);
                let stmts = self.fused_loop(chain, extra, span, |s, x| vec![s.apply(f, vec![x], Type::Unit, span)]);
                let mut stmts = stmts;
                stmts.push(mk(HKind::Unit, Type::Unit, span));
                return block_ty(stmts, ty, span);
            }
            Consumer::Count => {
                let n = self.bind_tmp(int(0, span), &mut extra);
                let stmts = self.fused_loop(chain, extra, span, |_, _| vec![add_assign(n, 1, span)]);
                (n, stmts)
            }
            Consumer::Any | Consumer::All => {
                let any = c == Consumer::Any;
                let f = self.func(rest.next().unwrap(), &mut extra);
                let r = self.bind_tmp(bool_(!any, span), &mut extra);
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let test = s.apply(f, vec![x], Type::Bool, span);
                    let test = if any { test } else { not(test) };
                    let found = block(vec![assign(r, bool_(any, span), span), break_(span)], span);
                    vec![if_(test, found, span)]
                });
                (r, stmts)
            }
            Consumer::Find => {
                let f = self.func(rest.next().unwrap(), &mut extra);
                let r = self.bind_tmp(mk(HKind::Tag(self.none_rt()), ty.clone(), span), &mut extra);
                let some = self.some_rt();
                let opt_ty = ty.clone();
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let test = s.apply(f, vec![x.clone()], Type::Bool, span);
                    let wrapped = mk(HKind::MakeVariant { rt_id: some, args: vec![x] }, opt_ty, span);
                    let found = block(vec![assign(r, wrapped, span), break_(span)], span);
                    vec![if_(test, found, span)]
                });
                (r, stmts)
            }
            Consumer::Sum => {
                let acc = self.bind_tmp(num, &mut extra);
                let acc_ty = ty.clone();
                let stmts = self.fused_loop(chain, extra, span, |_, x| {
                    let sum =
                        mk(HKind::Arith { op: ArithOp::Add, l: Box::new(local(acc, acc_ty.clone(), span)), r: Box::new(x) }, acc_ty, span);
                    vec![assign(acc, sum, span)]
                });
                (acc, stmts)
            }
        };
        let mut stmts = stmts;
        stmts.push(local(result, ty.clone(), span));
        block_ty(stmts, ty, span)
    }
}

/// The arguments of call `call` after its first `skip` (dictionaries).
fn plain_args(call: HExpr, skip: usize) -> Vec<HExpr> {
    let HKind::Call { args, .. } = call.kind else { unreachable!() };
    args.into_iter()
        .skip(skip)
        .map(|a| match a {
            HArg::Expr(x) => x,
            HArg::Default { .. } => unreachable!(),
        })
        .collect()
}

fn local(l: LocalId, ty: Type, span: Span) -> HExpr {
    mk(HKind::Local(l), ty, span)
}

fn int(i: i64, span: Span) -> HExpr {
    mk(HKind::Int(i), Type::Int, span)
}

fn bool_(b: bool, span: Span) -> HExpr {
    mk(HKind::Bool(b), Type::Bool, span)
}

fn bind(l: LocalId, ty: Type, span: Span) -> HPat {
    HPat { kind: HPatKind::Bind(l), ty, span }
}

fn let_(l: LocalId, ty: Type, init: HExpr, span: Span) -> HExpr {
    mk(HKind::Let { pat: bind(l, ty, span), init: Box::new(init), else_: None }, Type::Unit, span)
}

fn assign(l: LocalId, value: HExpr, span: Span) -> HExpr {
    mk(HKind::Assign { place: Place::Local(l), op: None, op_ty: value.ty.clone(), value: Box::new(value) }, Type::Unit, span)
}

fn add_assign(l: LocalId, k: i64, span: Span) -> HExpr {
    mk(HKind::Assign { place: Place::Local(l), op: Some(ArithOp::Add), op_ty: Type::Int, value: Box::new(int(k, span)) }, Type::Unit, span)
}

fn call_local(f: LocalId, args: Vec<HExpr>, ret: Type, span: Span) -> HExpr {
    let fty = Type::func(args.iter().map(|a| a.ty.clone()).collect(), ret.clone());
    mk(HKind::CallValue { callee: Box::new(local(f, fty, span)), args }, ret, span)
}

fn not(e: HExpr) -> HExpr {
    let span = e.span;
    mk(HKind::Unary { op: HUnOp::Not, e: Box::new(e) }, Type::Bool, span)
}

fn cmp(op: CmpOp, l: HExpr, r: HExpr) -> HExpr {
    let span = l.span;
    mk(HKind::Cmp { op, l: Box::new(l), r: Box::new(r) }, Type::Bool, span)
}

fn if_(cond: HExpr, then: HExpr, span: Span) -> HExpr {
    mk(HKind::If { cond: Box::new(cond), then: Box::new(then), else_: None }, Type::Unit, span)
}

fn block(stmts: Vec<HExpr>, span: Span) -> HExpr {
    block_ty(stmts, Type::Unit, span)
}

fn block_ty(stmts: Vec<HExpr>, ty: Type, span: Span) -> HExpr {
    mk(HKind::Block(stmts), ty, span)
}

fn break_(span: Span) -> HExpr {
    mk(HKind::Break { value: None, depth: 0 }, Type::Never, span)
}

fn continue_(span: Span) -> HExpr {
    mk(HKind::Continue { depth: 0 }, Type::Never, span)
}
