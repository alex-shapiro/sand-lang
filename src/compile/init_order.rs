//! Which globals are initialized where code runs, so that loops can load
//! immutable globals once, before they start (`Compiler::hoisted_loop`).
//!
//! Initializers run their top-level statements in order, and a statement
//! that fails ends the program, so a global is initialized after the
//! statement that declares it until the program ends. Code runs no earlier
//! than its [`Time`]: a top-level statement at its own, and a function at
//! the first statement that reaches it in a conservative call graph. A
//! statement reaches the functions it calls, makes values of (lambdas
//! included) or makes trait dictionaries from, and the functions those
//! reach: every function that can run before the statement completes.
//! The functions of other compilations, which cannot name the new ones,
//! only call what such values and dictionaries hold.
//!
//! Times of different initializers compare by the order they run in. That
//! order may differ between programs, but only for modules that do not
//! import each other, whose code cannot reach the other's globals.
//!
//! A REPL session keeps the functions of an input that fails, and the
//! globals the input did not initialize stay uninitialized, so the
//! functions of an input rely only on the globals initialized before it.

use super::walk_pruned;
use crate::check::budget::CompileBudget;
use crate::check::{CheckedProgram, FnKind};
use crate::hir::{HArg, HExpr, HFn, HKind, Place};
use crate::types::{FnId, GlobalId, ModId};
use crate::vm::value::FxHashMap;
use std::collections::HashMap;

/// A point in the run of the initializers: before them, during one, or
/// after them all.
pub(super) type Time = u64;

/// Before the compilation's code runs.
const BEGIN: Time = 0;
/// After `BEGIN`, before the initializers: when the functions of a
/// session's input may run.
const START: Time = 1;
/// After every initializer.
const END: Time = u64::MAX - 1;
/// After everything: when a global that may stay uninitialized is ready.
const NEVER: Time = u64::MAX;

/// Top-level statement `stmt` of the initializer that runs `pos`-th.
pub(super) fn at(pos: usize, stmt: usize) -> Time {
    ((pos as u64 + 1) << 32) | stmt as u64
}

/// After the initializer that runs `pos`-th.
fn after(pos: usize) -> Time {
    ((pos as u64 + 1) << 32) | u32::MAX as u64
}

pub(super) struct InitOrder {
    /// The position of each initializer compiled, in the order they run.
    inits: FxHashMap<FnId, usize>,
    /// The globals that existed before the compilation.
    old_globals: usize,
    /// When each new global is initialized.
    ready: FxHashMap<GlobalId, Time>,
    /// When each function compiled can first run (absent: after the
    /// initializers).
    first_run: FxHashMap<FnId, Time>,
    /// For a session: which old globals are initialized.
    initialized: Option<Vec<bool>>,
    /// When the initializer of each module initialized by the compilation's
    /// entry ends.
    modules: FxHashMap<ModId, Time>,
}

impl InitOrder {
    /// The order of the initializers of `bodies` (the functions compiled),
    /// those of `entry` first. Globals numbered from `old_globals` are new;
    /// `initialized` is given for a session (see the module documentation).
    pub(super) fn new(
        cp: &CheckedProgram,
        bodies: &HashMap<FnId, &HFn>,
        entry: &[FnId],
        old_globals: usize,
        initialized: Option<Vec<bool>>,
        budget: &CompileBudget,
    ) -> InitOrder {
        // The modules of a snapshot are initialized by later programs.
        let mut later: Vec<FnId> =
            bodies.keys().copied().filter(|f| matches!(cp.fns[*f as usize].kind, FnKind::Init) && !entry.contains(f)).collect();
        later.sort_unstable();
        let mut o = InitOrder {
            inits: FxHashMap::default(),
            old_globals,
            ready: FxHashMap::default(),
            first_run: FxHashMap::default(),
            modules: FxHashMap::default(),
            initialized,
        };
        let session = o.initialized.is_some();
        for (pos, &init) in entry.iter().chain(&later).enumerate() {
            o.modules.insert(cp.fns[init as usize].module, after(pos));
            let Some(body) = bodies.get(&init) else { continue };
            o.inits.insert(init, pos);
            let HKind::Block(stmts) = &body.body.kind else { continue };
            for (j, s) in stmts.iter().enumerate() {
                if let HKind::Assign { place: Place::Global(g), op: None, .. } = s.kind {
                    o.ready.insert(g, at(pos, j));
                }
                if !session {
                    o.reach(cp, bodies, s, at(pos, j), budget);
                }
            }
        }
        o
    }

    /// Records the functions `e` reaches as running from `t` on, unless
    /// they run earlier.
    fn reach(&mut self, cp: &CheckedProgram, bodies: &HashMap<FnId, &HFn>, e: &HExpr, t: Time, budget: &CompileBudget) {
        let walk = |e: &HExpr, found: &mut Vec<FnId>| {
            walk_pruned(e, &mut |x| {
                callees(cp, x, found);
                budget.charge(1)
            })
        };
        let mut found = Vec::new();
        walk(e, &mut found);
        while let Some(f) = found.pop() {
            if let Some(body) = bodies.get(&f)
                && !self.inits.contains_key(&f)
                && !self.first_run.contains_key(&f)
            {
                self.first_run.insert(f, t);
                walk(&body.body, &mut found);
            }
        }
    }

    /// The position of initializer `f` in the order they run.
    pub(super) fn init_pos(&self, f: FnId) -> Option<usize> {
        self.inits.get(&f).copied()
    }

    /// When function `f` can first run (an initializer: at its first
    /// statement).
    pub(super) fn first_run(&self, f: FnId) -> Time {
        if let Some(&pos) = self.inits.get(&f) {
            return at(pos, 0);
        }
        match self.initialized {
            Some(_) => START,
            None => self.first_run.get(&f).copied().unwrap_or(END),
        }
    }

    /// Whether global `g` is initialized wherever code that runs from `t`
    /// on reads it.
    pub(super) fn ready(&self, cp: &CheckedProgram, g: GlobalId, t: Time) -> bool {
        let at = if g as usize >= self.old_globals {
            self.ready.get(&g).copied().unwrap_or(NEVER)
        } else {
            match &self.initialized {
                None => BEGIN,
                Some(flags) if flags.get(g as usize) == Some(&true) => BEGIN,
                Some(_) => self.modules.get(&cp.globals[g as usize].module).copied().unwrap_or(NEVER),
            }
        };
        at < t
    }
}

/// Adds to `out` the functions that `x` itself calls, makes a value of or
/// makes a trait dictionary from.
fn callees(cp: &CheckedProgram, x: &HExpr, out: &mut Vec<FnId>) {
    let defaults = |args: &[HArg], out: &mut Vec<FnId>| {
        out.extend(args.iter().filter_map(|a| if let HArg::Default { helper, .. } = a { Some(*helper) } else { None }))
    };
    match &x.kind {
        HKind::Call { func, args, .. } => {
            out.push(*func);
            defaults(args, out);
        }
        HKind::CallNative { args, .. } => defaults(args, out),
        HKind::CallTrait { args, impl_id, .. } => {
            defaults(args, out);
            if let Some(i) = impl_id {
                out.extend(&cp.impls[*i as usize].slots);
            }
        }
        HKind::FnValue { func: f, .. } | HKind::Inlined { func: f, .. } => out.push(*f),
        HKind::Dict { impl_id, .. } => out.extend(&cp.impls[*impl_id as usize].slots),
        _ => {}
    }
}
