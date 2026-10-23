//! Fusion of lazy iterator pipelines (spec §12.13).
//!
//! An `Iter` pipeline such as `xs.iter().map(f).filter(g)` costs a closure
//! call per adapter and element, and every element is wrapped in `Some` at
//! each level. When the whole pipeline is visible where it is consumed — as
//! the iterable of a `for` loop or comprehension, or as the receiver of a
//! consumer such as `to_list`, `fold` or `sum` — the checker lowers it to a
//! single loop that calls only the adapters' functions:
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
//! the source is pulled the same number of times.
//! A list source becomes a native list loop, which reads the list's length
//! when it starts, only if evaluating the adapters' and the consumer's
//! arguments cannot run code.
//!
//! Eager list and range methods that take a function (`xs.map(f)` is
//! `xs.iter().map(f).to_list()`) are lowered the same way. Lambda literals
//! passed to fused adapters and consumers are inlined.

use super::expr::mk;
use super::{Checker, Ctor};
use crate::diag::Span;
use crate::hir::*;
use crate::types::{FnId, Ty, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

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
    FindIndex,
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
    /// The native id of `[T].enumerate` (see `enumerate_for`).
    list_enumerate: Option<u32>,
    /// The native id of `str.chars` (see `chars_source`).
    str_chars: Option<u32>,
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
    out: TypeId,
}

struct Chain {
    kind: SourceKind,
    source: HExpr,
    elem: TypeId,
    /// Adapters, from the source outwards.
    stages: Vec<Stage>,
}

/// A function called by the fused loop: a temporary holding its value, or
/// a lambda literal inlined at its single call site.
enum Func {
    Tmp(LocalId),
    Inline(Box<HLambda>),
    /// A script function, called directly.
    Fn(FnId),
}

enum LoopSrc {
    Value { local: LocalId, ty: TypeId, kind: IterKind },
    Bounds { start: LocalId, end: LocalId, inclusive: bool },
}

/// The run-time state of a stage in the fused loop.
struct StageVars {
    kind: Adapter,
    out: TypeId,
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

impl Checker {
    fn fuse_role(&mut self, f: FnId) -> Option<Role> {
        if self.fuse_fns.is_none() {
            let found = self.find_fuse_fns();
            if found.roles.is_empty() {
                return None;
            }
            self.fuse_fns = Some(Arc::new(found));
        }
        self.fuse_fns.as_ref().and_then(|t| t.roles.get(&f).copied())
    }

