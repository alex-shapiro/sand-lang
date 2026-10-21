//! `help` and `members`, prelude functions that the checker evaluates.
//!
//! The argument names a module (imported or not), type, trait, function,
//! method, field, variant or variable, as a path in code or in a string
//! literal; any other expression stands for its type and is not evaluated.
//! A call compiles to a constant: `help` prints the text rendered here, and
//! `members` returns the names that may follow `x.`.

use super::expr::{CallArgs, Ns, mk};
use super::traits::impl_key;
use super::*;
use crate::syntax::ast::{Expr, ExprKind, StrSeg};
use std::fmt::Write;

/// What the argument of `help` or `members` refers to.
#[derive(Clone)]
enum Target {
    /// No argument.
    Nothing,
    Module(ModId),
    /// A type constructor, with its arity for tuples.
    Type(Ctor, u32),
    Param(ParamId),
    Alias(usize),
    Trait(TraitId),
    /// The functions of one name: a method may be declared for several
    /// receivers (`extend Iter<int>`, `extend Iter<float>`).
    Fns(Vec<FnId>),
    TraitMethod(TraitId, u32),
    /// `to_str` or `debug`, which every value has.
    Universal(&'static str),
    Variant(TypeDefId, u32),
    /// A field, with its type (instantiated if reached through a value).
    Field(TypeDefId, u32, TypeId),
    Global(GlobalId),
    /// A local variable or parameter: how it is declared (`let x`, `var x`,
    /// `x`), and its type.
    Local(String, TypeId),
    /// Any other expression: its source text and type.
    Value(String, TypeId),
}

/// The indentation of a section's entries, and of their docs.
const ITEM: &str = "    ";
const ITEM_DOC: &str = "        ";

const BUILTIN_TYPES: &[&str] = &["bool", "int", "float", "str", "bytes", "duration", "Range", "List<T>", "Map<K, V>", "Set<T>"];

impl Checker {
    pub(crate) fn check_intrinsic(&mut self, which: Intrinsic, args: &CallArgs, span: Span) -> HExpr {
        let name = match which {
            Intrinsic::Help => "help",
            Intrinsic::Members => "members",
        };
        let failed = mk(HKind::Error, TypeId::ERROR, span);
        if let Some((n, _)) = args.named.first() {
            self.error(n.span, format!("`{name}` takes no named arguments"));
            return failed;
        }
        if let Some(extra) = args.positional.get(1) {
            self.error(extra.span, format!("`{name}` takes at most one argument"));
            return failed;
        }
        let target = match args.positional.first() {
            Some(e) => match self.target(e) {
                Some(t) => t,
                None => return failed,
            },
            None => Target::Nothing,
        };
        if let Target::Fns(fs) = &target {
            self.fs.doc_refs.extend(fs);
        }
        match which {
            Intrinsic::Help => {
                let text = self.render(&target);
                self.print_call(text, span)
            }
            Intrinsic::Members => {
                let items = self.members_of(&target).into_iter().map(|n| mk(HKind::Str(n), TypeId::STR, span)).collect();
                let t = self.list_ty(TypeId::STR);
                mk(HKind::List(items), t, span)
            }
        }
    }

    /// A call of the prelude's `print` with `text`.
    fn print_call(&mut self, text: String, span: Span) -> HExpr {
        let print = self.prelude.and_then(|p| match self.mods[p as usize].scope.get(&self.syms.get("print")?)?.def {
            Def::Fn(f) => Some(f),
            _ => None,
        });
        let Some(FnKind::Native(native)) = print.map(|f| self.fns[f as usize].kind.clone()) else {
            panic!("the prelude is missing `print`")
        };
        let args = vec![HArg::Expr(mk(HKind::Str(text), TypeId::STR, span))];
        mk(HKind::CallNative { native, args, reflect: vec![] }, TypeId::UNIT, span)
    }

    // ------------------------------------------------------------------
    // Resolution

    fn target(&mut self, e: &Expr) -> Option<Target> {
        match &e.kind {
            ExprKind::Paren(inner) => self.target(inner),
            ExprKind::Str(segs) if segs.iter().all(|s| matches!(s, StrSeg::Text(_))) => {
                let text: String = segs.iter().map(|s| if let StrSeg::Text(t) = s { t.as_str() } else { "" }).collect();
                self.named_target(&text, e.span)
            }
            ExprKind::Ident(id) if self.find_local(&id.name).is_none() => self.root_target(&id.name, id.span),
            ExprKind::Ident(id) => {
                let decl = match self.find_local(&id.name) {
                    Some(l) if !self.fs.data[l as usize].is_param => {
                        let kw = if self.fs.locals[l as usize].mutable { "var" } else { "let" };
                        format!("{kw} {}", id.name)
                    }
                    _ => id.name.clone(),
                };
                let h = self.infer(e, None);
                Some(Target::Local(decl, h.ty))
            }
            ExprKind::SelfValue => {
                let h = self.infer(e, None);
                Some(Target::Local("self".into(), h.ty))
            }
            ExprKind::SelfType => match self.fs.self_ty {
                Some(t) => Some(self.type_target(t)),
                None => {
                    self.error(e.span, "`Self` is only valid inside a type's methods");
                    None
                }
            },
            ExprKind::Field(base, name) => {
                let t = self.target(base)?;
                self.member(t, &name.name, name.span)
            }
            ExprKind::List(items) if items.len() == 1 && matches!(self.resolve_ns(&items[0]), Some(Ns::Type(_))) => {
                Some(Target::Type(Ctor::List, 0))
            }
            ExprKind::Map(entries) if entries.len() == 1 && matches!(self.resolve_ns(&entries[0].0), Some(Ns::Type(_))) => {
                Some(Target::Type(Ctor::Map, 0))
            }
            _ => {
                let h = self.infer(e, None);
                Some(Target::Value(self.source_text(e.span), h.ty))
            }
        }
    }

