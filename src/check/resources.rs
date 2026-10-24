//! How resources flow through functions, for the resource lints (spec
//! §17.2): `unclosed-resource`, `resource-field` and `resource-escapes`.
//!
//! A *resource* is a value whose type implements the prelude's `Close`. Each
//! function's final HIR is analysed, flow-insensitively, for the *origins* of
//! the resources it handles (parameters, `use` bindings, resources it
//! creates), the values that carry them, and *sinks*, where a value may
//! outlive the call: returned, stored, captured by `spawn`, or passed to a
//! parameter that is a sink itself.
//!
//! Each function's [`ParamFlow`] is kept for callers in later modules.
//! Natives are described by their signatures. Calls through function values
//! and trait dictionaries are resolved only as far as the signatures go.

use super::*;
use crate::hir::{HArg, HExpr, HFn, HKind, HLambda, HPat, HPatKind, InterpPart, LocalId, Place};
use std::collections::HashMap;

/// How the parameters of a function may outlive a call to it: bit `i`
/// stands for declared parameter `i` (dictionaries excluded).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParamFlow {
    /// Parameters that may be stored, captured by a task, or passed on to
    /// a parameter that may.
    pub escapes: u64,
    /// Parameters the result may refer to.
    pub returns: u64,
}

/// A set of origins.
type Set = u128;

const MAX_ORIGINS: usize = 128;

#[derive(Clone, Copy, PartialEq)]
enum OriginKind {
    Param(u32),
    Use(LocalId),
    /// A resource created here.
    Fresh,
}

struct Origin {
    kind: OriginKind,
    span: Span,
    ty: TypeId,
    /// The first local a fresh resource is bound to.
    binding: Option<LocalId>,
    /// Bound by `use` (closed when its block ends).
    closed: bool,
}

#[derive(Clone, Copy)]
enum SinkKind {
    Return,
    Store,
    /// Taken over by a new resource of this type.
    Own(TypeId),
    Spawn,
    /// Passed to a parameter of this function that may outlive the call.
    Pass(FnId),
}

struct Sink {
    set: Set,
    span: Span,
    kind: SinkKind,
}

/// The analysis of one function body.
struct Flow<'b> {
    body: &'b HFn,
    /// The first declared parameter in `body.params` (after dictionaries).
    first_param: usize,
    origins: Vec<Origin>,
    /// Fresh origins by the span of the expression creating them.
    fresh: HashMap<(u32, u32, u32), usize>,
    /// What each local may carry.
    locals: Vec<Set>,
    sinks: Vec<Sink>,
    /// Values `break` gives the loops being walked.
    breaks: Vec<Set>,
    /// Lambdas being walked (innermost last): the locals each reads, and
    /// the ids of the lambdas in it (whose locals it does not capture).
    lambdas: Vec<(Vec<LocalId>, Vec<u32>)>,
    changed: bool,
}

/// What the pass needs of the checker, borrowed while bodies are walked.
struct Ctx<'c> {
    ck: &'c mut Checker,
    close: TraitId,
    natives: HashMap<u32, FnId>,
    resource: HashMap<TypeId, bool>,
    plain: HashMap<TypeId, bool>,
}