    fn find_fuse_fns(&self) -> FuseFns {
        let mut roles = HashMap::new();
        let mut list_enumerate = None;
        let mut str_chars = None;
        let (Some(iter), Some(prelude)) = (self.iter_struct, self.prelude) else { return FuseFns { roles, list_enumerate, str_chars } };
        for (id, fi) in self.fns.iter().enumerate() {
            if fi.module != prelude || !fi.has_self {
                continue;
            }
            let name = self.name(fi.name);
            match (&fi.owner, name, &fi.kind) {
                (Some(Ctor::List), "enumerate", super::FnKind::Native(n)) => list_enumerate = Some(*n),
                (Some(Ctor::Str), "chars", super::FnKind::Native(n)) => str_chars = Some(*n),
                _ => {}
            }
            let eager = |range: bool, stage: Option<Adapter>, consumer: Consumer| Role::Eager { range, stage, consumer };
            let role = match (&fi.owner, name) {
                (Some(Ctor::List), "iter") => Role::ListIter,
                (Some(Ctor::Range), "iter") => Role::RangeIter,
                (Some(Ctor::List | Ctor::Range), name) => {
                    let range = fi.owner == Some(Ctor::Range);
                    match name {
                        "map" => eager(range, Some(Adapter::Map), Consumer::ToList),
                        "filter" => eager(range, Some(Adapter::Filter), Consumer::ToList),
                        "each" => eager(range, None, Consumer::Each),
                        "filter_map" => eager(range, Some(Adapter::FilterMap), Consumer::ToList),
                        "take_while" => eager(range, Some(Adapter::TakeWhile), Consumer::ToList),
                        "count" => eager(range, Some(Adapter::Filter), Consumer::Count),
                        "fold" => eager(range, None, Consumer::Fold),
                        "any" => eager(range, None, Consumer::Any),
                        "all" => eager(range, None, Consumer::All),
                        "find" => eager(range, None, Consumer::Find),
                        "find_index" => eager(range, None, Consumer::FindIndex),
                        "skip_while" => eager(range, Some(Adapter::SkipWhile), Consumer::ToList),
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
                    "find_index" => Role::Consumer(Consumer::FindIndex),
                    "sum" => Role::Consumer(Consumer::Sum),
                    _ => continue,
                },
                _ => continue,
            };
            roles.insert(id as FnId, role);
        }
        FuseFns { roles, list_enumerate, str_chars }
    }

    fn iter_elem(&self, t: TypeId) -> Option<TypeId> {
        match self.rty(t) {
            Ty::Named(d, args) if Some(d) == self.iter_struct && self.arena.list_len(args) == 1 => Some(self.arena.elem(args, 0)),
            _ => None,
        }
    }

    fn adapter_of(&mut self, e: &HExpr) -> Option<Adapter> {
        let HKind::Call { func, args } = &e.kind else { return None };
        let Some(Role::Adapter(kind)) = self.fuse_role(*func) else { return None };
        let fits = args.len() == arity(kind) && args.iter().all(|a| matches!(a, HArg::Expr(_)));
        (fits && self.iter_elem(e.ty).is_some()).then_some(kind)
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
        self.iter_elem(e.ty).map(|_| SourceKind::Iter)
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
            let out = self.iter_elem(cur.ty).unwrap();
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
                let elem = self.iter_elem(cur.ty).unwrap();
                (cur, elem)
            }
            SourceKind::List | SourceKind::Range => {
                let elem = self.iter_elem(cur.ty).unwrap();
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
        if let HKind::FnValue(f) = e.kind
            && self.fns[f as usize].dicts.is_empty()
        {
            return Func::Fn(f);
        }
        if let HKind::Lambda(lam) = &e.kind
            && lam.self_local.is_none()
        {
            let mut e = e;
            let HKind::Lambda(lam) = &mut e.kind else { unreachable!() };
            // Charged: nested pipelines walk inner bodies again.
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
                // they start at `lambda_first_local`.
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

    fn apply(&mut self, f: Func, args: Vec<HExpr>, ret: TypeId, span: Span) -> HExpr {
        match f {
            Func::Tmp(l) => self.call_local(l, args, ret, span),
            Func::Fn(f) => mk(HKind::Call { func: f, args: args.into_iter().map(HArg::Expr).collect() }, ret, span),
            Func::Inline(lam) => {
                let lam = *lam;
                let mut stmts: Vec<HExpr> = lam.params.iter().zip(args).map(|(&p, a)| let_(p, a.ty, a, span)).collect();
                stmts.push(lam.body);
                block_ty(stmts, ret, span)
            }
        }
    }

    /// `let <tmp> = e` (appended to `out`); returns the temporary.
    fn bind_tmp(&mut self, e: HExpr, out: &mut Vec<HExpr>) -> LocalId {
        let (ty, span) = (e.ty, e.span);
        let l = self.hidden_local(ty, span);
        out.push(let_(l, ty, e, span));
        l
    }

    /// The fused loop over `chain`: the statements evaluating the source and
    /// the adapters' arguments, then `extra`, then the loop, which runs the
    /// statements `body` returns for each element (given as a local).
    fn fused_loop(&mut self, chain: Chain, extra: Vec<HExpr>, span: Span, body: impl FnOnce(&mut Self, HExpr) -> Vec<HExpr>) -> Vec<HExpr> {
        let Chain { kind, source, elem, stages } = chain;
        let mut setup = Vec::new();
        let opt = self.option_of(elem);
        // An `Iter` source is kept as its `next` function (an immutable field).
        let source = match kind {
            SourceKind::Iter => {
                let next_ty = self.fn_ty(&[], opt);
                mk(HKind::Field { obj: Box::new(source), index: 0 }, next_ty, span)
            }
            _ => source,
        };
        let src = match (kind, source.kind) {
            (SourceKind::Range, HKind::Range { start, end, inclusive }) => {
                LoopSrc::Bounds { start: self.bind_tmp(*start, &mut setup), end: self.bind_tmp(*end, &mut setup), inclusive }
            }
            (_, source_kind) => {
                let source = HExpr { kind: source_kind, ..source };
                let (source, iter_kind) = match kind {
                    SourceKind::Range => (source, IterKind::Range),
                    _ => match self.chars_source(source) {
                        Ok(s) => (s, IterKind::Chars),
                        Err(source) => (source, IterKind::List),
                    },
                };
                let ty = source.ty;
                LoopSrc::Value { local: self.bind_tmp(source, &mut setup), ty, kind: iter_kind }
            }
        };
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
                let stop = if_(cmp(CmpOp::Le, local(n, TypeId::INT, span), int(0, span)), break_(span), span);
                let start = block(vec![stop, add_assign(n, -1, span), assign(pull, bool_(false, span), span)], span);
                top.push(if_(local(pull, TypeId::BOOL, span), start, span));
            }
        }
        let x0 = self.hidden_local(elem, span);
        let mut stmts = Vec::new();
        let mut cur = local(x0, elem, span);
        for mut v in vars {
            let mut f = v.func.take();
            let mut call = |s: &mut Self, args: Vec<HExpr>, ty: TypeId| s.apply(f.take().expect("adapter function"), args, ty, span);
            match v.kind {
                Adapter::Map => {
                    let x = self.hidden_local(v.out, span);
                    stmts.push(let_(x, v.out, call(self, vec![cur], v.out), span));
                    cur = local(x, v.out, span);
                }
                Adapter::Filter => stmts.push(if_(not(call(self, vec![cur.clone()], TypeId::BOOL)), continue_(span), span)),
                Adapter::FilterMap => {
                    let x = self.hidden_local(v.out, span);
                    let opt = self.option_of(v.out);
                    let pat = HPat { kind: HPatKind::Variant { rt_id: self.some_rt(), args: vec![bind(x, v.out, span)] }, ty: opt, span };
                    let init = call(self, vec![cur], opt);
                    stmts.push(mk(HKind::Let { pat, init: Box::new(init), else_: Some(Box::new(continue_(span))) }, TypeId::UNIT, span));
                    cur = local(x, v.out, span);
                }
                Adapter::Enumerate => {
                    let n = v.count.unwrap();
                    let x = self.hidden_local(v.out, span);
                    let pair = mk(HKind::Tuple(vec![local(n, TypeId::INT, span), cur]), v.out, span);
                    stmts.push(let_(x, v.out, pair, span));
                    stmts.push(add_assign(n, 1, span));
                    cur = local(x, v.out, span);
                }
                Adapter::Inspect => stmts.push(call(self, vec![cur.clone()], TypeId::UNIT)),
                Adapter::Take => stmts.push(assign(v.flag.unwrap(), bool_(true, span), span)),
                Adapter::Skip => {
                    let n = v.count.unwrap();
                    let skip = block(vec![add_assign(n, -1, span), continue_(span)], span);
                    stmts.push(if_(cmp(CmpOp::Gt, local(n, TypeId::INT, span), int(0, span)), skip, span));
                }
                Adapter::TakeWhile => stmts.push(if_(not(call(self, vec![cur.clone()], TypeId::BOOL)), break_(span), span)),
                Adapter::SkipWhile => {
                    let skipping = v.flag.unwrap();
                    let skip = if_(call(self, vec![cur.clone()], TypeId::BOOL), continue_(span), span);
                    let check = block(vec![skip, assign(skipping, bool_(false, span), span)], span);
                    stmts.push(if_(local(skipping, TypeId::BOOL, span), check, span));
                }
            }
        }
        stmts.extend(body(self, cur));

        let lp = match (kind, src) {
            (_, LoopSrc::Bounds { start, end, inclusive }) => {
                top.extend(stmts);
                let (start, end) = (Box::new(local(start, TypeId::INT, span)), Box::new(local(end, TypeId::INT, span)));
                let pat = bind(x0, elem, span);
                mk(HKind::ForRange { pat, start, end, inclusive, body: Box::new(block(top, span)) }, TypeId::UNIT, span)
            }
            (SourceKind::List | SourceKind::Range, LoopSrc::Value { local: src, ty, kind }) => {
                top.extend(stmts);
                let pat = bind(x0, elem, span);
                mk(HKind::For { pat, iter: Box::new(local(src, ty, span)), kind, body: Box::new(block(top, span)) }, TypeId::UNIT, span)
            }
            (SourceKind::Iter, LoopSrc::Value { local: src, ty, .. }) => {
                let next = mk(HKind::CallValue { callee: Box::new(local(src, ty, span)), args: vec![] }, opt, span);
                let pat = HPat { kind: HPatKind::Variant { rt_id: self.some_rt(), args: vec![bind(x0, elem, span)] }, ty: opt, span };
                if top.is_empty() {
                    mk(HKind::WhileLet { pat, value: Box::new(next), body: Box::new(block(stmts, span)) }, TypeId::UNIT, span)
                } else {
                    top.push(mk(HKind::Let { pat, init: Box::new(next), else_: Some(Box::new(break_(span))) }, TypeId::UNIT, span));
                    top.extend(stmts);
                    mk(HKind::Loop(Box::new(block(top, span))), TypeId::UNIT, span)
                }
            }
        };
        setup.push(lp);
        setup
    }

    fn call_local(&mut self, f: LocalId, args: Vec<HExpr>, ret: TypeId, span: Span) -> HExpr {
        let ps: Vec<TypeId> = args.iter().map(|a| a.ty).collect();
        let fty = self.fn_ty(&ps, ret);
        mk(HKind::CallValue { callee: Box::new(local(f, fty, span)), args }, ret, span)
    }

    fn some_rt(&self) -> u32 {
        self.types[self.wk.option as usize].variants()[0].rt_id
    }

    pub(crate) fn none_rt(&self) -> u32 {
        self.types[self.wk.option as usize].variants()[1].rt_id
    }

    /// The string `s` of `s.chars()` (`Err(e)` for any other `e`), whose
    /// characters a loop can visit without building the list.
    pub(crate) fn chars_source(&mut self, e: HExpr) -> Result<HExpr, HExpr> {
        if !self.optimize {
            return Err(e);
        }
        if self.fuse_fns.is_none() {
            let _ = self.fuse_role(0);
        }
        let chars = self.fuse_fns.as_ref().and_then(|t| t.str_chars);
        match e.kind {
            HKind::CallNative { native, args, .. } if Some(native) == chars && matches!(args[..], [HArg::Expr(_)]) => {
                let Some(HArg::Expr(s)) = args.into_iter().next() else { unreachable!() };
                Ok(s)
            }
            _ => Err(e),
        }
    }

    /// Whether a loop over `iter` binding `pat` is `for (i, x) in
    /// xs.enumerate()`, which `enumerate_for` lowers.
    pub(crate) fn is_enumerate_for(&mut self, iter: &HExpr, pat: &HPat) -> bool {
        if !self.optimize {
            return false;
        }
        if self.fuse_fns.is_none() {
            // Builds the tables; any prelude function will do.
            let _ = self.fuse_role(0);
        }
        let enumerate = self.fuse_fns.as_ref().and_then(|t| t.list_enumerate);
        matches!(&iter.kind, HKind::CallNative { native, args, .. } if Some(*native) == enumerate && matches!(args[..], [HArg::Expr(_)]))
            && matches!(&pat.kind, HPatKind::Tuple(ps) if ps.len() == 2)
    }

    /// `for (i, x) in xs.enumerate() { body }` (see `is_enumerate_for`)
    /// without building the list of pairs: the loop runs over a copy of
    /// `xs` (as `enumerate` copies its elements when called) with a counter.
    pub(crate) fn enumerate_for(&mut self, iter: HExpr, pat: HPat, body: HExpr, span: Span) -> HExpr {
        let HKind::CallNative { args, .. } = iter.kind else { unreachable!() };
        let Some(HArg::Expr(list)) = args.into_iter().next() else { unreachable!() };
        let HPatKind::Tuple(mut ps) = pat.kind else { unreachable!() };
        let (px, pi) = (ps.pop().unwrap(), ps.pop().unwrap());
        let mut stmts = Vec::new();
        let list_ty = list.ty;
        let copy = mk(HKind::Slice { obj: Box::new(list), start: None, end: None, inclusive: false, kind: SeqKind::List }, list_ty, span);
        let snap = self.bind_tmp(copy, &mut stmts);
        let n = self.bind_tmp(int(0, span), &mut stmts);
        let x = self.hidden_local(px.ty, span);
        let x_ty = px.ty;
        let body = block(
            vec![
                mk(HKind::Let { pat: pi, init: Box::new(local(n, TypeId::INT, span)), else_: None }, TypeId::UNIT, span),
                add_assign(n, 1, span),
                mk(HKind::Let { pat: px, init: Box::new(local(x, x_ty, span)), else_: None }, TypeId::UNIT, span),
                body,
            ],
            span,
        );
        let lp = mk(
            HKind::For { pat: bind(x, x_ty, span), iter: Box::new(local(snap, list_ty, span)), kind: IterKind::List, body: Box::new(body) },
            TypeId::UNIT,
            span,
        );
        stmts.push(lp);
        block(stmts, span)
    }

    pub(crate) fn can_fuse(&mut self, iter: &HExpr) -> bool {
        self.optimize && self.chain_shape(iter, true).is_some()
    }

    /// A `for` loop (or comprehension clause) over the fusible pipeline
    /// `iter` (see `can_fuse`), as a fused loop.
    pub(crate) fn fuse_for(&mut self, iter: HExpr, pat: HPat, body: HExpr, span: Span) -> HExpr {
        let shape = self.chain_shape(&iter, true).expect("a fusible pipeline");
        let chain = self.take_chain(iter, shape);
        let stmts = self
            .fused_loop(chain, vec![], span, |_, x| vec![mk(HKind::Let { pat, init: Box::new(x), else_: None }, TypeId::UNIT, span), body]);
        block(stmts, span)
    }

    /// A call of an `Iter` consumer (`to_list`, `fold`, ...) on a fusible
    /// pipeline, as a fused loop; any other call is returned unchanged.
    pub(crate) fn fuse_consumer(&mut self, call: HExpr) -> HExpr {
        let HKind::Call { func, args } = &call.kind else { return call };
        if !self.optimize {
            return call;
        }
        let role = self.fuse_role(*func);
        let ndicts = self.fns[*func as usize].dicts.len();
        if !args.iter().all(|a| matches!(a, HArg::Expr(_))) || args.len() <= ndicts {
            return call;
        }
        let HArg::Expr(recv) = &args[ndicts] else { unreachable!() };
        let inert = args[ndicts + 1..].iter().all(|a| matches!(a, HArg::Expr(x) if is_inert(x)));
        let (c, shape) = match role {
            Some(Role::Consumer(c)) if let Some(shape) = self.chain_shape(recv, inert) => (c, shape),
            Some(Role::Eager { range, .. }) if range || matches!(self.rty(recv.ty), Ty::List(_)) => return self.fuse_eager(call),
            _ => return call,
        };
        let num = match c {
            Consumer::Sum => match self.iter_elem(recv.ty).map(|t| self.resolve(t)) {
                Some(TypeId::INT) => int(0, call.span),
                Some(TypeId::FLOAT) => mk(HKind::Float(0.0), TypeId::FLOAT, call.span),
                _ => return call,
            },
            _ => mk(HKind::Unit, TypeId::UNIT, call.span),
        };
        let (ty, span) = (call.ty, call.span);
        let mut args = plain_args(call, ndicts);
        let rest = args.split_off(1);
        let chain = self.take_chain(args.pop().unwrap(), shape);
        self.consume(c, chain, rest, num, ty, span)
    }

    /// An eager list or range method call (`xs.map(f)`) as a fused loop.
    fn fuse_eager(&mut self, call: HExpr) -> HExpr {
        let HKind::Call { func, .. } = &call.kind else { unreachable!() };
        let Some(Role::Eager { range, stage, consumer }) = self.fuse_role(*func) else { unreachable!() };
        let (ty, span) = (call.ty, call.span);
        let mut args = plain_args(call, 0);
        let rest = args.split_off(1);
        let source = args.pop().unwrap();
        let elem = match self.rty(source.ty) {
            Ty::List(e) => e,
            _ => TypeId::INT,
        };
        let mut rest = rest.into_iter();
        let stages = match stage {
            Some(kind) => {
                let out = match (kind, self.rty(ty)) {
                    (Adapter::Map | Adapter::FilterMap, Ty::List(u)) => u,
                    _ => elem,
                };
                vec![Stage { kind, arg: rest.next(), out }]
            }
            None => vec![],
        };
        let kind = if range { SourceKind::Range } else { SourceKind::List };
        let chain = Chain { kind, source, elem, stages };
        let unit = mk(HKind::Unit, TypeId::UNIT, span);
        self.consume(consumer, chain, rest.collect(), unit, ty, span)
    }

    /// The fused loop of consumer `c` (with arguments `rest`) over `chain`;
    /// `num` is the zero of `sum`.
    fn consume(&mut self, c: Consumer, chain: Chain, rest: Vec<HExpr>, num: HExpr, ty: TypeId, span: Span) -> HExpr {
        let mut extra = Vec::new();
        let mut rest = rest.into_iter();
        let (result, stmts) = match c {
            Consumer::ToList => {
                let out = self.bind_tmp(mk(HKind::List(vec![]), ty, span), &mut extra);
                let stmts = self.fused_loop(chain, extra, span, |_, x| {
                    vec![mk(HKind::PushLocal { list: out, value: Box::new(x) }, TypeId::UNIT, span)]
                });
                (out, stmts)
            }
            Consumer::Fold => {
                let acc = self.bind_tmp(rest.next().unwrap(), &mut extra);
                let f = self.func(rest.next().unwrap(), &mut extra);
                let acc_ty = ty;
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let v = s.apply(f, vec![local(acc, acc_ty, span), x], acc_ty, span);
                    vec![assign(acc, v, span)]
                });
                (acc, stmts)
            }
            Consumer::Each => {
                let f = self.func(rest.next().unwrap(), &mut extra);
                let stmts = self.fused_loop(chain, extra, span, |s, x| vec![s.apply(f, vec![x], TypeId::UNIT, span)]);
                let mut stmts = stmts;
                stmts.push(mk(HKind::Unit, TypeId::UNIT, span));
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
                    let test = s.apply(f, vec![x], TypeId::BOOL, span);
                    let test = if any { test } else { not(test) };
                    let found = block(vec![assign(r, bool_(any, span), span), break_(span)], span);
                    vec![if_(test, found, span)]
                });
                (r, stmts)
            }
            Consumer::Find => {
                let f = self.func(rest.next().unwrap(), &mut extra);
                let r = self.bind_tmp(mk(HKind::Tag(self.none_rt()), ty, span), &mut extra);
                let some = self.some_rt();
                let opt_ty = ty;
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let test = s.apply(f, vec![x.clone()], TypeId::BOOL, span);
                    let wrapped = mk(HKind::MakeVariant { rt_id: some, args: vec![x] }, opt_ty, span);
                    let found = block(vec![assign(r, wrapped, span), break_(span)], span);
                    vec![if_(test, found, span)]
                });
                (r, stmts)
            }
            Consumer::FindIndex => {
                let f = self.func(rest.next().unwrap(), &mut extra);
                let r = self.bind_tmp(mk(HKind::Tag(self.none_rt()), ty, span), &mut extra);
                let n = self.bind_tmp(int(0, span), &mut extra);
                let some = self.some_rt();
                let opt_ty = ty;
                let stmts = self.fused_loop(chain, extra, span, |s, x| {
                    let test = s.apply(f, vec![x], TypeId::BOOL, span);
                    let wrapped = mk(HKind::MakeVariant { rt_id: some, args: vec![local(n, TypeId::INT, span)] }, opt_ty, span);
                    let found = block(vec![assign(r, wrapped, span), break_(span)], span);
                    vec![if_(test, found, span), add_assign(n, 1, span)]
                });
                (r, stmts)
            }
            Consumer::Sum => {
                let acc = self.bind_tmp(num, &mut extra);
                let acc_ty = ty;
                let stmts = self.fused_loop(chain, extra, span, |_, x| {
                    let sum = mk(HKind::Arith { op: ArithOp::Add, l: Box::new(local(acc, acc_ty, span)), r: Box::new(x) }, acc_ty, span);
                    vec![assign(acc, sum, span)]
                });
                (acc, stmts)
            }
        };
        let mut stmts = stmts;
        stmts.push(local(result, ty, span));
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