    /// What a name refers to at the start of a path.
    fn root_target(&mut self, name: &str, span: Span) -> Option<Target> {
        if let Some(def) = self.lookup_global(name) {
            return Some(self.def_target(def));
        }
        let sym = self.syms.get(name);
        if let Some(&(_, p)) = self.fs.generics.iter().rev().find(|(n, _)| Some(*n) == sym) {
            return Some(Target::Param(p));
        }
        if let Some(m) = self.module_named(name) {
            return Some(Target::Module(m));
        }
        let msg = if self.module_index.iter().any(|(m, _)| m == name) {
            format!("module `{name}` is not loaded: import it, or name it directly as in `help({name})`")
        } else if crate::stdlib::OS_MODULES.contains(&name) {
            format!("module `{name}` is not available: this build of Sand has no operating-system access")
        } else {
            let mut names: Vec<String> = self.module_index.iter().map(|(m, _)| m.clone()).collect();
            names.extend(self.scope_names(self.cur_mod, false));
            if let Some(p) = self.prelude {
                names.extend(self.scope_names(p, true));
            }
            names.extend(BUILTIN_TYPES.iter().map(|t| t.to_string()));
            format!("undefined name `{name}`{}", suggestion(name, names))
        };
        self.error(span, msg);
        None
    }

    fn def_target(&mut self, def: Def) -> Target {
        match def {
            Def::Fn(f) => Target::Fns(vec![f]),
            Def::Type(TyRef::Prim(t)) => self.type_target(t),
            Def::Type(TyRef::Coll(c)) => Target::Type(c, 0),
            Def::Type(TyRef::Def(d)) => Target::Type(Ctor::Def(d), 0),
            Def::Type(TyRef::Alias(i)) => Target::Alias(i),
            Def::Global(g) => {
                self.mark_global_used(g);
                Target::Global(g)
            }
            Def::Module(m) => Target::Module(m),
            Def::Variant(d, i) => Target::Variant(d, i),
            Def::Trait(t) => Target::Trait(t),
        }
    }

    fn type_target(&mut self, t: TypeId) -> Target {
        let t = self.resolve(t);
        match self.ty(t) {
            Ty::Param(p) => Target::Param(p),
            ty => match impl_key(&self.arena, t) {
                Some((c, n)) => Target::Type(c, n),
                None if matches!(ty, Ty::Fn(..)) => Target::Type(Ctor::Fn, 0),
                None => Target::Value(self.show(t), t),
            },
        }
    }

    /// The standard, host or prelude module named `name` (whether or not
    /// the current module imports it).
    pub(crate) fn module_named(&self, name: &str) -> Option<ModId> {
        self.mods.iter().position(|m| !matches!(m.kind, ModKind::Script { .. }) && self.name(m.name) == name).map(|i| i as ModId)
    }

    /// Resolves a string target path such as `json.Json.get` or `[T].map`.
    fn named_target(&mut self, text: &str, span: Span) -> Option<Target> {
        let text = text.trim();
        let bad = |c: &mut Self| {
            let found = crate::stdlib::truncate(text, 40);
            let msg = format!("`help` and `members` take a name, such as \"json\" or \"json.Json\"; found {found:?}");
            c.error(span, msg);
            None
        };
        let (mut target, rest) = if text.starts_with('[') {
            let Some(close) = closing_bracket(text) else { return bad(self) };
            let ctor = if top_level_colon(&text[1..close]) { Ctor::Map } else { Ctor::List };
            (Target::Type(ctor, 0), &text[close + 1..])
        } else {
            let end = segment_end(text);
            let first = strip_type_args(&text[..end]);
            match first.strip_suffix('?') {
                Some(inner) if is_ident(inner) => (Target::Type(Ctor::Def(self.wk.option), 0), &text[end..]),
                _ if is_ident(first) => (self.root_target(first, span)?, &text[end..]),
                _ => return bad(self),
            }
        };
        let mut rest = rest;
        while !rest.is_empty() {
            let Some(r) = rest.strip_prefix('.') else { return bad(self) };
            let end = segment_end(r);
            let seg = strip_type_args(&r[..end]);
            if !is_ident(seg) {
                return bad(self);
            }
            target = self.member(target, seg, span)?;
            rest = &r[end..];
        }
        Some(target)
    }

    /// What `name` refers to as a member of `t` (`t.name`).
    fn member(&mut self, t: Target, name: &str, span: Span) -> Option<Target> {
        match t {
            Target::Module(m) => {
                let found = self.syms.get(name).and_then(|s| self.mods[m as usize].scope.get(&s).copied());
                match found {
                    Some(b) if b.is_pub || m == self.cur_mod => Some(self.def_target(b.def)),
                    Some(_) => {
                        self.error(span, format!("`{name}` is private"));
                        None
                    }
                    None => {
                        let mn = self.name(self.mods[m as usize].name).to_string();
                        let names = self.module_items(m).into_iter().map(|(n, _)| self.name(n).to_string()).collect();
                        self.error(span, format!("module `{mn}` has no public item `{name}`{}", suggestion(name, names)));
                        None
                    }
                }
            }
            Target::Type(c, n) => self.type_member(c, n, None, name, span),
            Target::Local(_, ty) | Target::Value(_, ty) | Target::Field(_, _, ty) => self.ty_member(ty, name, span),
            Target::Global(g) => self.ty_member(self.globals[g as usize].ty, name, span),
            Target::Alias(i) => {
                let t = self.resolve_alias(i, span);
                self.ty_member(t, name, span)
            }
            Target::Param(p) => self.param_member(p, name, span),
            Target::Trait(t) => {
                let slot = self.trait_method_slot(t, name, span)?;
                Some(Target::TraitMethod(t, slot))
            }
            Target::Variant(d, i) if self.types[d as usize].variants()[i as usize].fields.is_empty() => {
                self.type_member(Ctor::Def(d), 0, None, name, span)
            }
            Target::Nothing | Target::Fns(_) | Target::TraitMethod(..) | Target::Universal(_) | Target::Variant(..) => {
                self.error(span, format!("no member `{name}`: functions have no members besides `to_str` and `debug`"));
                None
            }
        }
    }