impl Ctx<'_> {
    /// Whether values of type `t` are resources.
    fn is_resource(&mut self, t: TypeId) -> bool {
        let t = self.ck.resolve(t);
        if let Some(&r) = self.resource.get(&t) {
            return r;
        }
        // A recursive type is not a resource while it is being examined.
        self.resource.insert(t, false);
        let r = match self.ck.ty(t) {
            Ty::List(e) => self.is_resource(e),
            Ty::Named(d, args) if d == self.ck.wk.option => {
                let a = self.ck.arena.elem(args, 0);
                self.is_resource(a)
            }
            Ty::Named(..) | Ty::Param(_) => self.ck.implements_trait(t, self.close),
            _ => false,
        };
        self.resource.insert(t, r);
        r
    }

    /// Whether values of type `t` are plain data, which cannot hold a
    /// resource: primitives, and lists, sets, maps, tuples, options and
    /// results of plain data.
    fn is_plain(&mut self, t: TypeId) -> bool {
        let t = self.ck.resolve(t);
        if let Some(&p) = self.plain.get(&t) {
            return p;
        }
        let p = match self.ck.ty(t) {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Never | Ty::Range | Ty::Error => true,
            Ty::List(e) | Ty::Set(e) => self.is_plain(e),
            Ty::Map(k, v) => self.is_plain(k) && self.is_plain(v),
            Ty::Tuple(ts) => {
                let ts = self.ck.elems(ts);
                ts.into_iter().all(|t| self.is_plain(t))
            }
            Ty::Named(d, args) if d == self.ck.wk.option || d == self.ck.wk.result => {
                let args = self.ck.elems(args);
                args.into_iter().all(|t| self.is_plain(t))
            }
            _ => false,
        };
        self.plain.insert(t, p);
        p
    }

    /// The resource that a value of type `t` (which holds one) holds.
    fn resource_in(&mut self, t: TypeId) -> TypeId {
        let t = self.ck.resolve(t);
        if self.is_resource(t) {
            return t;
        }
        let parts = match self.ck.ty(t) {
            Ty::Named(d, args) if d == self.ck.wk.result => vec![self.ck.arena.elem(args, 0)],
            Ty::Tuple(ts) => self.ck.elems(ts),
            _ => vec![],
        };
        parts.into_iter().find(|&p| self.holds_resource(p)).map_or(t, |p| self.resource_in(p))
    }

    /// Whether values of type `t` hold a resource: are one, or a `Result`,
    /// option or tuple of one.
    fn holds_resource(&mut self, t: TypeId) -> bool {
        let t = self.ck.resolve(t);
        if self.is_resource(t) {
            return true;
        }
        match self.ck.ty(t) {
            Ty::Named(d, args) if d == self.ck.wk.result => {
                let a = self.ck.arena.elem(args, 0);
                self.holds_resource(a)
            }
            Ty::Tuple(ts) => {
                let ts = self.ck.elems(ts);
                ts.into_iter().any(|t| self.holds_resource(t))
            }
            _ => false,
        }
    }

    /// What a function's parameters may do, and how many dictionaries
    /// precede them.
    fn param_flow(&mut self, f: FnId) -> (ParamFlow, usize) {
        let fi = &self.ck.fns[f as usize];
        let dicts = fi.dicts.len();
        if let Some(p) = self.ck.flows.get(&f) {
            return (*p, dicts);
        }
        (self.signature_flow(f), dicts)
    }

    /// What a function without an analysed body (a native, a trait
    /// method's signature) may do with its parameters, by its signature.
    fn signature_flow(&mut self, f: FnId) -> ParamFlow {
        let fi = self.ck.fns[f as usize].clone();
        let mut flow = ParamFlow::default();
        for (i, p) in fi.params.iter().enumerate().take(64) {
            if fi.generics.iter().any(|&g| self.ck.arena.mentions(p.ty, g) && self.ck.arena.mentions(fi.ret, g)) {
                flow.returns |= 1 << i;
            }
        }
        if let (FnKind::Native(_), Some(owner)) = (fi.kind, fi.owner) {
            let key = (owner.native_prefix(&self.ck.types, &self.ck.syms), self.ck.name(fi.name).to_string());
            const STORING: [(&str, &str); 7] = [
                ("List", "push"),
                ("List", "insert"),
                ("List", "extend"),
                ("Map", "insert"),
                ("Map", "get_or_insert"),
                ("Set", "add"),
                ("Channel", "send"),
            ];
            if STORING.iter().any(|&(o, n)| o == key.0 && n == key.1) {
                // Everything but the receiver.
                flow.escapes |= !1 & ((1u64 << fi.params.len().min(63)) - 1);
            }
        }
        flow
    }
}

fn param_bits(origins: &[Origin], set: Set) -> u64 {
    let mut bits = 0;
    for (i, o) in origins.iter().enumerate() {
        if set & (1 << i) != 0
            && let OriginKind::Param(p) = o.kind
            && p < 64
        {
            bits |= 1 << p;
        }
    }
    bits
}

