//! Extraction of [`crate::docs::ModuleDoc`] from type-checked modules.

use super::*;
use crate::docs::{ItemDoc, ItemKind, MemberDoc, MemberKind, ModuleDoc, ModuleKind};

const BUILTIN_CTORS: &[Ctor] =
    &[Ctor::Bool, Ctor::Int, Ctor::Float, Ctor::Str, Ctor::Bytes, Ctor::Duration, Ctor::Range, Ctor::List, Ctor::Map, Ctor::Set];

impl Checker {
    pub(crate) fn module_doc(&mut self, m: ModId, kind: ModuleKind) -> ModuleDoc {
        // Omit the current module prefix in item signatures.
        self.cur_mod = m;
        let mut items = Vec::new();
        if kind == ModuleKind::Prelude {
            for &c in BUILTIN_CTORS {
                let doc = self.ctor_docs.get(&c).map(|d| d.to_string());
                let name = self.ctor_name(c, 0);
                items.push(self.type_item(ItemKind::Builtin, c, name.clone(), format!("type {name}"), doc));
            }
        }
        for (name, def) in self.public_items(m) {
            let name = self.name(name).to_string();
            let item = match def {
                Def::Type(TyRef::Def(d)) => {
                    let td = &self.types[d as usize];
                    let doc = td.doc.as_deref().map(str::to_string);
                    let decl = self.type_header(d, false);
                    self.type_item(ItemKind::Type, Ctor::Def(d), name, decl, doc)
                }
                Def::Type(TyRef::Alias(i)) => {
                    let ty = self.resolve_alias(i, Span::default());
                    let decl = format!("type {name} = {}", self.show(ty));
                    item_doc(ItemKind::Alias, name, decl, None)
                }
                Def::Trait(t) => self.trait_item(t, name),
                Def::Fn(f) => {
                    let doc = self.fn_doc(f).as_deref().map(str::to_string);
                    item_doc(ItemKind::Function, name, self.signature(f, false), doc)
                }
                Def::Global(g) => {
                    let gi = &self.globals[g as usize];
                    let kw = if gi.mutable { "var" } else { "let" };
                    let decl = format!("{kw} {name}: {}", self.show(gi.ty));
                    item_doc(ItemKind::Value, name, decl, gi.doc.as_deref().map(str::to_string))
                }
                _ => continue,
            };
            items.push(item);
        }
        let info = &self.mods[m as usize];
        ModuleDoc { name: self.name(info.name).to_string(), kind, doc: info.doc.as_deref().map(str::to_string), items }
    }

    fn public_items(&self, m: ModId) -> Vec<(Symbol, Def)> {
        let info = &self.mods[m as usize];
        let mut items: Vec<(Symbol, Binding)> = info
            .scope
            .entries()
            .into_iter()
            .filter(|(_, b)| b.is_pub && b.span.file == info.file && !matches!(b.def, Def::Module(_) | Def::Variant(..)))
            .map(|(&n, &b)| (n, b))
            .collect();
        items.sort_by(|(m, a), (n, b)| (a.span.lo, self.name(*m)).cmp(&(b.span.lo, self.name(*n))));
        items.into_iter().map(|(n, b)| (n, b.def)).collect()
    }

    fn type_item(&mut self, kind: ItemKind, c: Ctor, name: String, decl: String, doc: Option<String>) -> ItemDoc {
        let mut members = Vec::new();
        if let Ctor::Def(d) = c {
            let def = &self.types[d as usize];
            for (i, v) in def.variants().iter().enumerate() {
                let decl = self.variant_text(d, i as u32);
                members.push(member_doc(MemberKind::Variant, self.name(v.name), decl, &v.doc));
            }
            for f in def.fields().iter().filter(|f| f.is_pub) {
                let var = if f.mutable { "var " } else { "" };
                let default = self.default_text(d, f);
                let decl = format!("{var}{}: {}{default}", self.name(f.name), self.show(f.ty));
                members.push(member_doc(MemberKind::Field, self.name(f.name), decl, &f.doc));
            }
        }
        for f in self.methods_of(c) {
            let fi = &self.fns[f as usize];
            if fi.is_pub {
                members.push(member_doc(MemberKind::Method, self.name(fi.name), self.signature(f, false), &self.fn_doc(f)));
            }
        }
        let mut related = Vec::new();
        for i in self.impls_by_ctor.get(&(c, 0)).cloned().unwrap_or_default().iter() {
            let tr = &self.impls[*i as usize].trait_ref;
            if self.traits[tr.trait_id as usize].is_pub {
                related.push(self.show_trait(tr));
            }
        }
        ItemDoc { kind, name, decl, doc, members, related }
    }

    fn trait_item(&mut self, t: TraitId, name: String) -> ItemDoc {
        let td = &self.traits[t as usize];
        let mut decl = format!("trait {}", self.trait_header_name(t));
        let mut supers: Vec<String> = td.supers.iter().map(|s| self.show_trait(s)).collect();
        supers.extend(td.builtin_supers.iter().map(|b| b.name().to_string()));
        if !supers.is_empty() {
            decl = format!("{decl}: {}", supers.join(" + "));
        }
        let members = td
            .methods
            .iter()
            .map(|m| member_doc(MemberKind::Method, self.name(m.name), self.signature(m.sig, false), &self.fns[m.sig as usize].doc))
            .collect();
        let mut related: Vec<String> = Vec::new();
        for imp in self.impls.iter().filter(|i| i.trait_ref.trait_id == t) {
            let name = self.item_name(imp.target);
            if !related.contains(&name) {
                related.push(name);
            }
        }
        ItemDoc { kind: ItemKind::Trait, name, decl, doc: td.doc.as_deref().map(str::to_string), members, related }
    }

    /// Returns the canonical item name for type `t` (e.g. `List<T>` for `[int]`,
    /// `Option<T>` for `str?`).
    fn item_name(&self, t: TypeId) -> String {
        match traits::impl_key(&self.arena, t) {
            Some((Ctor::Def(d), _)) => self.type_header_name(d),
            Some((c, arity)) => self.ctor_name(c, arity),
            None => self.show(t),
        }
    }
}

fn item_doc(kind: ItemKind, name: String, decl: String, doc: Option<String>) -> ItemDoc {
    ItemDoc { kind, name, decl, doc, members: Vec::new(), related: Vec::new() }
}

fn member_doc(kind: MemberKind, name: &str, decl: String, doc: &Option<Arc<str>>) -> MemberDoc {
    MemberDoc { kind, name: name.to_string(), decl, doc: doc.as_deref().map(str::to_string) }
}