    fn ty_member(&mut self, ty: TypeId, name: &str, span: Span) -> Option<Target> {
        let ty = self.resolve(ty);
        match self.type_target(ty) {
            Target::Type(c, n) => self.type_member(c, n, Some(ty), name, span),
            Target::Param(p) => self.param_member(p, name, span),
            _ => {
                if ty != TypeId::ERROR {
                    let msg = format!("no member `{name}` on type {}", self.show(ty));
                    self.error(span, msg);
                }
                None
            }
        }
    }

    /// A member of the type `c` (`inst`, if known: the type of a value).
    fn type_member(&mut self, c: Ctor, arity: u32, inst: Option<TypeId>, name: &str, span: Span) -> Option<Target> {
        if let Ctor::Def(d) = c {
            let sym = self.syms.get(name);
            let def = &self.types[d as usize];
            if let Some(i) = def.variants().iter().position(|v| Some(v.name) == sym) {
                return Some(Target::Variant(d, i as u32));
            }
            if let Some(i) = def.fields().iter().position(|f| Some(f.name) == sym) {
                let ty = match inst.map(|t| self.ty(t)) {
                    Some(Ty::Named(_, args)) => {
                        let args = self.elems(args);
                        self.field_type(d, i, &args)
                    }
                    _ => def.fields()[i].ty,
                };
                return Some(Target::Field(d, i as u32, ty));
            }
        }
        if let Some(fs) = self.methods_named(c, name) {
            let fs: Vec<FnId> = self.methods_of(c).into_iter().filter(|f| fs.contains(f)).collect();
            if !fs.is_empty() {
                return Some(Target::Fns(fs));
            }
        }
        let sym = self.syms.get(name);
        let impl_fns: Vec<FnId> = self
            .visible_impls(c, arity)
            .into_iter()
            .filter_map(|i| {
                let imp = &self.impls[i as usize];
                let slot = self.traits[imp.trait_ref.trait_id as usize].slot(sym?)?;
                Some(imp.slots[slot as usize])
            })
            .collect();
        if !impl_fns.is_empty() {
            return Some(Target::Fns(impl_fns));
        }
        if let Some(u) = universal(name) {
            return Some(Target::Universal(u));
        }
        let names = self.type_members(c, arity);
        let shown = match inst {
            Some(t) => self.show(t),
            None => self.ctor_name(c, arity),
        };
        self.error(span, format!("no member `{name}` on type {shown}{}", suggestion(name, names)));
        None
    }

    fn param_member(&mut self, p: ParamId, name: &str, span: Span) -> Option<Target> {
        let sym = self.syms.get(name);
        for tr in self.bound_traits(p) {
            if let Some(slot) = sym.and_then(|s| self.traits[tr.trait_id as usize].slot(s)) {
                return Some(Target::TraitMethod(tr.trait_id, slot));
            }
        }
        if let Some(u) = universal(name) {
            return Some(Target::Universal(u));
        }
        let names = self.param_members(p);
        let pn = self.name(self.params[p as usize].name).to_string();
        self.error(span, format!("no member `{name}` on type {pn}{}", suggestion(name, names)));
        None
    }