impl<'b> Flow<'b> {
    fn new(body: &'b HFn, first_param: usize) -> Flow<'b> {
        Flow {
            body,
            first_param,
            origins: Vec::new(),
            fresh: HashMap::new(),
            locals: vec![0; body.locals.len()],
            sinks: Vec::new(),
            breaks: Vec::new(),
            lambdas: Vec::new(),
            changed: false,
        }
    }

    fn origin(&mut self, kind: OriginKind, span: Span, ty: TypeId) -> Set {
        if self.origins.len() >= MAX_ORIGINS {
            return 0;
        }
        self.origins.push(Origin { kind, span, ty, binding: None, closed: false });
        1 << (self.origins.len() - 1)
    }

    /// The origin of a resource created by the expression at `span`.
    fn fresh(&mut self, span: Span, ty: TypeId) -> Set {
        let key = (span.file, span.lo, span.hi);
        if let Some(&i) = self.fresh.get(&key) {
            return 1 << i;
        }
        let set = self.origin(OriginKind::Fresh, span, ty);
        if set != 0 {
            self.fresh.insert(key, self.origins.len() - 1);
        }
        set
    }

    /// Notes that the lambdas being walked use local `l`.
    fn read(&mut self, l: LocalId) {
        if let Some((reads, _)) = self.lambdas.last_mut() {
            reads.push(l);
        }
    }

    fn flow_into(&mut self, l: LocalId, set: Set) {
        let cur = &mut self.locals[l as usize];
        if *cur | set != *cur {
            *cur |= set;
            self.changed = true;
        }
    }

    fn sink(&mut self, set: Set, span: Span, kind: SinkKind) {
        if set != 0 {
            self.sinks.push(Sink { set, span, kind });
        }
    }

    /// Binds the locals of `pat` to what the matched value carries.
    fn bind(&mut self, pat: &HPat, set: Set) {
        match &pat.kind {
            HPatKind::Bind(l) => {
                // A fresh resource is named after its first binding.
                for i in 0..self.origins.len() {
                    if set & (1 << i) != 0 && self.origins[i].kind == OriginKind::Fresh && self.origins[i].binding.is_none() {
                        self.origins[i].binding = Some(*l);
                    }
                }
                self.flow_into(*l, set);
            }
            HPatKind::Tuple(ps) | HPatKind::Or(ps) | HPatKind::Variant { args: ps, .. } => ps.iter().for_each(|p| self.bind(p, set)),
            HPatKind::Struct { fields } => fields.iter().for_each(|(_, p)| self.bind(p, set)),
            HPatKind::List { before, rest, after } => {
                before.iter().chain(after).for_each(|p| self.bind(p, set));
                if let Some(Some(l)) = rest {
                    self.flow_into(*l, set);
                }
            }
            _ => {}
        }
    }
}

impl Ctx<'_> {
    /// Walks `e`: what its value may carry, recording sinks.
    fn carry(&mut self, f: &mut Flow<'_>, e: &HExpr) -> Set {
        let set = self.carry_any(f, e);
        if set != 0 && self.is_plain(e.ty) { 0 } else { set }
    }

