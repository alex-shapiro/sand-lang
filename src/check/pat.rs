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
    /// Variant index within its enum, with display name.
    Variant(u32, String),
    Tuple(usize),
    Struct(usize, String),
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

impl Checker<'_> {
    pub(crate) fn check_pattern(&mut self, p: &Pattern, ty: &Type, mutable: bool) -> HPat {
        let mut or_map = None;
        self.check_pat_inner(p, ty, mutable, &mut or_map)
    }

    fn pat(&self, kind: HPatKind, ty: &Type, span: Span) -> HPat {
        HPat { kind, ty: ty.clone(), span }
    }

    fn bind_name(&mut self, id: &ast::Ident, ty: &Type, mutable: bool, or_map: &mut Option<HashMap<String, LocalId>>) -> HPat {
        if let Some(map) = or_map {
            match map.get(&id.name) {
                Some(&l) => {
                    let lt = self.fs.data[l as usize].ty.clone();
                    self.expect_type(&lt, ty, id.span);
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
        // Duplicate bindings within one pattern are an error.
        let l = self.declare_local(&id.name, ty.clone(), mutable, id.span, false);
        self.pat(HPatKind::Bind(l), ty, id.span)
    }

    /// Resolves a variant path in a pattern against the scrutinee type.
    fn pattern_variant(&mut self, path: &[ast::Ident], ty: &Type) -> Option<(TypeDefId, u32, Vec<Type>)> {
        let rt = self.resolve(ty);
        let last = path.last().unwrap();
        if path.len() == 1 {
            if let Type::Named(d, args) = &rt
                && let Some(i) = self.types[*d as usize].variants().iter().position(|v| v.name == last.name)
            {
                return Some((*d, i as u32, args.clone()));
            }
            match self.lookup_global(&last.name) {
                Some(Def::Variant(d, i)) => {
                    let n = self.types[d as usize].generics.len();
                    let args: Vec<Type> = (0..n).map(|_| self.fresh()).collect();
                    let et = Type::Named(d, args.clone());
                    if !self.unify(&et, &rt) {
                        let msg = format!("type mismatch: expected {}, found {}", self.show(&rt), self.show(&et));
                        self.error(last.span, msg);
                        return None;
                    }
                    return Some((d, i, args));
                }
                _ => {
                    let msg = if matches!(rt, Type::Named(d, _) if self.types[d as usize].is_enum()) {
                        format!("no variant `{}` on type {}", last.name, self.show(&rt))
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
        let Some(i) = self.types[d as usize].variants().iter().position(|v| v.name == last.name) else {
            let msg = format!("no variant `{}` on type {}", last.name, self.types[d as usize].name);
            self.error(last.span, msg);
            return None;
        };
        let n = self.types[d as usize].generics.len();
        let args: Vec<Type> = (0..n).map(|_| self.fresh()).collect();
        let et = Type::Named(d, args.clone());
        if !self.unify(&et, &rt) {
            let msg = format!("type mismatch: expected {}, found {}", self.show(&rt), self.show(&et));
            self.error(last.span, msg);
            return None;
        }
        Some((d, i as u32, args))
    }

    fn resolve_struct_path_quiet(&mut self, path: &[ast::Ident]) -> Option<TypeDefId> {
        if path.len() == 1 {
            if path[0].name == "Self" {
                if let Some(Type::Named(d, _)) = self.fs.self_ty.clone() {
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
                    self.lint.used_names.insert((self.cur_mod, path[0].name.clone()));
                    match self.mods[m as usize].scope.get(&path[1].name) {
                        Some((Def::Type(TyRef::Def(d)), true, _)) => Some(*d),
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn check_pat_inner(&mut self, p: &Pattern, ty: &Type, mutable: bool, or_map: &mut Option<HashMap<String, LocalId>>) -> HPat {
        let span = p.span;
        match &p.kind {
            PatternKind::Wildcard => self.pat(HPatKind::Wildcard, ty, span),
            PatternKind::Binding(id) => {
                // A payload-less variant of the scrutinee's enum?
                let rt = self.resolve(ty);
                if let Type::Named(d, _) = &rt
                    && let Some(v) = self.types[*d as usize].variants().iter().find(|v| v.name == id.name)
                    && v.fields.is_empty()
                {
                    return self.pat(HPatKind::Variant { rt_id: v.rt_id, args: vec![] }, ty, span);
                }
                if let Some(Def::Variant(d, i)) = self.mods[self.prelude.unwrap_or(0) as usize].scope.get(&id.name).map(|x| x.0.clone()) {
                    let v = self.types[d as usize].variants()[i as usize].clone();
                    if v.fields.is_empty() {
                        let n = self.types[d as usize].generics.len();
                        let args: Vec<Type> = (0..n).map(|_| self.fresh()).collect();
                        self.expect_type(&rt, &Type::Named(d, args), span);
                        return self.pat(HPatKind::Variant { rt_id: v.rt_id, args: vec![] }, ty, span);
                    }
                }
                self.bind_name(id, ty, mutable, or_map)
            }
            PatternKind::Literal(lit) => {
                let (kind, lt) = match lit {
                    PatLit::Int(i) => (HPatKind::Int(*i), Type::Int),
                    PatLit::Str(s) => (HPatKind::Str(s.clone()), Type::Str),
                    PatLit::Bool(b) => (HPatKind::Bool(*b), Type::Bool),
                    PatLit::Unit => (HPatKind::Unit, Type::Unit),
                };
                self.expect_type(ty, &lt, span);
                self.pat(kind, ty, span)
            }
            PatternKind::Range(a, b) => {
                self.expect_type(ty, &Type::Int, span);
                if a > b {
                    self.error(span, "empty range pattern");
                }
                self.pat(HPatKind::Range(*a, *b), ty, span)
            }
            PatternKind::Tuple(ps) => {
                let rt = self.resolve(ty);
                let tys = match rt {
                    Type::Tuple(ts) if ts.len() == ps.len() => ts,
                    Type::Var(_) => {
                        let ts: Vec<Type> = ps.iter().map(|_| self.fresh()).collect();
                        self.unify(ty, &Type::Tuple(ts.clone()));
                        ts
                    }
                    Type::Error => ps.iter().map(|_| Type::Error).collect(),
                    other => {
                        let msg = format!("type mismatch: expected {}, found a tuple pattern of {} elements", self.show(&other), ps.len());
                        self.error(span, msg);
                        ps.iter().map(|_| Type::Error).collect()
                    }
                };
                let hs = ps.iter().zip(&tys).map(|(p, t)| self.check_pat_inner(p, t, mutable, or_map)).collect();
                self.pat(HPatKind::Tuple(hs), ty, span)
            }
            PatternKind::Variant { path, args } => {
                let Some((d, i, targs)) = self.pattern_variant(path, ty) else {
                    if let Some(args) = args {
                        for a in args {
                            self.check_pat_inner(a, &Type::Error, mutable, or_map);
                        }
                    }
                    return self.pat(HPatKind::Wildcard, ty, span);
                };
                let v = self.types[d as usize].variants()[i as usize].clone();
                let ftys = self.variant_field_types_pub(d, i, &targs);
                let args = args.clone().unwrap_or_default();
                if args.len() != ftys.len() {
                    let msg = format!(
                        "variant `{}` has {} field{}, but the pattern has {}",
                        v.name,
                        ftys.len(),
                        if ftys.len() == 1 { "" } else { "s" },
                        args.len()
                    );
                    self.error(span, msg);
                    return self.pat(HPatKind::Wildcard, ty, span);
                }
                let hs = args.iter().zip(&ftys).map(|(p, t)| self.check_pat_inner(p, t, mutable, or_map)).collect();
                self.pat(HPatKind::Variant { rt_id: v.rt_id, args: hs }, ty, span)
            }
            PatternKind::Struct { path, fields, rest } => {
                let Some(d) = self.resolve_struct_path_quiet(path) else {
                    let name = path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
                    self.error(path[0].span, format!("unknown struct `{name}`"));
                    return self.pat(HPatKind::Wildcard, ty, span);
                };
                let def = self.types[d as usize].clone();
                if !def.is_struct() {
                    self.error(path[0].span, format!("`{}` is not a struct", def.name));
                    return self.pat(HPatKind::Wildcard, ty, span);
                }
                let n = def.generics.len();
                let targs: Vec<Type> = (0..n).map(|_| self.fresh()).collect();
                let st = Type::Named(d, targs.clone());
                self.expect_type(ty, &st, span);
                let gens = def.generics.clone();
                let mut hs = Vec::new();
                let mut seen = HashSet::new();
                for f in fields {
                    let Some(idx) = def.fields().iter().position(|x| x.name == f.name.name) else {
                        self.error(f.name.span, format!("no field `{}` on type {}", f.name.name, def.name));
                        continue;
                    };
                    seen.insert(idx);
                    let fty = def.fields()[idx].ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| targs[k].clone()));
                    let h = match &f.pattern {
                        Some(p) => self.check_pat_inner(p, &fty, mutable, or_map),
                        None => self.bind_name(&f.name, &fty, mutable, or_map),
                    };
                    hs.push((idx as u32, h));
                }
                if !rest {
                    for (i, fd) in def.fields().iter().enumerate() {
                        if !seen.contains(&i) {
                            self.error(span, format!("missing field `{}` in pattern; add `..` to ignore the rest", fd.name));
                        }
                    }
                }
                self.pat(HPatKind::Struct { fields: hs }, ty, span)
            }
            PatternKind::List { before, rest, after } => {
                let rt = self.resolve(ty);
                let elem = match rt {
                    Type::List(e) => *e,
                    Type::Var(_) => {
                        let e = self.fresh();
                        self.unify(ty, &Type::list(e.clone()));
                        e
                    }
                    Type::Error => Type::Error,
                    other => {
                        let msg = format!("type mismatch: expected {}, found a list pattern", self.show(&other));
                        self.error(span, msg);
                        Type::Error
                    }
                };
                let b = before.iter().map(|p| self.check_pat_inner(p, &elem, mutable, or_map)).collect();
                let r = match rest {
                    Some(Some(id)) => {
                        let lt = Type::list(elem.clone());
                        match self.bind_name(id, &lt, mutable, or_map).kind {
                            HPatKind::Bind(l) => Some(Some(l)),
                            _ => Some(None),
                        }
                    }
                    Some(None) => Some(None),
                    None => None,
                };
                let a = after.iter().map(|p| self.check_pat_inner(p, &elem, mutable, or_map)).collect();
                self.pat(HPatKind::List { before: b, rest: r, after: a }, ty, span)
            }
            PatternKind::Or(alts) => {
                let first = self.check_pat_inner(&alts[0], ty, mutable, or_map);
                let names: HashMap<String, LocalId> = self.pattern_bindings(&first).into_iter().collect();
                let mut hs = vec![first];
                for alt in &alts[1..] {
                    let mut sub = Some(names.clone());
                    let h = self.check_pat_inner(alt, ty, mutable, &mut sub);
                    let bound: HashSet<String> = self.pattern_bindings(&h).into_iter().map(|(n, _)| n).collect();
                    for n in names.keys() {
                        if !bound.contains(n) {
                            self.error(alt.span, format!("all alternatives of an or-pattern must bind the same names; `{n}` is missing"));
                        }
                    }
                    hs.push(h);
                }
                self.pat(HPatKind::Or(hs), ty, span)
            }
        }
    }

    pub(crate) fn variant_field_types_pub(&self, d: TypeDefId, i: u32, args: &[Type]) -> Vec<Type> {
        let def = &self.types[d as usize];
        let gens = def.generics.clone();
        def.variants()[i as usize]
            .fields
            .iter()
            .map(|t| t.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| args[k].clone())))
            .collect()
    }

    pub(crate) fn pattern_bindings(&self, p: &HPat) -> Vec<(String, LocalId)> {
        let mut out = Vec::new();
        fn go(c: &Checker, p: &HPat, out: &mut Vec<(String, LocalId)>) {
            match &p.kind {
                HPatKind::Bind(l) => {
                    let n = c.fs.locals[*l as usize].name.clone();
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
                        out.push((c.fs.locals[*l as usize].name.clone(), *l));
                    }
                    after.iter().for_each(|x| go(c, x, out));
                }
                _ => {}
            }
        }
        go(self, p, &mut out);
        out
    }

    pub(crate) fn pattern_names(&self, p: &HPat) -> Vec<String> {
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
                let name = self.types[d as usize].variants()[i as usize].name.clone();
                SPat::Ctor(SCtor::Variant(i, name), args.iter().map(|x| self.simplify(x)).collect())
            }
            HPatKind::Struct { fields } => {
                let (n, name) = match self.resolve(&p.ty) {
                    Type::Named(d, _) => (self.types[d as usize].fields().len(), self.types[d as usize].name.clone()),
                    _ => (0, String::new()),
                };
                let mut args = vec![SPat::Wild; n];
                for (i, f) in fields {
                    if (*i as usize) < n {
                        args[*i as usize] = self.simplify(f);
                    }
                }
                SPat::Ctor(SCtor::Struct(n, name), args)
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
        let row = vec![self.simplify(p)];
        let ty = self.zonk(&p.ty);
        if matches!(ty, Type::Never | Type::Error) {
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

    pub(crate) fn check_exhaustive(&mut self, ty: &Type, arms: &[HArm], span: Span) {
        let ty = self.zonk(ty);
        // No values (`never`), or an error has been reported already.
        if matches!(ty, Type::Never | Type::Error) {
            return;
        }
        let mut rows: Vec<Vec<SPat>> = Vec::new();
        self.budget.start_pattern_check();
        for arm in arms {
            if self.budget.pattern_too_complex() {
                break;
            }
            let sp = self.simplify(&arm.pat);
            if self.useful(&rows, std::slice::from_ref(&sp), std::slice::from_ref(&ty)).is_none() && !self.budget.pattern_too_complex() {
                self.error(arm.pat.span, "unreachable pattern");
            }
            if arm.guard.is_none() {
                rows.push(vec![sp]);
            }
        }
        let missing = if self.budget.pattern_too_complex() { None } else { self.useful(&rows, &[SPat::Wild], std::slice::from_ref(&ty)) };
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
                    SCtor::Variant(_, name) => {
                        if a.is_empty() {
                            name.clone()
                        } else {
                            format!("{name}({})", a.join(", "))
                        }
                    }
                    SCtor::Tuple(_) => format!("({})", a.join(", ")),
                    SCtor::Struct(_, name) => format!("{name} {{ .. }}"),
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
    fn ctor_args(&self, c: &SCtor, ty: &Type) -> Vec<Type> {
        let ty = self.resolve(ty);
        match c {
            SCtor::Tuple(n) => match ty {
                Type::Tuple(ts) => ts,
                _ => vec![Type::Error; *n],
            },
            SCtor::Variant(i, _) => match ty {
                Type::Named(d, args) => self.variant_field_types_pub(d, *i, &args),
                _ => vec![],
            },
            SCtor::Struct(n, _) => match ty {
                Type::Named(d, args) => {
                    let def = &self.types[d as usize];
                    let gens = def.generics.clone();
                    def.fields()
                        .iter()
                        .map(|f| f.ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| args[k].clone())))
                        .collect()
                }
                _ => vec![Type::Error; *n],
            },
            SCtor::ListExact(n) => {
                let e = match ty {
                    Type::List(e) => *e,
                    _ => Type::Error,
                };
                vec![e; *n]
            }
            SCtor::ListAtLeast(k, m) => {
                let e = match ty {
                    Type::List(e) => *e,
                    _ => Type::Error,
                };
                vec![e; k + m]
            }
            _ => vec![],
        }
    }

    /// All constructors of a type, if finite and known.
    fn all_ctors(&self, ty: &Type, rows: &[Vec<SPat>]) -> Option<Vec<SCtor>> {
        match self.resolve(ty) {
            Type::Bool => Some(vec![SCtor::Bool(true), SCtor::Bool(false)]),
            Type::Unit => Some(vec![SCtor::Unit]),
            Type::Tuple(ts) => Some(vec![SCtor::Tuple(ts.len())]),
            Type::Named(d, _) => {
                let def = &self.types[d as usize];
                match &def.kind {
                    TypeDefKind::Enum { variants } => {
                        Some(variants.iter().enumerate().map(|(i, v)| SCtor::Variant(i as u32, v.name.clone())).collect())
                    }
                    TypeDefKind::Struct { fields } => Some(vec![SCtor::Struct(fields.len(), def.name.clone())]),
                    TypeDefKind::Opaque => None,
                }
            }
            Type::List(_) => {
                let l = max_list_len(rows);
                let mut v: Vec<SCtor> = (0..=l).map(SCtor::ListExact).collect();
                v.push(SCtor::ListAtLeast(l + 1, 0));
                Some(v)
            }
            _ => None,
        }
    }

    /// Specializes a row by constructor `c` (with `arity` arguments).
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
    fn useful(&self, rows: &[Vec<SPat>], v: &[SPat], tys: &[Type]) -> Option<Vec<Witness>> {
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
        let ty = &tys[0];
        let try_ctor = |c: &SCtor, vhead: Vec<SPat>| -> Option<Vec<Witness>> {
            let args = self.ctor_args(c, ty);
            let arity = match c {
                SCtor::ListAtLeast(n, 0) if vhead.is_empty() => *n,
                // The type does not determine the fields (a `never` or erroneous
                // type, e.g. a variant pattern on `panic(..)`): the pattern does.
                _ if !vhead.is_empty() => vhead.len(),
                _ => args.len(),
            };
            let arg_tys: Vec<Type> = if args.len() == arity { args } else { vec![args.first().cloned().unwrap_or(Type::Error); arity] };
            let srows: Vec<Vec<SPat>> = rows.iter().filter_map(|r| self.specialize_row(r, c, arity)).collect();
            let mut nv = if vhead.is_empty() && arity > 0 { vec![SPat::Wild; arity] } else { vhead };
            nv.extend(v[1..].iter().cloned());
            let mut ntys = arg_tys;
            ntys.extend(tys[1..].iter().cloned());
            let w = self.useful(&srows, &nv, &ntys)?;
            let (head, tail) = w.split_at(arity.min(w.len()));
            let mut out = vec![Witness::Ctor(c.clone(), head.to_vec())];
            out.extend(tail.iter().cloned());
            Some(out)
        };
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
                    if let Some(w) = try_ctor(&c, head) {
                        return Some(w);
                    }
                }
                None
            }
            SPat::Ctor(c, args) => {
                let c = c.clone();
                try_ctor(&c, args.clone())
            }
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
                        if let Some(w) = try_ctor(&c, vec![]) {
                            return Some(w);
                        }
                    }
                    None
                } else {
                    let default: Vec<Vec<SPat>> = rows.iter().filter(|r| matches!(r[0], SPat::Wild)).map(|r| r[1..].to_vec()).collect();
                    let w = self.useful(&default, &v[1..], &tys[1..])?;
                    let missing = match all {
                        Some(all) if !used.is_empty() || matches!(self.resolve(ty), Type::Bool | Type::Named(..)) => {
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