fn local(l: LocalId, ty: TypeId, span: Span) -> HExpr {
    mk(HKind::Local(l), ty, span)
}

fn int(i: i64, span: Span) -> HExpr {
    mk(HKind::Int(i), TypeId::INT, span)
}

fn bool_(b: bool, span: Span) -> HExpr {
    mk(HKind::Bool(b), TypeId::BOOL, span)
}

fn bind(l: LocalId, ty: TypeId, span: Span) -> HPat {
    HPat { kind: HPatKind::Bind(l), ty, span }
}

fn let_(l: LocalId, ty: TypeId, init: HExpr, span: Span) -> HExpr {
    mk(HKind::Let { pat: bind(l, ty, span), init: Box::new(init), else_: None }, TypeId::UNIT, span)
}

fn assign(l: LocalId, value: HExpr, span: Span) -> HExpr {
    mk(HKind::Assign { place: Place::Local(l), op: None, op_ty: value.ty, value: Box::new(value) }, TypeId::UNIT, span)
}

fn add_assign(l: LocalId, k: i64, span: Span) -> HExpr {
    mk(
        HKind::Assign { place: Place::Local(l), op: Some(ArithOp::Add), op_ty: TypeId::INT, value: Box::new(int(k, span)) },
        TypeId::UNIT,
        span,
    )
}

fn not(e: HExpr) -> HExpr {
    let span = e.span;
    mk(HKind::Unary { op: HUnOp::Not, e: Box::new(e) }, TypeId::BOOL, span)
}

fn cmp(op: CmpOp, l: HExpr, r: HExpr) -> HExpr {
    let span = l.span;
    mk(HKind::Cmp { op, l: Box::new(l), r: Box::new(r) }, TypeId::BOOL, span)
}

fn if_(cond: HExpr, then: HExpr, span: Span) -> HExpr {
    mk(HKind::If { cond: Box::new(cond), then: Box::new(then), else_: None }, TypeId::UNIT, span)
}

fn block(stmts: Vec<HExpr>, span: Span) -> HExpr {
    block_ty(stmts, TypeId::UNIT, span)
}

fn block_ty(stmts: Vec<HExpr>, ty: TypeId, span: Span) -> HExpr {
    mk(HKind::Block(stmts), ty, span)
}

fn break_(span: Span) -> HExpr {
    mk(HKind::Break { value: None, depth: 0 }, TypeId::NEVER, span)
}

fn continue_(span: Span) -> HExpr {
    mk(HKind::Continue { depth: 0 }, TypeId::NEVER, span)
}