    fn carry_any(&mut self, f: &mut Flow<'_>, e: &HExpr) -> Set {
        if !self.ck.budget.charge(1) {
            return 0;
        }
        match &e.kind {
            HKind::Local(l) => {
                f.read(*l);
                f.locals[*l as usize]
            }
            HKind::Let { pat, init, else_ } => {
                let set = self.carry(f, init);
                if let Some(x) = else_ {
                    self.carry(f, x);
                }
                f.bind(pat, set);
                0
            }
            HKind::LocalFns { locals, lambdas } => {
                for (l, lam) in locals.iter().zip(lambdas) {
                    let set = self.carry(f, lam);
                    f.flow_into(*l, set);
                }
                0
            }
            HKind::Assign { place, value, .. } => {
                let set = self.carry(f, value);
                match place {
                    Place::Local(l) => {
                        f.read(*l);
                        f.flow_into(*l, set)
                    }
                    Place::Global(_) => f.sink(set, value.span, SinkKind::Store),
                    Place::Field { obj, .. } => {
                        self.carry(f, obj);
                        f.sink(set, value.span, SinkKind::Store);
                    }
                    Place::Index { obj, index, .. } => {
                        self.carry(f, obj);
                        self.carry(f, index);
                        f.sink(set, value.span, SinkKind::Store);
                    }
                }
                0
            }
            HKind::Block(xs) => self.block(f, xs),
            HKind::If { cond, then, else_ } => {
                self.carry(f, cond);
                let mut set = self.carry(f, then);
                if let Some(x) = else_ {
                    set |= self.carry(f, x);
                }
                set
            }
            HKind::IfLet { pat, value, then, else_ } => {
                let v = self.carry(f, value);
                f.bind(pat, v);
                let mut set = self.carry(f, then);
                if let Some(x) = else_ {
                    set |= self.carry(f, x);
                }
                set
            }
            HKind::Match { scrut, arms } => {
                let v = self.carry(f, scrut);
                let mut set = 0;
                for a in arms {
                    f.bind(&a.pat, v);
                    if let Some(g) = &a.guard {
                        self.carry(f, g);
                    }
                    set |= self.carry(f, &a.body);
                }
                set
            }
            HKind::Loop(b) => {
                f.breaks.push(0);
                self.carry(f, b);
                f.breaks.pop().unwrap_or(0)
            }
            HKind::While { cond, body } => {
                f.breaks.push(0);
                self.carry(f, cond);
                self.carry(f, body);
                f.breaks.pop();
                0
            }
            HKind::WhileLet { pat, value, body } | HKind::For { pat, iter: value, body, .. } => {
                f.breaks.push(0);
                let v = self.carry(f, value);
                f.bind(pat, v);
                self.carry(f, body);
                f.breaks.pop();
                0
            }
            HKind::ForRange { start, end, body, .. } => {
                f.breaks.push(0);
                self.carry(f, start);
                self.carry(f, end);
                self.carry(f, body);
                f.breaks.pop();
                0
            }
            HKind::Break { value, depth } => {
                if let Some(x) = value {
                    let set = self.carry(f, x);
                    let n = f.breaks.len();
                    if let Some(b) = n.checked_sub(*depth as usize + 1).and_then(|i| f.breaks.get_mut(i)) {
                        *b |= set;
                    }
                }
                0
            }
            HKind::Return(x) => {
                if let Some(x) = x {
                    let set = self.carry(f, x);
                    if f.lambdas.is_empty() {
                        f.sink(set, x.span, SinkKind::Return);
                    }
                }
                0
            }
            HKind::Call { func, args, .. } => self.call(f, Some(*func), args, e),
            HKind::CallNative { native, args, .. } => {
                let func = self.natives.get(native).copied();
                self.call(f, func, args, e)
            }
            HKind::CallTrait { trait_id, slot, args, impl_id, .. } => {
                let func = match impl_id {
                    Some(i) => self.ck.impls[*i as usize].slots.get(*slot as usize).copied(),
                    None => self.ck.traits[*trait_id as usize].methods.get(*slot as usize).map(|m| m.sig),
                };
                self.call(f, func, args, e)
            }
            HKind::CallValue { callee, args } => {
                // An unknown function: its result may refer to anything given.
                let mut set = self.carry(f, callee);
                for a in args {
                    set |= self.carry(f, a);
                }
                set
            }
            HKind::MakeStruct { fields: xs, .. } | HKind::MakeVariant { args: xs, .. } | HKind::Tuple(xs) | HKind::List(xs) => {
                let mut set = 0;
                for x in xs {
                    set |= self.carry(f, x);
                }
                self.construct(f, e, set)
            }
            HKind::Map(kvs) => {
                let mut set = 0;
                for (k, v) in kvs {
                    set |= self.carry(f, k);
                    set |= self.carry(f, v);
                }
                set
            }
            HKind::Repeat { elem, count } => {
                self.carry(f, count);
                let set = self.carry(f, elem);
                self.construct(f, e, set)
            }
            HKind::PushLocal { list, value } => {
                let set = self.carry(f, value);
                f.read(*list);
                f.flow_into(*list, set);
                0
            }
            HKind::InsertLocal { map, key, value } => {
                let set = self.carry(f, key) | self.carry(f, value);
                f.read(*map);
                f.flow_into(*map, set);
                0
            }
            HKind::Field { obj, .. } | HKind::Try { e: obj, .. } => self.carry(f, obj),
            HKind::Index { obj, index, .. } => {
                self.carry(f, index);
                self.carry(f, obj)
            }
            HKind::Slice { obj, start, end, .. } => {
                for x in [start, end].into_iter().flatten() {
                    self.carry(f, x);
                }
                self.carry(f, obj)
            }
            HKind::Coalesce { l, r, .. } => self.carry(f, l) | self.carry(f, r),
            HKind::Lambda(lam) => self.lambda(f, lam),
            HKind::Spawn(x) => {
                let set = self.carry(f, x);
                f.sink(set, x.span, SinkKind::Spawn);
                0
            }
            HKind::Defer(x) | HKind::Unary { e: x, .. } | HKind::ToStr(x) | HKind::Debug(x) => {
                self.carry(f, x);
                0
            }
            HKind::Inlined { body, .. } => self.carry(f, body),
            HKind::Arith { l, r, .. }
            | HKind::Cmp { l, r, .. }
            | HKind::And(l, r)
            | HKind::Or(l, r)
            | HKind::Range { start: l, end: r, .. } => {
                self.carry(f, l);
                self.carry(f, r);
                0
            }
            HKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        self.carry(f, x);
                    }
                }
                0
            }
            HKind::Dict { args, .. } => {
                for a in args {
                    self.carry(f, a);
                }
                0
            }
            HKind::Unit
            | HKind::Bool(_)
            | HKind::Int(_)
            | HKind::Float(_)
            | HKind::Dur(_)
            | HKind::Str(_)
            | HKind::Bytes(_)
            | HKind::Global(_)
            | HKind::FnValue { .. }
            | HKind::TypeDesc { .. }
            | HKind::VariantFn { .. }
            | HKind::Tag(_)
            | HKind::Continue { .. }
            | HKind::Evidence(_)
            | HKind::Error => 0,
        }
    }

    /// A block: a `use` binding (`let` followed by the `defer` of its
    /// `close`) becomes an origin of its own, closing what it is bound to.
    fn block(&mut self, f: &mut Flow<'_>, xs: &[HExpr]) -> Set {
        let mut set = 0;
        let mut i = 0;
        while i < xs.len() {
            if let Some(l) = self.use_binding(&xs[i], xs.get(i + 1)) {
                let HKind::Let { init, .. } = &xs[i].kind else { unreachable!() };
                let closed = self.carry(f, init);
                for (j, o) in f.origins.iter_mut().enumerate() {
                    if closed & (1 << j) != 0 && o.kind == OriginKind::Fresh {
                        o.closed = true;
                    }
                }
                let existing = f.origins.iter().position(|o| o.kind == OriginKind::Use(l));
                let own = match existing {
                    Some(j) => 1 << j,
                    None => f.origin(OriginKind::Use(l), xs[i].span, init.ty),
                };
                f.flow_into(l, own);
                // The `defer` closes it.
                i += 2;
                set = 0;
                continue;
            }
            set = self.carry(f, &xs[i]);
            i += 1;
        }
        set
    }

    /// The local `let_` binds, if it is a `use` binding: followed by the
    /// deferred `Close.close` of that local.
    fn use_binding(&self, let_: &HExpr, next: Option<&HExpr>) -> Option<LocalId> {
        let HKind::Let { pat: HPat { kind: HPatKind::Bind(l), .. }, else_: None, .. } = &let_.kind else { return None };
        let HKind::Defer(call) = &next?.kind else { return None };
        let HKind::CallTrait { trait_id, args, .. } = &call.kind else { return None };
        let recv = args.iter().rev().find_map(|a| match a {
            HArg::Expr(HExpr { kind: HKind::Local(r), .. }) => Some(*r),
            _ => None,
        });
        (*trait_id == self.close && recv == Some(*l)).then_some(*l)
    }

    fn call(&mut self, f: &mut Flow<'_>, func: Option<FnId>, args: &[HArg], e: &HExpr) -> Set {
        let (flow, dicts) = match func {
            Some(func) => self.param_flow(func),
            None => (ParamFlow::default(), 0),
        };
        let mut result = 0;
        // Whether the result may come from an argument.
        let mut derived = func.is_none();
        for (i, a) in args.iter().enumerate() {
            let HArg::Expr(x) = a else { continue };
            let set = self.carry(f, x);
            let Some(p) = i.checked_sub(dicts).filter(|&p| p < 64) else { continue };
            if flow.escapes & (1 << p) != 0 {
                f.sink(set, x.span, SinkKind::Pass(func.unwrap_or_default()));
            }
            if flow.returns & (1 << p) != 0 {
                result |= set;
                derived = true;
            }
        }
        if !derived && self.holds_resource(e.ty) { f.fresh(e.span, e.ty) } else { result }
    }

    /// A value built from values carrying `parts`: a resource of its own
    /// (which owns the resources it holds) if its type is one.
    fn construct(&mut self, f: &mut Flow<'_>, e: &HExpr, parts: Set) -> Set {
        if !self.is_resource(e.ty) {
            return parts;
        }
        let own = f.fresh(e.span, e.ty);
        f.sink(parts & !own, e.span, SinkKind::Own(e.ty));
        own
    }

    /// A lambda carries what it captures: the locals it uses that it (and
    /// the lambdas in it) does not declare.
    fn lambda(&mut self, f: &mut Flow<'_>, lam: &HLambda) -> Set {
        for (_, ids) in &mut f.lambdas {
            ids.push(lam.id);
        }
        f.lambdas.push((Vec::new(), vec![lam.id]));
        self.carry(f, &lam.body);
        let (reads, inner) = f.lambdas.pop().unwrap_or_default();
        let mut set = 0;
        for &l in &reads {
            if !inner.contains(&f.body.locals[l as usize].owner) {
                set |= f.locals[l as usize];
                // What it captures, the enclosing lambda uses.
                f.read(l);
            }
        }
        set
    }
}

