//! Pattern checking, exhaustiveness and reachability (Maranget's usefulness algorithm).

use super::*;
use crate::syntax::ast::{PatLit, Pattern, PatternKind};

/// A witness of a value not matched by a set of patterns.
#[derive(Clone, Debug)]
pub enum Witness {
    Wild,
    Ctor(SCtor, Vec<Witness>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SCtor {
    Bool(bool),
    /// Variant `i` of enum `d`.
    Variant(TypeDefId, u32),
    Tuple(usize),
    /// A struct (if its type is known) with its number of fields.
    Struct(usize, Option<TypeDefId>),
    Unit,
    Int(i64),
    IntRange(i64, i64),
    Str(String),
    ListExact(usize),
    /// At least `prefix + suffix` elements.
    ListAtLeast(usize, usize),
}

#[derive(Clone, Debug)]
enum SPat {
    Wild,
    Ctor(SCtor, Vec<SPat>),
    Or(Vec<SPat>),
}

/// The names bound so far by the pattern being checked.
#[derive(Default)]
struct Binds {
    /// Each name's position in `order`.
    pos: HashMap<Symbol, usize>,
    order: Vec<Symbol>,
    /// Names before this position are bound outside the or-pattern
    /// alternative being checked. In an alternative after the first, binding
    /// one again is not reported as a duplicate, since it is reported either
    /// in the first alternative or as a name the first does not bind.
    floor: usize,
    /// In an or-pattern alternative after the first, the locals that the
    /// first alternative binds.
    first_alt: Option<HashMap<Symbol, LocalId>>,
}

impl Binds {
    /// Records `name`. Returns false if it is already bound (see `floor`).
    fn add(&mut self, name: Symbol) -> bool {
        if let Some(&i) = self.pos.get(&name) {
            return i < self.floor;
        }
        self.pos.insert(name, self.order.len());
        self.order.push(name);
        true
    }

    fn truncate(&mut self, len: usize) {
        for name in self.order.drain(len..) {
            self.pos.remove(&name);
        }
    }
}

impl Checker {
    pub(crate) fn check_pattern(&mut self, p: &Pattern, ty: TypeId, mutable: bool) -> HPat {
        self.check_pat_inner(p, ty, mutable, &mut Binds::default())
    }

    /// Checks that a pattern of type `pat_ty` can match values of type
    /// `scrut`, reporting a mismatch. A `never` scrutinee matches anything
    /// (its expression diverges).
    fn expect_scrutinee(&mut self, scrut: TypeId, pat_ty: TypeId, span: Span) -> bool {
        self.resolve(scrut).is_never() || self.expect_type(scrut, pat_ty, span)
    }

    fn pat(&self, kind: HPatKind, ty: TypeId, span: Span) -> HPat {
        HPat { kind, ty, span }
    }

    fn bind_name(&mut self, id: &ast::Ident, ty: TypeId, mutable: bool, binds: &mut Binds) -> HPat {
        let name = self.sym(&id.name);
        if !binds.add(name) {
            self.error(id.span, format!("`{}` is bound more than once in the pattern", id.name));
        }
        if let Some(map) = &binds.first_alt {
            match map.get(&name) {
                Some(&l) => {
                    let lt = self.fs.data[l as usize].ty;
                    self.expect_type(lt, ty, id.span);
                    return self.pat(HPatKind::Bind(l), ty, id.span);
                }
                None => {
                    self.error(
                        id.span,
                        format!("all alternatives of an or-pattern must bind the same names; `{}` is not bound in the first", id.name),
                    );
                    return self.pat(HPatKind::Wildcard, ty, id.span);
                }
            }
        }
        let l = self.declare_local_sym(name, ty, mutable, id.span, false);
        self.pat(HPatKind::Bind(l), ty, id.span)
    }

    fn fresh_args(&mut self, d: TypeDefId) -> Vec<TypeId> {
        let n = self.types[d as usize].generics.len();
        (0..n).map(|_| self.fresh()).collect()
    }

    /// Resolves a variant path in a pattern against the scrutinee type.
    fn pattern_variant(&mut self, path: &[ast::Ident], ty: TypeId) -> Option<(TypeDefId, u32, Vec<TypeId>)> {
        let rt = self.resolve(ty);
        let last = path.last().unwrap();
        if path.len() == 1 {
            if let Ty::Named(d, args) = self.ty(rt)
                && let Some(i) = self.find_variant_named(d, &last.name)
            {
                return Some((d, i, self.elems(args)));
            }
            match self.lookup_global(&last.name) {
                Some(Def::Variant(d, i)) => {
                    let args = self.fresh_args(d);
                    let t = self.named_ty(d, &args);
                    if !self.expect_scrutinee(rt, t, last.span) {
                        return None;
                    }
                    return Some((d, i, args));
                }
                _ => {
                    let msg = if matches!(self.ty(rt), Ty::Named(d, _) if self.types[d as usize].is_enum()) {
                        format!("no variant `{}` on type {}", last.name, self.show(rt))
                    } else {
                        format!("unknown variant `{}`", last.name)
                    };
                    self.error(last.span, msg);
                    return None;
                }
            }
        }
        // Qualified: `Enum.Variant` or `module.Enum.Variant`.
        let type_path = &path[..path.len() - 1];
        let d = match self.resolve_struct_path_quiet(type_path) {
            Some(d) => d,
            None => {
                let name = type_path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
                self.error(type_path[0].span, format!("unknown type `{name}`"));
                return None;
            }
        };
        let Some(i) = self.find_variant_named(d, &last.name) else {
            let msg = format!("no variant `{}` on type {}", last.name, self.name(self.types[d as usize].name));
            self.error(last.span, msg);
            return None;
        };
        let args = self.fresh_args(d);
        let t = self.named_ty(d, &args);
        if !self.expect_scrutinee(rt, t, last.span) {
            return None;
        }
        Some((d, i, args))
    }

    fn find_variant_named(&self, d: TypeDefId, name: &str) -> Option<u32> {
        let s = self.syms.get(name)?;
        self.types[d as usize].variants().iter().position(|v| v.name == s).map(|i| i as u32)
    }

    fn resolve_struct_path_quiet(&mut self, path: &[ast::Ident]) -> Option<TypeDefId> {
        if path.len() == 1 {
            if path[0].name == "Self" {
                if let Some(Ty::Named(d, _)) = self.fs.self_ty.map(|t| self.ty(t)) {
                    return Some(d);
                }
                return None;
            }
            match self.lookup_global(&path[0].name)? {
                Def::Type(TyRef::Def(d)) => Some(d),
                _ => None,
            }
        } else if path.len() == 2 {
            match self.lookup_global(&path[0].name)? {
                Def::Module(m) => {
                    self.note_name_used(&path[0].name);
                    let found = self.syms.get(&path[1].name).and_then(|s| self.mods[m as usize].scope.get(&s).copied());
                    match found {
                        Some(Binding { def: Def::Type(TyRef::Def(d)), is_pub: true, .. }) => Some(d),
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn check_pat_inner(&mut self, p: &Pattern, ty: TypeId, mutable: bool, binds: &mut Binds) -> HPat {
        let span = p.span;
        match &p.kind {
            PatternKind::Wildcard => self.pat(HPatKind::Wildcard, ty, span),
            PatternKind::Binding(id) => {
                // A payload-less variant of the scrutinee's enum?
                let rt = self.resolve(ty);
                if let Ty::Named(d, _) = self.ty(rt)
                    && let Some(i) = self.find_variant_named(d, &id.name)
                    && self.types[d as usize].variants()[i as usize].fields.is_empty()
                {
                    let rt_id = self.types[d as usize].variants()[i as usize].rt_id;
                    return self.pat(HPatKind::Variant { rt_id, args: vec![] }, ty, span);
                }
                if let Some(Def::Variant(d, i)) = self.scope_def(self.prelude.unwrap_or(0), &id.name) {
                    let v = &self.types[d as usize].variants()[i as usize];
                    if v.fields.is_empty() {
                        let rt_id = v.rt_id;
                        let args = self.fresh_args(d);
                        let t = self.named_ty(d, &args);
                        self.expect_scrutinee(rt, t, span);
                        return self.pat(HPatKind::Variant { rt_id, args: vec![] }, ty, span);
                    }
                }
                self.bind_name(id, ty, mutable, binds)
            }
            PatternKind::Literal(lit) => {
                let (kind, lt) = match lit {
                    PatLit::Int(i) => (HPatKind::Int(*i), TypeId::INT),
                    PatLit::Str(s) => (HPatKind::Str(s.clone()), TypeId::STR),
                    PatLit::Bool(b) => (HPatKind::Bool(*b), TypeId::BOOL),
                    PatLit::Unit => (HPatKind::Unit, TypeId::UNIT),
                };
                self.expect_scrutinee(ty, lt, span);
                self.pat(kind, ty, span)
            }
            PatternKind::Range(a, b) => {
                self.expect_scrutinee(ty, TypeId::INT, span);
                if a > b {
                    self.error(span, "empty range pattern");
                }
                self.pat(HPatKind::Range(*a, *b), ty, span)
            }
            PatternKind::Tuple(ps) => {
                let rt = self.resolve(ty);
                let tys: Vec<TypeId> = match self.ty(rt) {
                    Ty::Tuple(ts) if self.arena.list_len(ts) == ps.len() => self.elems(ts),
                    Ty::Var(_) | Ty::Never => {
                        let ts: Vec<TypeId> = ps.iter().map(|_| self.fresh()).collect();
                        let t = self.tuple_ty(&ts);
                        self.unify(ty, t);
                        ts
                    }
                    Ty::Error => vec![TypeId::ERROR; ps.len()],
                    _ => {
                        let msg = format!("type mismatch: expected {}, found a tuple pattern of {} elements", self.show(rt), ps.len());
                        self.error(span, msg);
                        vec![TypeId::ERROR; ps.len()]
                    }
                };
                let hs = ps.iter().zip(tys).map(|(p, t)| self.check_pat_inner(p, t, mutable, binds)).collect();
                self.pat(HPatKind::Tuple(hs), ty, span)
            }
            PatternKind::Variant { path, args } => {
                let Some((d, i, targs)) = self.pattern_variant(path, ty) else {
                    if let Some(args) = args {
                        for a in args {
                            self.check_pat_inner(a, TypeId::ERROR, mutable, binds);
                        }
                    }
                    return self.pat(HPatKind::Wildcard, ty, span);
                };
                let rt_id = self.types[d as usize].variants()[i as usize].rt_id;
                let ftys = self.variant_field_types(d, i, &targs);
                let args = args.as_deref().unwrap_or_default();
                if args.len() != ftys.len() {
                    let msg = format!(
                        "variant `{}` has {} field{}, but the pattern has {}",
                        self.name(self.types[d as usize].variants()[i as usize].name),
                        ftys.len(),
                        if ftys.len() == 1 { "" } else { "s" },
                        args.len()
                    );
                    self.error(span, msg);
                    return self.pat(HPatKind::Wildcard, ty, span);
                }
                let hs = args.iter().zip(ftys).map(|(p, t)| self.check_pat_inner(p, t, mutable, binds)).collect();
                self.pat(HPatKind::Variant { rt_id, args: hs }, ty, span)
            }
            PatternKind::Struct { path, fields, rest } => {
                let Some(d) = self.resolve_struct_path_quiet(path) else {
                    let name = path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
                    self.error(path[0].span, format!("unknown struct `{name}`"));
                    return self.pat(HPatKind::Wildcard, ty, span);
                };
                if !self.types[d as usize].is_struct() {
                    let msg = format!("`{}` is not a struct", self.name(self.types[d as usize].name));
                    self.error(path[0].span, msg);
                    return self.pat(HPatKind::Wildcard, ty, span);
                }
                let targs = self.fresh_args(d);
                let st = self.named_ty(d, &targs);
                self.expect_scrutinee(ty, st, span);
                let mut hs = Vec::new();
                let mut seen = vec![false; self.types[d as usize].fields().len()];
                for f in fields {
                    let sym = self.syms.get(&f.name.name);
                    let Some(idx) = self.types[d as usize].fields().iter().position(|x| Some(x.name) == sym) else {
                        let msg = format!("no field `{}` on type {}", f.name.name, self.name(self.types[d as usize].name));
                        self.error(f.name.span, msg);
                        continue;
                    };
                    seen[idx] = true;
                    let fty = self.field_type(d, idx, &targs);
                    let h = match &f.pattern {
                        Some(p) => self.check_pat_inner(p, fty, mutable, binds),
                        None => self.bind_name(&f.name, fty, mutable, binds),
                    };
                    hs.push((idx as u32, h));
                }
                if !rest {
                    for i in seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| i) {
                        let fname = self.name(self.types[d as usize].fields()[i].name);
                        let msg = format!("missing field `{fname}` in pattern; add `..` to ignore the rest");
                        self.error(span, msg);
                    }
                }
                self.pat(HPatKind::Struct { fields: hs }, ty, span)
            }
            PatternKind::List { before, rest, after } => {
                let rt = self.resolve(ty);
                let elem = match self.ty(rt) {
                    Ty::List(e) => e,
                    Ty::Var(_) | Ty::Never => {
                        let e = self.fresh();
                        let t = self.list_ty(e);
                        self.unify(ty, t);
                        e
                    }
                    Ty::Error => TypeId::ERROR,
                    _ => {
                        let msg = format!("type mismatch: expected {}, found a list pattern", self.show(rt));
                        self.error(span, msg);
                        TypeId::ERROR
                    }
                };
                let b = before.iter().map(|p| self.check_pat_inner(p, elem, mutable, binds)).collect();
                let r = match rest {
                    Some(Some(id)) => {
                        let lt = self.list_ty(elem);
                        match self.bind_name(id, lt, mutable, binds).kind {
                            HPatKind::Bind(l) => Some(Some(l)),
                            _ => Some(None),
                        }
                    }
                    Some(None) => Some(None),
                    None => None,
                };
                let a = after.iter().map(|p| self.check_pat_inner(p, elem, mutable, binds)).collect();
                self.pat(HPatKind::List { before: b, rest: r, after: a }, ty, span)
            }
            PatternKind::Or(alts) => {
                let outer = binds.order.len();
                let first = self.check_pat_inner(&alts[0], ty, mutable, binds);
                let first_names = binds.order[outer..].to_vec();
                let names: HashMap<Symbol, LocalId> = self.pattern_bindings(&first).into_iter().collect();
                let saved = (std::mem::replace(&mut binds.floor, outer), binds.first_alt.take());
                let mut hs = vec![first];
                for alt in &alts[1..] {
                    binds.truncate(outer);
                    binds.first_alt = Some(names.clone());
                    let h = self.check_pat_inner(alt, ty, mutable, binds);
                    let bound: HashSet<Symbol> = self.pattern_bindings(&h).into_iter().map(|(n, _)| n).collect();
                    let mut missing: Vec<&Symbol> = names.keys().filter(|n| !bound.contains(n)).collect();
                    missing.sort_by_key(|n| names[n]);
                    for n in missing {
                        let msg = format!("all alternatives of an or-pattern must bind the same names; `{}` is missing", self.name(*n));
                        self.error(alt.span, msg);
                    }
                    hs.push(h);
                }
                binds.truncate(outer);
                (binds.floor, binds.first_alt) = saved;
                for n in first_names {
                    binds.add(n);
                }
                self.pat(HPatKind::Or(hs), ty, span)
            }
        }
    }

    pub(crate) fn pattern_bindings(&self, p: &HPat) -> Vec<(Symbol, LocalId)> {
        let mut out = Vec::new();
        fn go(c: &Checker, p: &HPat, out: &mut Vec<(Symbol, LocalId)>) {
            match &p.kind {
                HPatKind::Bind(l) => {
                    let n = c.fs.locals[*l as usize].name;
                    if !out.iter().any(|(_, x)| x == l) {
                        out.push((n, *l));
                    }
                }
                HPatKind::Tuple(ps) => ps.iter().for_each(|x| go(c, x, out)),
                HPatKind::Or(ps) => {
                    if let Some(f) = ps.first() {
                        go(c, f, out)
                    }
                }
                HPatKind::Variant { args, .. } => args.iter().for_each(|x| go(c, x, out)),
                HPatKind::Struct { fields } => fields.iter().for_each(|(_, x)| go(c, x, out)),
                HPatKind::List { before, rest, after } => {
                    before.iter().for_each(|x| go(c, x, out));
                    if let Some(Some(l)) = rest {
                        out.push((c.fs.locals[*l as usize].name, *l));
                    }
                    after.iter().for_each(|x| go(c, x, out));
                }
                _ => {}
            }
        }
        go(self, p, &mut out);
        out
    }

    pub(crate) fn pattern_names(&self, p: &HPat) -> Vec<Symbol> {
        self.pattern_bindings(p).into_iter().map(|(n, _)| n).collect()
    }

    // ------------------------------------------------------------------
    // Exhaustiveness

    fn simplify(&self, p: &HPat) -> SPat {
        match &p.kind {
            HPatKind::Wildcard | HPatKind::Bind(_) => SPat::Wild,
            HPatKind::Int(i) => SPat::Ctor(SCtor::Int(*i), vec![]),
            HPatKind::Range(a, b) => SPat::Ctor(SCtor::IntRange(*a, *b), vec![]),
            HPatKind::Str(s) => SPat::Ctor(SCtor::Str(s.clone()), vec![]),
            HPatKind::Bool(b) => SPat::Ctor(SCtor::Bool(*b), vec![]),
            HPatKind::Unit => SPat::Ctor(SCtor::Unit, vec![]),
            HPatKind::Tuple(ps) => SPat::Ctor(SCtor::Tuple(ps.len()), ps.iter().map(|x| self.simplify(x)).collect()),
            HPatKind::Variant { rt_id, args } => {
                let (d, i) = self.variants[*rt_id as usize];
                SPat::Ctor(SCtor::Variant(d, i), args.iter().map(|x| self.simplify(x)).collect())
            }
            HPatKind::Struct { fields } => {
                let (n, def) = match self.rty(p.ty) {
                    Ty::Named(d, _) => (self.types[d as usize].fields().len(), Some(d)),
                    _ => (0, None),
                };
                let mut args = vec![SPat::Wild; n];
                for (i, f) in fields {
                    if (*i as usize) < n {
                        args[*i as usize] = self.simplify(f);
                    }
                }
                SPat::Ctor(SCtor::Struct(n, def), args)
            }
            HPatKind::List { before, rest, after } => {
                let mut args: Vec<SPat> = before.iter().map(|x| self.simplify(x)).collect();
                args.extend(after.iter().map(|x| self.simplify(x)));
                match rest {
                    None => SPat::Ctor(SCtor::ListExact(before.len()), args),
                    Some(_) => SPat::Ctor(SCtor::ListAtLeast(before.len(), after.len()), args),
                }
            }
            HPatKind::Or(ps) => SPat::Or(ps.iter().map(|x| self.simplify(x)).collect()),
        }
    }

    pub(crate) fn pattern_irrefutable(&mut self, p: &HPat) -> bool {
        // Most `let`s only bind names: no need to check.
        fn binds_only(p: &HPat) -> bool {
            match &p.kind {
                HPatKind::Wildcard | HPatKind::Bind(_) => true,
                HPatKind::Tuple(ps) => ps.iter().all(binds_only),
                HPatKind::Struct { fields } => fields.iter().all(|(_, p)| binds_only(p)),
                _ => false,
            }
        }
        if binds_only(p) {
            return true;
        }
        let row = vec![self.simplify(p)];
        let ty = self.zonk(p.ty);
        if matches!(self.ty(ty), Ty::Never | Ty::Error) {
            return true;
        }
        self.budget.start_pattern_check();
        let irrefutable = self.useful(&[row], &[SPat::Wild], &[ty]).is_none();
        if self.budget.pattern_too_complex() {
            self.error(p.span, "pattern too complex to check for exhaustiveness");
            return true;
        }
        irrefutable
    }

    pub(crate) fn check_exhaustive(&mut self, ty: TypeId, arms: &[HArm], span: Span) {
        let ty = self.zonk(ty);
        // No values (`never`), or an error has been reported already.
        if matches!(self.ty(ty), Ty::Never | Ty::Error) {
            return;
        }
        let mut rows: Vec<Vec<SPat>> = Vec::new();
        self.budget.start_pattern_check();
        for arm in arms {
            if self.budget.pattern_too_complex() {
                break;
            }
            let sp = self.simplify(&arm.pat);
            if self.useful(&rows, std::slice::from_ref(&sp), &[ty]).is_none() && !self.budget.pattern_too_complex() {
                self.error(arm.pat.span, "unreachable pattern");
            }
            if arm.guard.is_none() {
                rows.push(vec![sp]);
            }
        }
        let missing = if self.budget.pattern_too_complex() { None } else { self.useful(&rows, &[SPat::Wild], &[ty]) };
        if self.budget.pattern_too_complex() {
            self.error(span, "pattern too complex to check for exhaustiveness");
        } else if let Some(w) = missing {
            let text = self.show_witness(&w[0]);
            self.error(span, format!("non-exhaustive match: missing `{text}`"));
        }
    }

    fn show_witness(&self, w: &Witness) -> String {
        match w {
            Witness::Wild => "_".into(),
            Witness::Ctor(c, args) => {
                let a: Vec<String> = args.iter().map(|x| self.show_witness(x)).collect();
                match c {
                    SCtor::Bool(b) => b.to_string(),
                    SCtor::Unit => "()".into(),
                    SCtor::Variant(d, i) => {
                        let name = self.name(self.types[*d as usize].variants()[*i as usize].name);
                        if a.is_empty() { name.to_string() } else { format!("{name}({})", a.join(", ")) }
                    }
                    SCtor::Tuple(_) => format!("({})", a.join(", ")),
                    SCtor::Struct(_, d) => format!("{} {{ .. }}", d.map_or("_", |d| self.name(self.types[d as usize].name))),
                    SCtor::ListExact(_) => format!("[{}]", a.join(", ")),
                    SCtor::ListAtLeast(..) => {
                        let mut parts = a.clone();
                        parts.push("..".into());
                        format!("[{}]", parts.join(", "))
                    }
                    SCtor::Int(i) => i.to_string(),
                    SCtor::IntRange(x, y) => format!("{x}..={y}"),
                    SCtor::Str(s) => format!("{s:?}"),
                }
            }
        }
    }

    /// Constructor arity and argument types for the column type `ty`.
    fn ctor_args(&mut self, c: &SCtor, ty: TypeId) -> Vec<TypeId> {
        let ty = self.rty(ty);
        match c {
            SCtor::Tuple(n) => match ty {
                Ty::Tuple(ts) => self.elems(ts),
                _ => vec![TypeId::ERROR; *n],
            },
            SCtor::Variant(_, i) => match ty {
                Ty::Named(d, args) => {
                    let args = self.elems(args);
                    self.variant_field_types(d, *i, &args)
                }
                _ => vec![],
            },
            SCtor::Struct(n, _) => match ty {
                Ty::Named(d, args) => {
                    let args = self.elems(args);
                    (0..self.types[d as usize].fields().len()).map(|i| self.field_type(d, i, &args)).collect()
                }
                _ => vec![TypeId::ERROR; *n],
            },
            SCtor::ListExact(n) => {
                let e = match ty {
                    Ty::List(e) => e,
                    _ => TypeId::ERROR,
                };
                vec![e; *n]
            }
            SCtor::ListAtLeast(k, m) => {
                let e = match ty {
                    Ty::List(e) => e,
                    _ => TypeId::ERROR,
                };
                vec![e; k + m]
            }
            _ => vec![],
        }
    }

    /// All constructors of a type, if finite and known.
    fn all_ctors(&self, ty: TypeId, rows: &[Vec<SPat>]) -> Option<Vec<SCtor>> {
        match self.rty(ty) {
            Ty::Bool => Some(vec![SCtor::Bool(true), SCtor::Bool(false)]),
            Ty::Unit => Some(vec![SCtor::Unit]),
            Ty::Tuple(ts) => Some(vec![SCtor::Tuple(self.arena.list_len(ts))]),
            Ty::Named(d, _) => {
                let def = &self.types[d as usize];
                match &def.kind {
                    TypeDefKind::Enum { variants } => Some((0..variants.len()).map(|i| SCtor::Variant(d, i as u32)).collect()),
                    TypeDefKind::Struct { fields } => Some(vec![SCtor::Struct(fields.len(), Some(d))]),
                    TypeDefKind::Opaque => None,
                }
            }
            Ty::List(_) => {
                let l = max_list_len(rows);
                let mut v: Vec<SCtor> = (0..=l).map(SCtor::ListExact).collect();
                v.push(SCtor::ListAtLeast(l + 1, 0));
                Some(v)
            }
            _ => None,
        }
    }

    /// Specializes a row by constructor `c`.
    fn specialize_row(&self, row: &[SPat], c: &SCtor, arity: usize) -> Option<Vec<SPat>> {
        let rest = &row[1..];
        let head: Vec<SPat> = match &row[0] {
            SPat::Wild => vec![SPat::Wild; arity],
            SPat::Ctor(rc, args) => match (rc, c) {
                (SCtor::ListAtLeast(k, m), SCtor::ListExact(n)) => {
                    if *n < k + m {
                        return None;
                    }
                    let mut v = args[..*k].to_vec();
                    v.extend(std::iter::repeat_n(SPat::Wild, n - k - m));
                    v.extend(args[*k..].iter().cloned());
                    v
                }
                (SCtor::ListAtLeast(k, m), SCtor::ListAtLeast(n, 0)) => {
                    if *n < k + m {
                        return None;
                    }
                    let mut v = args[..*k].to_vec();
                    v.extend(std::iter::repeat_n(SPat::Wild, n - k - m));
                    v.extend(args[*k..].iter().cloned());
                    v
                }
                (SCtor::ListExact(_), SCtor::ListAtLeast(..)) => return None,
                (SCtor::IntRange(a, b), SCtor::Int(i)) => {
                    if a <= i && i <= b {
                        vec![]
                    } else {
                        return None;
                    }
                }
                _ => {
                    if rc == c {
                        // Same length as `arity` unless the type did not determine it.
                        let mut a = args.clone();
                        a.resize(arity, SPat::Wild);
                        a
                    } else {
                        return None;
                    }
                }
            },
            SPat::Or(_) => unreachable!(),
        };
        let mut out = head;
        out.extend(rest.iter().cloned());
        Some(out)
    }

    /// Expands or-patterns in the first column. Each row produced costs a
    /// unit of pattern work; expansion stops when the check is too complex.
    fn expand_or(&self, rows: &[Vec<SPat>]) -> Vec<Vec<SPat>> {
        let mut out = Vec::new();
        for r in rows {
            if !self.budget.pattern_step(r.len() as u64) {
                break;
            }
            if let Some(SPat::Or(alts)) = r.first() {
                for a in alts {
                    let mut nr = vec![a.clone()];
                    nr.extend(r[1..].iter().cloned());
                    out.extend(self.expand_or(&[nr]));
                }
            } else {
                out.push(r.clone());
            }
        }
        out
    }

    /// Returns a witness vector if `v` matches something no row in `rows` matches.
    /// Gives up (returning `None`) once the check is too complex.
    fn useful(&mut self, rows: &[Vec<SPat>], v: &[SPat], tys: &[TypeId]) -> Option<Vec<Witness>> {
        // Each row is cloned or specialized below: O(width) work per row.
        if !self.budget.pattern_step(1 + (rows.len() * v.len()) as u64) {
            return None;
        }
        if v.is_empty() {
            return if rows.is_empty() { Some(vec![]) } else { None };
        }
        if let SPat::Or(alts) = &v[0] {
            for a in alts {
                let mut nv = vec![a.clone()];
                nv.extend(v[1..].iter().cloned());
                if let Some(w) = self.useful(rows, &nv, tys) {
                    return Some(w);
                }
            }
            return None;
        }
        let rows = self.expand_or(rows);
        let ty = tys[0];
        match &v[0] {
            SPat::Ctor(SCtor::ListAtLeast(k, m), args) => {
                // A rest pattern stands for every length >= k + m.
                let l = max_list_len(&rows).max(k + m);
                let mut cands: Vec<SCtor> = (k + m..=l).map(SCtor::ListExact).collect();
                cands.push(SCtor::ListAtLeast(l + 1, 0));
                for c in cands {
                    let n = match c {
                        SCtor::ListExact(n) | SCtor::ListAtLeast(n, 0) => n,
                        _ => unreachable!(),
                    };
                    let mut head = args[..*k].to_vec();
                    head.extend(std::iter::repeat_n(SPat::Wild, n - k - m));
                    head.extend(args[*k..].iter().cloned());
                    if let Some(w) = self.try_ctor(&rows, v, tys, &c, head) {
                        return Some(w);
                    }
                }
                None
            }
            SPat::Ctor(c, args) => self.try_ctor(&rows, v, tys, c, args.clone()),
            SPat::Wild => {
                let used: Vec<SCtor> =
                    rows.iter().filter_map(|r| if let SPat::Ctor(c, _) = &r[0] { Some(c.clone()) } else { None }).collect();
                let all = self.all_ctors(ty, &rows);
                let complete = match &all {
                    Some(all) => all.iter().all(|c| match c {
                        SCtor::ListExact(n) => {
                            used.iter().any(|u| *u == SCtor::ListExact(*n) || matches!(u, SCtor::ListAtLeast(k, m) if k + m <= *n))
                        }
                        SCtor::ListAtLeast(n, _) => used.iter().any(|u| matches!(u, SCtor::ListAtLeast(k, m) if k + m <= *n)),
                        other => used.contains(other),
                    }),
                    None => false,
                };
                if complete && !used.is_empty() {
                    for c in all.unwrap() {
                        if let Some(w) = self.try_ctor(&rows, v, tys, &c, vec![]) {
                            return Some(w);
                        }
                    }
                    None
                } else {
                    let default: Vec<Vec<SPat>> = rows.iter().filter(|r| matches!(r[0], SPat::Wild)).map(|r| r[1..].to_vec()).collect();
                    let w = self.useful(&default, &v[1..], &tys[1..])?;
                    let missing = match all {
                        Some(all) if !used.is_empty() || matches!(self.rty(ty), Ty::Bool | Ty::Named(..)) => {
                            let m = all.into_iter().find(|c| match c {
                                SCtor::ListExact(n) => {
                                    !used.iter().any(|u| *u == SCtor::ListExact(*n) || matches!(u, SCtor::ListAtLeast(k, m) if k + m <= *n))
                                }
                                SCtor::ListAtLeast(n, _) => !used.iter().any(|u| matches!(u, SCtor::ListAtLeast(k, m) if k + m <= *n)),
                                other => !used.contains(other),
                            });
                            match m {
                                Some(c) => {
                                    let n = match &c {
                                        SCtor::ListAtLeast(n, 0) => *n,
                                        _ => self.ctor_args(&c, ty).len(),
                                    };
                                    Witness::Ctor(c, vec![Witness::Wild; n])
                                }
                                None => Witness::Wild,
                            }
                        }
                        _ => Witness::Wild,
                    };
                    let mut out = vec![missing];
                    out.extend(w);
                    Some(out)
                }
            }
            SPat::Or(_) => unreachable!(),
        }
    }

    /// `useful` for the rows (or-patterns expanded) specialized by
    /// constructor `c`, whose arguments in `v` are `vhead`.
    fn try_ctor(&mut self, rows: &[Vec<SPat>], v: &[SPat], tys: &[TypeId], c: &SCtor, vhead: Vec<SPat>) -> Option<Vec<Witness>> {
        let args = self.ctor_args(c, tys[0]);
        let arity = match c {
            SCtor::ListAtLeast(n, 0) if vhead.is_empty() => *n,
            // The type does not determine the fields (a `never` or erroneous
            // type, e.g. a variant pattern on `fail(..)`): the pattern does.
            _ if !vhead.is_empty() => vhead.len(),
            _ => args.len(),
        };
        let arg_tys: Vec<TypeId> = if args.len() == arity { args } else { vec![args.first().copied().unwrap_or(TypeId::ERROR); arity] };
        let srows: Vec<Vec<SPat>> = rows.iter().filter_map(|r| self.specialize_row(r, c, arity)).collect();
        let mut nv = if vhead.is_empty() && arity > 0 { vec![SPat::Wild; arity] } else { vhead };
        nv.extend(v[1..].iter().cloned());
        let mut ntys = arg_tys;
        ntys.extend(tys[1..].iter().copied());
        let w = self.useful(&srows, &nv, &ntys)?;
        let (head, tail) = w.split_at(arity.min(w.len()));
        let mut out = vec![Witness::Ctor(c.clone(), head.to_vec())];
        out.extend(tail.iter().cloned());
        Some(out)
    }
}

fn max_list_len(rows: &[Vec<SPat>]) -> usize {
    let mut l = 0;
    for r in rows {
        match r.first() {
            Some(SPat::Ctor(SCtor::ListExact(n), _)) => l = l.max(*n),
            Some(SPat::Ctor(SCtor::ListAtLeast(k, m), _)) => l = l.max(k + m),
            _ => {}
        }
    }
    l
}