    /// The impls for constructor `c` of traits visible in the current module.
    fn visible_impls(&self, c: Ctor, arity: u32) -> Vec<ImplId> {
        let ids = self.impls_by_ctor.get(&(c, arity)).cloned().unwrap_or_default();
        ids.iter()
            .copied()
            .filter(|&i| {
                let t = &self.traits[self.impls[i as usize].trait_ref.trait_id as usize];
                t.is_pub || t.module == self.cur_mod
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Members

    fn members_of(&mut self, t: &Target) -> Vec<String> {
        let mut names = match t {
            Target::Nothing => {
                let mut names: Vec<String> = self.fs.scopes.names().map(|s| self.name(s).to_string()).collect();
                names.extend(self.scope_names(self.cur_mod, false));
                names
            }
            Target::Module(m) => self.module_items(*m).into_iter().map(|(n, _)| self.name(n).to_string()).collect(),
            Target::Type(c, n) => self.type_members(*c, *n),
            Target::Param(p) => self.param_members(*p),
            Target::Alias(i) => {
                let t = self.resolve_alias(*i, Span::default());
                self.ty_members(t)
            }
            Target::Trait(t) => self.traits[*t as usize].methods.iter().map(|m| self.name(m.name).to_string()).collect(),
            Target::Variant(d, i) if self.types[*d as usize].variants()[*i as usize].fields.is_empty() => {
                self.type_members(Ctor::Def(*d), 0)
            }
            Target::Fns(_) | Target::TraitMethod(..) | Target::Universal(_) | Target::Variant(..) => {
                vec!["debug".to_string(), "to_str".to_string()]
            }
            Target::Field(_, _, ty) | Target::Local(_, ty) | Target::Value(_, ty) => self.ty_members(*ty),
            Target::Global(g) => self.ty_members(self.globals[*g as usize].ty),
        };
        self.budget.charge(names.len() as u64);
        names.retain(|n| !n.starts_with('<'));
        names.sort();
        names.dedup();
        names
    }

    fn ty_members(&mut self, ty: TypeId) -> Vec<String> {
        match self.type_target(ty) {
            Target::Type(c, n) => self.type_members(c, n),
            Target::Param(p) => self.param_members(p),
            _ => universal_names(),
        }
    }

    /// The variants, fields and methods of type `c`, in no particular order.
    fn type_members(&self, c: Ctor, arity: u32) -> Vec<String> {
        let mut names = universal_names();
        if let Ctor::Def(d) = c {
            let def = &self.types[d as usize];
            names.extend(def.variants().iter().map(|v| self.name(v.name).to_string()));
            names.extend(def.fields().iter().map(|f| self.name(f.name).to_string()));
        }
        names.extend(self.methods_of(c).into_iter().map(|f| self.name(self.fns[f as usize].name).to_string()));
        for i in self.visible_impls(c, arity) {
            let t = &self.traits[self.impls[i as usize].trait_ref.trait_id as usize];
            names.extend(t.methods.iter().map(|m| self.name(m.name).to_string()));
        }
        names
    }

    fn param_members(&mut self, p: ParamId) -> Vec<String> {
        let mut names = universal_names();
        for tr in self.bound_traits(p) {
            names.extend(self.traits[tr.trait_id as usize].methods.iter().map(|m| self.name(m.name).to_string()));
        }
        names
    }

    /// Returns inherent methods of `c` in declaration order, excluding
    /// private helpers in non-script modules.
    pub(super) fn methods_of(&self, c: Ctor) -> Vec<FnId> {
        let shown = |f: FnId| {
            let fi = &self.fns[f as usize];
            fi.is_pub || fi.module == self.cur_mod || matches!(self.mods[fi.module as usize].kind, ModKind::Script { .. })
        };
        let mut fs: Vec<FnId> = self
            .methods
            .entries()
            .into_iter()
            .filter(|((k, _), _)| *k == c)
            .flat_map(|(_, fs)| fs.iter().copied())
            .filter(|&f| shown(f))
            .collect();
        self.budget.charge(fs.len() as u64);
        fs.sort_by_key(|&f| {
            let s = self.fns[f as usize].span;
            (s.file, s.lo)
        });
        fs
    }

    /// The items a module exports (all of them, for the current module), in
    /// order of declaration: those it declares, not those it imports.
    fn module_items(&self, m: ModId) -> Vec<(Symbol, Def)> {
        let info = &self.mods[m as usize];
        let mut items: Vec<(Symbol, Binding)> = info
            .scope
            .entries()
            .into_iter()
            .filter(|(_, b)| (b.is_pub || m == self.cur_mod) && b.span.file == info.file)
            .filter(|(_, b)| !matches!(b.def, Def::Module(_) | Def::Variant(..)))
            .map(|(&n, &b)| (n, b))
            .collect();
        self.budget.charge(items.len() as u64);
        items.sort_by_key(|(_, b)| b.span.lo);
        items.into_iter().map(|(n, b)| (n, b.def)).collect()
    }

    /// Returns a diagnostic hint if `name` matches an unimported module or standard-library function.
    pub(crate) fn import_hint(&self, name: &str) -> String {
        if self.module_index.iter().any(|(m, _)| m == name) {
            return format!(": add `import {name}`");
        }
        match self.module_exports.get(name) {
            Some(module) => format!("; did you mean `{module}.{name}`? (add `import {module}`)"),
            None => String::new(),
        }
    }

    fn scope_names(&self, m: ModId, public: bool) -> Vec<String> {
        let scope = &self.mods[m as usize].scope;
        scope.entries().into_iter().filter(|(_, b)| b.is_pub || !public).map(|(&n, _)| self.name(n).to_string()).collect()
    }

    // ------------------------------------------------------------------
    // Rendering

    fn render(&mut self, t: &Target) -> String {
        let mut out = String::new();
        match t {
            Target::Nothing => self.render_overview(&mut out),
            &Target::Module(m) => self.render_module(m, &mut out),
            &Target::Type(c, n) => self.render_type(c, n, &mut out),
            &Target::Param(p) => self.render_param(p, &mut out),
            &Target::Alias(i) => {
                let ty = self.resolve_alias(i, Span::default());
                let _ = writeln!(out, "type {} = {}\n", self.name(self.aliases[i].name), self.show(ty));
                self.render_type_of(ty, &mut out);
            }
            &Target::Trait(t) => self.render_trait(t, &mut out),
            Target::Fns(fs) => {
                for &f in fs {
                    let _ = writeln!(out, "{}\n", self.signature(f, true));
                    if let Some(doc) = self.fn_doc(f) {
                        let _ = writeln!(out, "{doc}\n");
                    }
                }
            }
            &Target::TraitMethod(t, slot) => {
                let f = self.traits[t as usize].methods[slot as usize].sig;
                let _ = writeln!(out, "{}\n", self.signature(f, true));
                if let Some(doc) = &self.fns[f as usize].doc {
                    let _ = writeln!(out, "{doc}");
                }
            }
            Target::Universal(name) => {
                let what = if *name == "to_str" {
                    "The display form of the value, as `print` and string interpolation show it."
                } else {
                    "The debug form of the value: strings quoted and escaped, as inside lists and other values."
                };
                let _ = writeln!(out, "fn {name}(self) -> str\n\n{what} Every value has this method.");
            }
            &Target::Variant(d, i) => {
                let def = &self.types[d as usize];
                let v = &def.variants()[i as usize];
                let _ = writeln!(out, "variant {}.{}\n", self.type_name(d), self.variant_text(d, i));
                if let Some(doc) = &v.doc {
                    let _ = writeln!(out, "{doc}");
                }
            }
            &Target::Field(d, i, ty) => {
                let f = &self.types[d as usize].fields()[i as usize];
                let var = if f.mutable { "var " } else { "" };
                let _ = writeln!(out, "field {}.{var}{}: {}\n", self.type_name(d), self.name(f.name), self.show(ty));
                if let Some(doc) = &f.doc {
                    let _ = writeln!(out, "{doc}");
                }
            }
            &Target::Global(g) => {
                let gi = &self.globals[g as usize];
                let kw = if gi.mutable { "var" } else { "let" };
                let _ = writeln!(out, "{kw} {}{}: {}\n", self.module_prefix(gi.module), self.name(gi.name), self.show(gi.ty));
                if let Some(doc) = &gi.doc {
                    let _ = writeln!(out, "{doc}");
                }
            }
            Target::Local(decl, ty) => {
                let _ = writeln!(out, "{decl}: {}", self.show(*ty));
            }
            Target::Value(text, ty) => {
                let _ = writeln!(out, "{text}: {}\n", self.show(*ty));
                self.render_type_of(*ty, &mut out);
            }
        }
        self.budget.charge(out.len() as u64 / 16);
        out.truncate(out.trim_end().len());
        out
    }

    fn render_overview(&mut self, out: &mut String) {
        out.push_str(
            "help(x) shows the documentation of x: a module, type, trait, function,\n\
             method, field, variant or variable, or the type of any other expression\n\
             (which is not evaluated). A string identifies an item too: help(\"json.Json\").\n\
             members(x) lists the names that may follow `x.`, and members() those\n\
             in scope.\n\n",
        );
        let Some(p) = self.prelude else { return };
        let items = self.module_items(p);
        let fns: Vec<(String, String)> = items
            .iter()
            .filter_map(|&(n, d)| match d {
                Def::Fn(f) => Some((self.name(n).to_string(), self.fns[f as usize].doc.as_deref().map(summary).unwrap_or_default())),
                _ => None,
            })
            .collect();
        out.push_str("Functions:\n");
        table(out, &fns);
        let mut types: Vec<String> = BUILTIN_TYPES.iter().map(|t| t.to_string()).collect();
        let mut traits = Vec::new();
        for &(_, d) in &items {
            match d {
                Def::Type(TyRef::Def(d)) => types.push(self.type_header_name(d)),
                Def::Trait(t) => traits.push(self.trait_header_name(t)),
                _ => {}
            }
        }
        out.push_str("Types:\n");
        wrap(out, &types);
        out.push_str("Traits:\n");
        wrap(out, &traits);
        out.push_str("Modules (help(name) describes one):\n");
        let mods: Vec<(String, String)> = self.module_index.iter().cloned().collect();
        table(out, &mods);
    }

    fn render_module(&mut self, m: ModId, out: &mut String) {
        let info = &self.mods[m as usize];
        let _ = writeln!(out, "module {}\n", self.name(info.name));
        if let Some(doc) = &info.doc {
            let _ = writeln!(out, "{doc}\n");
        }
        let (mut types, mut traits, mut fns, mut values) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (name, def) in self.module_items(m) {
            match def {
                Def::Type(TyRef::Def(d)) => types.push((self.type_header(d, false), self.types[d as usize].doc.clone())),
                Def::Type(TyRef::Alias(i)) => {
                    let ty = self.resolve_alias(i, Span::default());
                    types.push((format!("type {} = {}", self.name(name), self.show(ty)), None));
                }
                Def::Trait(t) => traits.push((format!("trait {}", self.trait_header_name(t)), self.traits[t as usize].doc.clone())),
                Def::Fn(f) => fns.push((self.signature(f, false), self.fns[f as usize].doc.clone())),
                Def::Global(g) => {
                    let gi = &self.globals[g as usize];
                    let kw = if gi.mutable { "var" } else { "let" };
                    values.push((format!("{kw} {}: {}", self.name(name), self.show(gi.ty)), gi.doc.clone()));
                }
                _ => {}
            }
        }
        for (title, entries) in [("Types", types), ("Traits", traits), ("Functions", fns), ("Values", values)] {
            section(out, title, &entries);
        }
    }

    fn render_type(&mut self, c: Ctor, arity: u32, out: &mut String) {
        let doc = match c {
            Ctor::Def(d) => {
                let _ = writeln!(out, "{}\n", self.type_header(d, true));
                self.types[d as usize].doc.clone().or_else(|| self.ctor_docs.get(&c).cloned())
            }
            _ => {
                let _ = writeln!(out, "type {}\n", self.ctor_name(c, arity));
                self.ctor_docs.get(&c).cloned()
            }
        };
        if let Some(doc) = doc {
            let _ = writeln!(out, "{doc}\n");
        }
        if let Ctor::Def(d) = c {
            let def = &self.types[d as usize];
            let variants: Vec<(String, Option<Arc<str>>)> =
                (0..def.variants().len()).map(|i| (self.variant_text(d, i as u32), def.variants()[i].doc.clone())).collect();
            section(out, "Variants", &variants);
            let fields: Vec<(String, Option<Arc<str>>)> = def
                .fields()
                .iter()
                .map(|f| {
                    let var = if f.mutable { "var " } else { "" };
                    let default = f.default.map(|h| format!(" = {}", self.source_text(self.fns[h as usize].span))).unwrap_or_default();
                    (format!("{var}{}: {}{default}", self.name(f.name), self.show(f.ty)), f.doc.clone())
                })
                .collect();
            section(out, "Fields", &fields);
        }
        let methods: Vec<(String, Option<Arc<str>>)> =
            self.methods_of(c).into_iter().map(|f| (self.signature(f, false), self.fns[f as usize].doc.clone())).collect();
        section(out, "Methods", &methods);
        let traits: Vec<String> =
            self.visible_impls(c, arity).into_iter().map(|i| self.show_trait(&self.impls[i as usize].trait_ref)).collect();
        if !traits.is_empty() {
            let _ = writeln!(out, "Implements: {}", traits.join(", "));
        }
    }

    /// The documentation of the type of a value.
    fn render_type_of(&mut self, ty: TypeId, out: &mut String) {
        match self.type_target(ty) {
            Target::Type(c, n) => self.render_type(c, n, out),
            Target::Param(p) => self.render_param(p, out),
            _ => {}
        }
    }

    fn render_param(&mut self, p: ParamId, out: &mut String) {
        let _ = writeln!(out, "type parameter {}\n", self.param_text(p));
        let mut methods = Vec::new();
        for tr in self.bound_traits(p) {
            for m in &self.traits[tr.trait_id as usize].methods {
                methods.push((self.signature(m.sig, true), self.fns[m.sig as usize].doc.clone()));
            }
        }
        section(out, "Methods", &methods);
    }

    fn render_trait(&mut self, t: TraitId, out: &mut String) {
        let td = &self.traits[t as usize];
        let _ = writeln!(out, "trait {}{}\n", self.module_prefix(td.module), self.trait_header_name(t));
        if let Some(doc) = &td.doc {
            let _ = writeln!(out, "{doc}\n");
        }
        let mut supers: Vec<String> = td.supers.iter().map(|s| self.show_trait(s)).collect();
        supers.extend(td.builtin_supers.iter().map(|b| b.name().to_string()));
        if !supers.is_empty() {
            let _ = writeln!(out, "Supertraits: {}\n", supers.join(" + "));
        }
        let methods: Vec<(String, Option<Arc<str>>)> =
            td.methods.iter().map(|m| (self.signature(m.sig, false), self.fns[m.sig as usize].doc.clone())).collect();
        section(out, "Methods", &methods);
        let mut impls: Vec<String> = Vec::new();
        for imp in self.impls.iter().filter(|i| i.trait_ref.trait_id == t) {
            let shown = self.show(imp.target);
            if !impls.contains(&shown) {
                impls.push(shown);
            }
        }
        self.budget.charge(self.impls.len() as u64);
        if !impls.is_empty() {
            out.push_str("Implemented for:\n");
            wrap(out, &impls);
        }
    }

    /// Formats the signature of `f`, prefixed with its owner or module if
    /// `qualify` is true.
    pub(super) fn signature(&self, f: FnId, qualify: bool) -> String {
        let fi = &self.fns[f as usize];
        let mut s = String::from("fn ");
        if qualify {
            s.push_str(&self.qualifier(f));
        }
        s.push_str(self.name(fi.name));
        let own = &fi.generics[fi.outer.min(fi.generics.len())..];
        if !own.is_empty() {
            let ps: Vec<String> = own.iter().map(|&p| self.param_text(p)).collect();
            let _ = write!(s, "<{}>", ps.join(", "));
        }
        s.push('(');
        for (i, p) in fi.params.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            let name = self.name(p.name);
            if i == 0 && fi.has_self {
                s.push_str("self");
                if let Some(t) = self.specialized_self(f) {
                    let _ = write!(s, ": {}", self.show(t));
                }
                continue;
            }
            let _ = write!(s, "{name}: {}", self.show(p.ty));
            let default = match &p.default {
                Some(DefaultArg::Inline(e)) => Some(e.span),
                Some(DefaultArg::Helper(h)) => Some(self.fns[*h as usize].span),
                None => None,
            };
            if let Some(span) = default {
                let _ = write!(s, " = {}", self.source_text(span));
            }
        }
        s.push(')');
        if self.resolve(fi.ret) != TypeId::UNIT {
            let _ = write!(s, " -> {}", self.show(fi.ret));
        }
        // Bounds a method puts on its type's parameters (`fn sum(self) -> T
        // where T: Num`), except `Hash`, which every map key and set element has.
        if fi.trait_of.is_none() {
            let keyed = matches!(fi.owner, Some(Ctor::Map | Ctor::Set));
            let wheres: Vec<String> = fi.generics[..fi.outer.min(fi.generics.len())]
                .iter()
                .filter_map(|&p| {
                    let bounds = self.bounds_text(p, keyed);
                    (!bounds.is_empty()).then(|| format!("{}: {bounds}", self.name(self.params[p as usize].name)))
                })
                .collect();
            if !wheres.is_empty() {
                let _ = write!(s, " where {}", wheres.join(", "));
            }
        }
        s
    }

    /// The receiver type of a method declared for some instances of its
    /// type only (`extend [str]`, `impl Describe for [int]`).
    fn specialized_self(&self, f: FnId) -> Option<TypeId> {
        let st = self.fns[f as usize].self_ty?;
        let args = match self.ty(st) {
            Ty::List(a) | Ty::Set(a) => vec![a],
            Ty::Map(k, v) => vec![k, v],
            Ty::Named(_, l) | Ty::Tuple(l) => self.arena.list(l).to_vec(),
            _ => vec![],
        };
        let mut seen = Vec::new();
        let generic = args.iter().all(|&a| matches!(self.ty(a), Ty::Param(p) if !seen.contains(&p) && { seen.push(p); true }));
        (!generic).then_some(st)
    }

    /// How a function is qualified: by its trait, type or module.
    fn qualifier(&self, f: FnId) -> String {
        let fi = &self.fns[f as usize];
        if let Some(t) = fi.trait_of.filter(|_| fi.impl_of.is_none()) {
            let td = &self.traits[t as usize];
            return format!("{}{}.", self.module_prefix(td.module), self.name(td.name));
        }
        match fi.owner {
            Some(Ctor::Def(d)) => format!("{}.", self.type_name(d)),
            Some(c) => {
                let name = self.ctor_name(c, 0);
                format!("{}.", name.split('<').next().unwrap_or(&name))
            }
            None => self.module_prefix(fi.module),
        }
    }

    /// `json.` for an item of module `json`; nothing for the prelude and
    /// the current module.
    fn module_prefix(&self, m: ModId) -> String {
        if Some(m) == self.prelude || m == self.cur_mod { String::new() } else { format!("{}.", self.name(self.mods[m as usize].name)) }
    }

    /// Returns the doc comment of `f`, falling back to the trait method doc
    /// comment for trait implementations.
    pub(super) fn fn_doc(&self, f: FnId) -> Option<Arc<str>> {
        let fi = &self.fns[f as usize];
        if fi.doc.is_some() {
            return fi.doc.clone();
        }
        let imp = &self.impls[fi.impl_of? as usize];
        let slot = imp.slots.iter().position(|&s| s == f)?;
        let sig = self.traits[imp.trait_ref.trait_id as usize].methods[slot].sig;
        self.fns[sig as usize].doc.clone()
    }

    /// The name of a type definition, qualified unless it is in scope.
    fn type_name(&self, d: TypeDefId) -> String {
        format!("{}{}", self.module_prefix(self.types[d as usize].module), self.name(self.types[d as usize].name))
    }

    pub(super) fn type_header_name(&self, d: TypeDefId) -> String {
        let def = &self.types[d as usize];
        let mut s = self.name(def.name).to_string();
        if !def.generics.is_empty() {
            let ps: Vec<&str> = def.generics.iter().map(|&p| self.name(self.params[p as usize].name)).collect();
            let _ = write!(s, "<{}>", ps.join(", "));
        }
        s
    }

    pub(super) fn type_header(&self, d: TypeDefId, qualify: bool) -> String {
        let def = &self.types[d as usize];
        let (kw, bounds) = match def.kind {
            TypeDefKind::Struct { .. } => ("struct", ""),
            TypeDefKind::Enum { .. } => ("enum", ""),
            TypeDefKind::Opaque(Some(v)) => ("type", if v.ord { ": Ord + Hash" } else { ": Hash" }),
            TypeDefKind::Opaque(None) => ("type", ""),
        };
        let prefix = if qualify { self.module_prefix(def.module) } else { String::new() };
        format!("{kw} {prefix}{}{bounds}", self.type_header_name(d))
    }

    pub(super) fn trait_header_name(&self, t: TraitId) -> String {
        let td = &self.traits[t as usize];
        let mut s = self.name(td.name).to_string();
        if !td.generics.is_empty() {
            let ps: Vec<&str> = td.generics.iter().map(|&p| self.name(self.params[p as usize].name)).collect();
            let _ = write!(s, "<{}>", ps.join(", "));
        }
        s
    }

    pub(super) fn ctor_name(&self, c: Ctor, arity: u32) -> String {
        match c {
            Ctor::Int => "int".into(),
            Ctor::Float => "float".into(),
            Ctor::Bool => "bool".into(),
            Ctor::Str => "str".into(),
            Ctor::Bytes => "bytes".into(),
            Ctor::Duration => "duration".into(),
            Ctor::Unit => "()".into(),
            Ctor::Range => "Range".into(),
            Ctor::List => "List<T>".into(),
            Ctor::Map => "Map<K, V>".into(),
            Ctor::Set => "Set<T>".into(),
            Ctor::Tuple => {
                let names: Vec<String> = (0..arity).map(|i| ((b'A' + (i % 26) as u8) as char).to_string()).collect();
                format!("({})", names.join(", "))
            }
            Ctor::Fn => "fn(...)".into(),
            Ctor::Def(d) => format!("{}{}", self.module_prefix(self.types[d as usize].module), self.type_header_name(d)),
        }
    }

    pub(super) fn variant_text(&self, d: TypeDefId, i: u32) -> String {
        let v = &self.types[d as usize].variants()[i as usize];
        let mut s = self.name(v.name).to_string();
        if !v.fields.is_empty() {
            let fields: Vec<String> = v
                .fields
                .iter()
                .enumerate()
                .map(|(k, &t)| match &v.field_names {
                    Some(ns) => format!("{}: {}", self.name(ns[k]), self.show(t)),
                    None => self.show(t),
                })
                .collect();
            let _ = write!(s, "({})", fields.join(", "));
        }
        s
    }

    /// `T: Ord + Describe`
    fn param_text(&self, p: ParamId) -> String {
        let name = self.name(self.params[p as usize].name);
        let bounds = self.bounds_text(p, false);
        if bounds.is_empty() { name.to_string() } else { format!("{name}: {bounds}") }
    }

    /// `Ord + Describe` (without `Hash` if `keyed`).
    fn bounds_text(&self, p: ParamId, keyed: bool) -> String {
        let info = &self.params[p as usize];
        // (`Add` cannot be written: `+` is allowed on any type.)
        let mut bounds: Vec<String> =
            info.bounds.iter().filter(|&&b| b != Bound::Add && !(keyed && b == Bound::Hash)).map(|b| b.name().to_string()).collect();
        bounds.dedup();
        let visible = |tr: &&TraitRef| {
            let t = &self.traits[tr.trait_id as usize];
            t.is_pub || t.module == self.cur_mod
        };
        bounds.extend(info.traits.iter().filter(visible).map(|tr| self.show_trait(tr)));
        bounds.join(" + ")
    }

    pub(super) fn source_text(&self, span: Span) -> String {
        let file = self.sources.get(&span.file);
        let text = file.and_then(|f| f.text.get(span.lo as usize..span.hi as usize)).unwrap_or("...");
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let short = crate::stdlib::truncate(&line, 60);
        if short.len() < line.len() { format!("{short}...") } else { line }
    }
}

impl Scopes {
    /// The local names in scope.
    pub(crate) fn names(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.by_name.keys().copied()
    }
}

fn universal(name: &str) -> Option<&'static str> {
    ["to_str", "debug"].into_iter().find(|&u| u == name)
}

fn universal_names() -> Vec<String> {
    vec!["debug".to_string(), "to_str".to_string()]
}

/// `; did you mean `x`?` for the name in `names` closest to `name`, if any is close.
fn suggestion(name: &str, names: Vec<String>) -> String {
    let max = (name.chars().count() / 3).max(1);
    let best = names
        .into_iter()
        .filter(|n| n != name && !n.starts_with('<'))
        .map(|n| (crate::lint::edit_distance(name, &n), n))
        .filter(|(d, _)| *d <= max)
        .min();
    best.map(|(_, n)| format!("; did you mean `{n}`?")).unwrap_or_default()
}

/// A titled list of entries, each with the first sentence of its doc.
fn section(out: &mut String, title: &str, entries: &[(String, Option<Arc<str>>)]) {
    if entries.is_empty() {
        return;
    }
    let _ = writeln!(out, "{title}:");
    for (text, doc) in entries {
        let _ = writeln!(out, "{ITEM}{text}");
        if let Some(doc) = doc.as_deref().map(summary) {
            let words: Vec<String> = doc.split_whitespace().map(String::from).collect();
            fill(out, ITEM_DOC, ITEM_DOC, &words);
        }
    }
    out.push('\n');
}

/// Names and descriptions in two columns.
fn table(out: &mut String, rows: &[(String, String)]) {
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
    let indent = format!("{ITEM}{:width$}", "");
    for (name, text) in rows {
        let words: Vec<String> = text.split_whitespace().map(String::from).collect();
        if words.is_empty() {
            let _ = writeln!(out, "{ITEM}{name}");
        } else {
            fill(out, &format!("{ITEM}{name:width$}"), &indent, &words);
        }
    }
    out.push('\n');
}

/// Comma-separated items, wrapped at 80 columns.
fn wrap(out: &mut String, items: &[String]) {
    let pieces: Vec<String> =
        items.iter().enumerate().map(|(i, x)| if i + 1 < items.len() { format!("{x},") } else { x.clone() }).collect();
    fill(out, ITEM, ITEM, &pieces);
    out.push('\n');
}

/// `words` wrapped at 80 columns, on a line starting with `first`, then on
/// lines starting with `indent`.
fn fill(out: &mut String, first: &str, indent: &str, words: &[String]) {
    if words.is_empty() {
        return;
    }
    let mut line = String::from(first);
    let mut empty = true;
    for w in words {
        if !empty {
            if line.len() + 1 + w.len() > 80 {
                let _ = writeln!(out, "{line}");
                line = String::from(indent);
            } else {
                line.push(' ');
            }
        }
        line.push_str(w);
        empty = false;
    }
    let _ = writeln!(out, "{line}");
}

/// The first sentence of the first paragraph of a doc comment.
pub(crate) fn summary(doc: &str) -> String {
    let para: Vec<&str> = doc.lines().map(str::trim).skip_while(|l| l.is_empty()).take_while(|l| !l.is_empty()).collect();
    let text = para.join(" ");
    let end = text
        .match_indices(". ")
        .map(|(i, _)| i)
        .find(|&i| {
            let word = text[..i].rsplit(' ').next().unwrap_or("").trim_start_matches(|c: char| !c.is_alphanumeric());
            !matches!(word, "e.g" | "i.e" | "etc" | "vs")
        })
        .map_or(text.len(), |i| i + 1);
    text[..end].to_string()
}

pub(crate) fn public_functions(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| l.strip_prefix("pub fn ").or_else(|| l.strip_prefix("pub native fn ")))
        .filter_map(|rest| rest.split(['(', '<']).next())
        .filter(|name| is_ident(name))
        .map(str::to_string)
        .collect()
}

/// The summary of a module's doc comment (its leading `//!` lines).
pub(crate) fn module_summary(src: &str) -> String {
    let doc: Vec<&str> = src
        .lines()
        .map(str::trim_start)
        .take_while(|l| l.is_empty() || l.starts_with("//"))
        .filter_map(|l| l.strip_prefix("//!"))
        .collect();
    summary(&doc.join("\n"))
}

pub(crate) fn is_ident(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Where the first segment of a dotted name ends (at a `.` outside `<...>`).
fn segment_end(s: &str) -> usize {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            '.' if depth == 0 => return i,
            _ => {}
        }
    }
    s.len()
}

/// `Option<T>` -> `Option`.
fn strip_type_args(s: &str) -> &str {
    s.split('<').next().unwrap_or(s).trim()
}

/// The index of the `]` closing the `[` that starts `s`.
fn closing_bracket(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `s` has a `:` outside brackets (`K: V`, not `[K: V]`).
fn top_level_colon(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' | '(' | '<' => depth += 1,
            ']' | ')' | '>' => depth -= 1,
            ':' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_first_sentences() {
        assert_eq!(summary("Parses `text`. Fails on errors."), "Parses `text`.");
        assert_eq!(summary("Splits, e.g. on commas. More."), "Splits, e.g. on commas.");
        assert_eq!(summary("The version (e.g. 4). More."), "The version (e.g. 4).");
        assert_eq!(summary("One line\ncontinued. Next.\n\nOther paragraph."), "One line continued.");
        assert_eq!(module_summary("// x\n//! Files.\n//!\n//! More.\nfn f() {}"), "Files.");
        assert_eq!(module_summary("fn f() {}\n//! Late."), "");
    }
}