impl Checker {
    /// Analyses the resource flows of the functions of the module being
    /// checked (those from `first_fn` on), recording what their parameters
    /// may do, and, for the main module when linting, the resource lints.
    pub(crate) fn resource_flows(&mut self, first_fn: usize, ast: &ast::Module) {
        let Some(close) = self.close_trait else { return };
        let lint = self.linting();
        let kind = self.mods[self.cur_mod as usize].kind;
        // The standard library's flows are kept for programs; a program's
        // own are only needed to lint it.
        if !self.lint_facts && !matches!(kind, ModKind::Std | ModKind::Prelude) {
            return;
        }
        let bodies = std::mem::take(&mut self.bodies);
        let fns: Vec<(FnId, &HFn)> = bodies.from(first_fn as FnId).collect();
        let natives = self
            .fns
            .iter()
            .enumerate()
            .filter_map(|(i, fi)| if let FnKind::Native(n) = fi.kind { Some((n, i as FnId)) } else { None })
            .collect();
        let mut ctx = Ctx { ck: self, close, natives, resource: HashMap::new(), plain: HashMap::new() };
        // Parameters' flows grow until no function's changes.
        let mut last: Vec<(FnId, Flow<'_>)> = Vec::new();
        for _ in 0..8 {
            last.clear();
            let mut changed = false;
            for &(id, body) in &fns {
                let flow = analyse(&mut ctx, id, body);
                let mut pf = ParamFlow::default();
                for s in &flow.sinks {
                    let bits = param_bits(&flow.origins, s.set);
                    match s.kind {
                        SinkKind::Return => pf.returns |= bits,
                        _ => pf.escapes |= bits,
                    }
                }
                if ctx.ck.flows.get(&id) != Some(&pf) {
                    changed = true;
                    ctx.ck.flows.insert(id, pf);
                }
                last.push((id, flow));
            }
            if !changed || ctx.ck.budget.stopped().is_some() {
                break;
            }
        }
        if lint {
            for (_, flow) in &last {
                ctx.lint_flow(flow);
            }
            ctx.lint_fields(ast);
        }
        drop(last);
        drop(fns);
        self.bodies = bodies;
    }
}

fn tail(e: &HExpr) -> &HExpr {
    match &e.kind {
        HKind::Block(xs) if !xs.is_empty() => tail(&xs[xs.len() - 1]),
        _ => e,
    }
}

/// Analyses the body of function `id` until what its locals carry is
/// settled.
fn analyse<'b>(ctx: &mut Ctx<'_>, id: FnId, body: &'b HFn) -> Flow<'b> {
    let fi = ctx.ck.fns[id as usize].clone();
    let first_param = fi.dicts.len();
    let mut f = Flow::new(body, first_param);
    for (i, p) in fi.params.iter().enumerate() {
        let Some(&l) = body.params.get(first_param + i) else { break };
        // Parameters of type parameters may be resources too.
        let generic = matches!(ctx.ck.ty(p.ty), Ty::Param(_));
        if generic || ctx.holds_resource(p.ty) {
            let set = f.origin(OriginKind::Param(i as u32), body.locals[l as usize].span, p.ty);
            f.flow_into(l, set);
        }
    }
    for _ in 0..6 {
        f.changed = false;
        f.sinks.clear();
        let value = ctx.carry(&mut f, &body.body);
        f.sink(value, tail(&body.body).span, SinkKind::Return);
        if !f.changed {
            break;
        }
    }
    f
}

impl Ctx<'_> {
    /// The `unclosed-resource` and `resource-escapes` warnings of a function.
    fn lint_flow(&mut self, f: &Flow<'_>) {
        let mut handed_on: Set = 0;
        for s in &f.sinks {
            handed_on |= s.set;
        }
        for (i, o) in f.origins.iter().enumerate() {
            if o.kind != OriginKind::Fresh || o.closed || handed_on & (1 << i) != 0 {
                continue;
            }
            let ty = self.resource_in(o.ty);
            let ty = self.ck.show(ty);
            let (span, msg) = match o.binding.map(|l| &f.body.locals[l as usize]) {
                Some(local) if local.name != Symbol::TMP => {
                    let name = self.ck.name(local.name);
                    (
                        local.span,
                        format!(
                            "`{name}` holds a resource ({ty}) that is never closed; bind it with `use {name} = ...` to close it when its block ends"
                        ),
                    )
                }
                _ => (o.span, format!("this resource ({ty}) is never closed; bind it with `use` to close it when the block ends")),
            };
            self.ck.lint.resources.push((span, "unclosed-resource", msg));
        }
        for s in &f.sinks {
            for (i, o) in f.origins.iter().enumerate() {
                if s.set & (1 << i) == 0 {
                    continue;
                }
                let (name, borrowed) = match o.kind {
                    OriginKind::Use(l) => (self.ck.name(f.body.locals[l as usize].name).to_string(), false),
                    // A parameter handed back to the caller is not kept.
                    OriginKind::Param(p) if !matches!(s.kind, SinkKind::Return) => {
                        let l = f.body.params[f.first_param + p as usize];
                        (self.ck.name(f.body.locals[l as usize].name).to_string(), true)
                    }
                    _ => continue,
                };
                let what = match s.kind {
                    SinkKind::Return => "returned".to_string(),
                    SinkKind::Store => "stored".to_string(),
                    SinkKind::Own(t) => format!("taken over by a new {}", self.ck.show(t)),
                    SinkKind::Spawn => "captured by a task".to_string(),
                    SinkKind::Pass(g) => format!("passed to `{}`, which keeps it", self.ck.name(self.ck.fns[g as usize].name)),
                };
                let why = if borrowed {
                    "it is a parameter: the caller may close it while it is kept".to_string()
                } else {
                    "it is bound with `use`, which closes it when its block ends".to_string()
                };
                let msg = format!("resource `{name}` is {what}, but {why}");
                self.ck.lint.resources.push((s.span, "resource-escapes", msg));
            }
        }
    }

