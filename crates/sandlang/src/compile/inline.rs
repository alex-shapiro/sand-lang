//! Function inlining for small non-generic functions without early returns or closures.
//!
//! Inlined calls expand to:
//!
//! ```text
//! { let p1 = a1; let p2 = a2; body }
//! ```
//!
//! Inlining appends callee locals to caller locals and marks inlined bodies with `HKind::Inlined`.
//! Immutable local arguments replace parameter bindings directly.

use crate::check::budget::CompileBudget;
use crate::check::persist::PMap;
use crate::check::{CheckedProgram, FnKind};
use crate::hir::*;
use crate::types::{FnId, TypeId};
use std::sync::Arc;

/// Bodies of at most this many expression nodes are inlined.
const MAX_NODES: usize = 24;
/// Calls inside inlined bodies are inlined this many levels deep.
const MAX_DEPTH: u32 = 2;

/// The body of function `f` if calls to it can be inlined.
pub(super) fn inlinable(cp: &CheckedProgram, f: FnId, body: &HFn) -> Option<Arc<HFn>> {
    let fi = &cp.fns[f as usize];
    if !matches!(fi.kind, FnKind::Script)
        || !fi.generics.is_empty()
        || !fi.dicts.is_empty()
        || fi.params.iter().any(|p| p.default.is_some())
    {
        return None;
    }
    let mut nodes = 0;
    let mut ok = true;
    let mut body = body.clone();
    body.body.visit_mut(&mut |e| {
        nodes += 1;
        ok &= nodes <= MAX_NODES
            && !matches!(
                e.kind,
                HKind::Return(_)
                    | HKind::Try { .. }
                    | HKind::Lambda(_)
                    | HKind::LocalFns { .. }
                    | HKind::Spawn(_)
                    | HKind::Defer(_)
                    | HKind::Loop(_)
                    | HKind::While { .. }
                    | HKind::WhileLet { .. }
                    | HKind::For { .. }
                    | HKind::ForRange { .. }
                    | HKind::Evidence(_)
                    | HKind::Error
            )
            && !matches!(e.kind, HKind::Call { func, .. } if func == f);
    });
    if !ok {
        return None;
    }
    if fi.ret == TypeId::UNIT && body.body.ty != TypeId::UNIT {
        let span = body.body.span;
        let b = std::mem::replace(&mut body.body, HExpr { kind: HKind::Unit, ty: TypeId::UNIT, span });
        body.body = HExpr { kind: HKind::Block(vec![b, HExpr { kind: HKind::Unit, ty: TypeId::UNIT, span }]), ty: TypeId::UNIT, span };
    }
    Some(Arc::new(body))
}

/// `body` with the calls to the functions of `table` inlined, or `None` if
/// it calls none of them.
pub(super) fn inline_calls(body: &HFn, table: &PMap<FnId, Arc<HFn>>, budget: &CompileBudget) -> Option<HFn> {
    if table.is_empty() {
        return None;
    }
    let mut out = body.clone();
    let mut cx = Cx { table, budget, locals: &mut out.locals, changed: false };
    cx.rewrite(&mut out.body, 0, 0);
    cx.changed.then_some(out)
}

struct Cx<'a> {
    table: &'a PMap<FnId, Arc<HFn>>,
    budget: &'a CompileBudget,
    locals: &'a mut Vec<LocalInfo>,
    changed: bool,
}

impl Cx<'_> {
    /// Inlines the calls in `e`, which is in the lambda `owner` (0: the
    /// function itself).
    fn rewrite(&mut self, e: &mut HExpr, owner: u32, depth: u32) {
        match &mut e.kind {
            HKind::Lambda(l) => {
                let id = l.id;
                self.rewrite(&mut l.body, id, depth);
                return;
            }
            _ => e.for_each_part(&mut |p| {
                if let Part::Expr(x) = p {
                    self.rewrite(x, owner, depth);
                }
            }),
        }
        let HKind::Call { func, args, .. } = &e.kind else { return };
        if depth >= MAX_DEPTH || args.iter().any(|a| !matches!(a, HArg::Expr(_))) {
            return;
        }
        let func = *func;
        let Some(callee) = self.table.get(&func).cloned() else { return };
        if callee.params.len() != args.len() || !self.budget.charge(callee.locals.len() as u64 + MAX_NODES as u64) {
            return;
        }
        let HKind::Call { args, .. } = std::mem::replace(&mut e.kind, HKind::Unit) else { unreachable!() };
        // The callee's local `l` is the caller's `map[l]`.
        let base = self.locals.len() as LocalId;
        let mut map: Vec<LocalId> = (0..callee.locals.len() as LocalId).map(|l| l + base).collect();
        self.locals.extend(callee.locals.iter().map(|l| LocalInfo { owner, ..l.clone() }));
        let mut items = Vec::new();
        for (&p, a) in callee.params.iter().zip(args) {
            let HArg::Expr(a) = a else { unreachable!() };
            match a.kind {
                HKind::Local(l)
                    if !self.locals[l as usize].mutable && !self.locals[l as usize].cell && !callee.locals[p as usize].mutable =>
                {
                    map[p as usize] = l;
                }
                _ => {
                    let local = map[p as usize];
                    let span = a.span;
                    items.push(HExpr {
                        kind: HKind::Let { pat: HPat { kind: HPatKind::Bind(local), ty: a.ty, span }, init: Box::new(a), else_: None },
                        ty: TypeId::UNIT,
                        span,
                    });
                }
            }
        }
        let mut b = callee.body.clone();
        relocate(&mut b, &map);
        self.rewrite(&mut b, owner, depth + 1);
        let ty = b.ty;
        items.push(HExpr { kind: HKind::Inlined { func, body: Box::new(b) }, ty, span: e.span });
        e.kind = HKind::Block(items);
        self.changed = true;
    }
}

/// Renames the locals of an inlined body.
fn relocate(e: &mut HExpr, map: &[LocalId]) {
    match &mut e.kind {
        HKind::Local(l) => *l = map[*l as usize],
        HKind::Assign { place: Place::Local(l), .. } => *l = map[*l as usize],
        HKind::PushLocal { list: l, .. } | HKind::InsertLocal { map: l, .. } => *l = map[*l as usize],
        _ => {}
    }
    e.for_each_part(&mut |p| match p {
        Part::Expr(x) => relocate(x, map),
        Part::Pat(p) => relocate_pat(p, map),
        Part::Type(_) => {}
    });
}

fn relocate_pat(p: &mut HPat, map: &[LocalId]) {
    match &mut p.kind {
        HPatKind::Bind(l) | HPatKind::List { rest: Some(Some(l)), .. } => *l = map[*l as usize],
        _ => {}
    }
    p.for_each_subpattern(&mut |q| relocate_pat(q, map));
}