    /// `resource-field`: a struct field or variant payload of the module
    /// that holds a resource, in a type that does not implement `Close`.
    fn lint_fields(&mut self, ast: &ast::Module) {
        for item in &ast.items {
            let (name, fields): (&ast::Ident, Vec<(Span, String, usize)>) = match &item.kind {
                ast::ItemKind::Struct(s) => {
                    (&s.name, s.fields.iter().enumerate().map(|(i, fd)| (fd.span, fd.name.name.clone(), i)).collect())
                }
                ast::ItemKind::Enum(en) => {
                    (&en.name, en.variants.iter().enumerate().map(|(i, v)| (v.span, v.name.name.clone(), i)).collect())
                }
                _ => continue,
            };
            let Some(Def::Type(TyRef::Def(d))) = self.ck.scope_def(self.ck.cur_mod, &name.name) else { continue };
            let def = self.ck.types[d as usize].clone();
            if def.module != self.ck.cur_mod {
                continue;
            }
            let this = {
                let args: Vec<TypeId> = def.generics.iter().map(|&g| self.ck.param_ty(g)).collect();
                self.ck.named_ty(d, &args)
            };
            if self.ck.implements_trait(this, self.close) {
                continue;
            }
            for (span, field, i) in fields {
                let tys: Vec<TypeId> = match &def.kind {
                    TypeDefKind::Struct { fields } => fields.get(i).map(|fd| vec![fd.ty]).unwrap_or_default(),
                    TypeDefKind::Enum { variants } => variants.get(i).map(|v| v.fields.clone()).unwrap_or_default(),
                    TypeDefKind::Opaque(_) => vec![],
                };
                let Some(t) = tys.into_iter().find(|&t| self.holds_resource(t)) else { continue };
                let ty = self.ck.show(t);
                let tname = &name.name;
                let what = if matches!(def.kind, TypeDefKind::Enum { .. }) { "variant" } else { "field" };
                let msg = format!(
                    "{what} `{field}` holds a resource ({ty}), but `{tname}` does not implement `Close`; implement it (closing `{field}`) so that `use` can close a `{tname}`"
                );
                self.ck.lint.resources.push((span, "resource-field", msg));
            }
        }
    }
}
