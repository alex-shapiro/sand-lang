//! Derive macros that bind Rust types to Sand types.
//!
//! Each type requires a `#[sand(module = "<module>")]` attribute and accepts
//! optional type attributes:
//!
//! - `#[sand(name = "<Name>")]` overrides the Sand type name.
//! - `#[sand(private)]` makes the type private.
//! - `#[sand(crate = <path>)]` overrides the `::sandlang` crate path.
//! - `#[sand(manual_conversion)]` omits `FromValue` and `IntoValue` implementations.
//!
//! Rust doc comments, including `#[doc = include_str!("...")]`, are copied into Sand docs.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Field, Fields, GenericParam, Ident, Lit, LitStr, Meta, Path, Type, WherePredicate,
    parse_macro_input, parse_quote,
};

/// Declare a sandlang enum.
///
/// Implements `NamedType`, `EnumType`, `SandType`, `FromValue`, and `IntoValue`.
/// Supports unit, tuple, and struct variants, and type parameters. Each payload
/// type must implement `IntoValue` and owned `FromValue`.
///
/// Use `#[sand(name = "<name>")]` on a struct variant field to override its Sand
/// field name.
#[proc_macro_derive(SandEnum, attributes(sand))]
pub fn derive_sand_enum(input: TokenStream) -> TokenStream {
    expand(input, sand_enum)
}

/// Declare a sandlang struct.
///
/// Implements `NamedType`, `StructType`, `SandType`, `FromValue`, and `IntoValue`.
/// Each field type must implement `IntoValue` and owned `FromValue`.
/// Rust type parameters define Sand type parameters.
///
/// Optional field attributes:
/// - `#[sand(name = "<name>")]`: overrides the Sand field name.
/// - `#[sand(private)]`: makes the field private to its module.
/// - `#[sand(var)]`: makes the field mutable in Sand.
/// - `#[sand(default)]` or `#[sand(default = <expr>)]`: sets the Sand default
///   value to `Default::default()` or `Into::into(<expr>)`. `IntoValue::to_const`
///   must return `Some` for the default value.
#[proc_macro_derive(SandStruct, attributes(sand))]
pub fn derive_sand_struct(input: TokenStream) -> TokenStream {
    expand(input, sand_struct)
}

/// Declare an sandlang opaque native type.
///
/// Implements `NamedType`, `ObjectType`, `SandType`, `FromValue`, and `IntoValue`
/// The type must implement the `NativeObject` marker trait.
#[proc_macro_derive(SandObject, attributes(sand))]
pub fn derive_sand_object(input: TokenStream) -> TokenStream {
    expand(input, sand_object)
}

/// Declare a sandlang value type.
///
/// Implements `NamedType`, `SandType`, `FromValue`, and `IntoValue`.
/// The type must implement the `ValueType` marker trait.
#[proc_macro_derive(SandValue, attributes(sand))]
pub fn derive_sand_value(input: TokenStream) -> TokenStream {
    expand(input, sand_value)
}

fn expand(input: TokenStream, derive: fn(&DeriveInput) -> syn::Result<Tokens>) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    derive(&input).unwrap_or_else(Error::into_compile_error).into()
}

struct TypeAttrs {
    module: LitStr,
    name: LitStr,
    public: bool,
    krate: Path,
    doc: Tokens,
    manual_conversion: bool,
}

fn parse_type_attrs(input: &DeriveInput, derive: &str) -> syn::Result<TypeAttrs> {
    let ty = &input.ident;
    let mut module = None;
    let mut name = LitStr::new(&ty.unraw().to_string(), ty.span());
    let mut public = true;
    let mut manual_conversion = false;
    let mut krate: Path = parse_quote!(::sandlang);
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("sand")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("module") {
                module = Some(meta.value()?.parse::<LitStr>()?);
            } else if meta.path.is_ident("name") {
                name = meta.value()?.parse()?;
            } else if meta.path.is_ident("private") {
                public = false;
            } else if meta.path.is_ident("crate") {
                krate = meta.value()?.parse()?;
            } else if meta.path.is_ident("manual_conversion") && matches!(derive, "SandEnum" | "SandStruct") {
                manual_conversion = true;
            } else if matches!(derive, "SandEnum" | "SandStruct") {
                return Err(meta.error("expected `module`, `name`, `private`, `crate` or `manual_conversion`"));
            } else {
                return Err(meta.error("expected `module`, `name`, `private` or `crate`"));
            }
            Ok(())
        })?;
    }
    let Some(module) = module else {
        return Err(Error::new(Span::call_site(), format!("`{derive}` requires `#[sand(module = \"...\")]`")));
    };
    Ok(TypeAttrs { module, name, public, krate, doc: doc_expr(&input.attrs), manual_conversion })
}

struct TypeParams {
    params: Vec<Ident>,
    canonical_self: Type,
}

impl TypeParams {
    fn empty(input: &DeriveInput, derive: &str) -> syn::Result<Self> {
        match input.generics.params.is_empty() {
            true => {
                let ty = &input.ident;
                Ok(Self { params: Vec::new(), canonical_self: parse_quote!(#ty) })
            }
            false => Err(Error::new_spanned(
                &input.generics,
                format!("`{derive}` cannot be derived for a type with type or lifetime parameters"),
            )),
        }
    }

    fn parse(input: &DeriveInput, derive: &str, krate: &Path) -> syn::Result<Self> {
        let mut params = Vec::new();
        for p in &input.generics.params {
            match p {
                GenericParam::Type(t) => params.push(t.ident.clone()),
                _ => {
                    return Err(Error::new_spanned(
                        p,
                        format!("`{derive}` cannot be derived for a type with lifetime or const parameters"),
                    ));
                }
            }
        }
        if params.len() > 255 {
            return Err(Error::new_spanned(&input.generics, "a Sand type has at most 255 type parameters"));
        }
        let ty = &input.ident;
        let canonical_self = match params.len() {
            0 => parse_quote!(#ty),
            n => {
                let i = (0..n).map(|i| i as u8);
                parse_quote!(#ty<#(#krate::native::Param<#i>),*>)
            }
        };
        Ok(Self { params, canonical_self })
    }

    fn substitute(&self, krate: &Path, ty: &Type) -> Type {
        struct Subst<'a>(&'a TypeParams, &'a Path);
        impl VisitMut for Subst<'_> {
            fn visit_type_mut(&mut self, t: &mut Type) {
                if let Type::Path(p) = t
                    && p.qself.is_none()
                    && let Some(id) = p.path.get_ident()
                {
                    if id == "Self" {
                        *t = self.0.canonical_self.clone();
                        return;
                    }
                    if let Some(i) = self.0.params.iter().position(|x| x == id) {
                        let (i, krate) = (i as u8, self.1);
                        *t = parse_quote!(#krate::native::Param<#i>);
                        return;
                    }
                }
                syn::visit_mut::visit_type_mut(self, t);
            }
        }
        let mut t = ty.clone();
        Subst(self, krate).visit_type_mut(&mut t);
        t
    }

    fn split_for_impl(&self, input: &DeriveInput, bound: impl Fn(&Ident) -> Tokens) -> (Tokens, Tokens, Tokens) {
        let (imp, ty, wh) = input.generics.split_for_impl();
        let mut preds: Vec<WherePredicate> = wh.map(|w| w.predicates.iter().cloned().collect()).unwrap_or_default();
        for p in &self.params {
            let b = bound(p);
            preds.push(parse_quote!(#p: 'static + #b));
        }
        let wh = if preds.is_empty() { quote!() } else { quote!(where #(#preds),*) };
        (quote!(#imp), quote!(#ty), wh)
    }
}

fn assert_type_name(krate: &Path, kind: &str, name: &str, span: Span) -> Tokens {
    let msg = format!("the Sand {kind} name `{name}` is not a capitalized identifier, or is a keyword");
    quote_spanned! {span=> const _: () = ::std::assert!(#krate::native::is_type_name(#name), #msg); }
}

fn assert_field_name(krate: &Path, name: &LitStr, rename: bool) -> Tokens {
    let hint = if rename { "; rename the field with `#[sand(name = \"...\")]`" } else { "" };
    let msg = format!("the Sand field name `{}` is not an identifier, or is a keyword{hint}", name.value());
    quote_spanned! {name.span()=> const _: () = ::std::assert!(#krate::native::is_name(#name), #msg); }
}

fn reject_member_attrs(data: &Data) -> syn::Result<()> {
    fn fields(f: &Fields) -> impl Iterator<Item = &Attribute> {
        f.iter().flat_map(|f| &f.attrs)
    }
    let (attrs, msg): (Vec<&Attribute>, _) = match data {
        Data::Struct(s) => (fields(&s.fields).collect(), "`#[sand(...)]` applies only to the type"),
        Data::Enum(e) => (
            e.variants
                .iter()
                .flat_map(|v| v.attrs.iter().chain(fields(&v.fields).filter(|_| !matches!(v.fields, Fields::Named(_)))))
                .collect(),
            "`#[sand(...)]` applies only to the type and to named payload fields",
        ),
        Data::Union(u) => (u.fields.named.iter().flat_map(|f| &f.attrs).collect(), "`#[sand(...)]` applies only to the type"),
    };
    match attrs.into_iter().find(|a| a.path().is_ident("sand")) {
        Some(a) => Err(Error::new_spanned(a, msg)),
        None => Ok(()),
    }
}

fn expand_named_type(input: &DeriveInput, a: &TypeAttrs, type_params: &TypeParams, binding: Tokens) -> Tokens {
    let TypeAttrs { module, name, public, krate, doc, .. } = a;
    let ty = &input.ident;
    let check = assert_type_name(krate, "type", &name.value(), name.span());
    let (imp, tyg, wh) = type_params.split_for_impl(input, |_| quote!(#krate::native::SandType));
    let params: Vec<String> = type_params.params.iter().map(|p| p.unraw().to_string()).collect();
    let checks = type_params.params.iter().zip(&params).map(|(p, n)| assert_type_name(krate, "type parameter", n, p.span()));
    // Declare a non-generic local type inside the method so all monomorphizations return the same TypeId.
    let rust_type = (!type_params.params.is_empty()).then(|| {
        quote! {
            fn rust_type() -> ::std::any::TypeId {
                enum Instance {}
                ::std::any::TypeId::of::<Instance>()
            }
        }
    });
    quote! {
        #check
        #(#checks)*
        impl #imp #krate::native::NamedType for #ty #tyg #wh {
            const MODULE: &'static str = #module;
            const NAME: &'static str = #name;
            const DOC: &'static str = #doc;
            const PUBLIC: bool = #public;
            const PARAMS: &'static [&'static str] = &[#(#params),*];
            #rust_type
            fn binding() -> #krate::native::Binding<Self> {
                #binding
            }
        }
    }
}

fn expand_sand_type(input: &DeriveInput, krate: &Path, type_params: &TypeParams) -> Tokens {
    let ty = &input.ident;
    let (imp, tyg, wh) = type_params.split_for_impl(input, |_| quote!(#krate::native::SandType));
    let args = &type_params.params;
    quote! {
        impl #imp #krate::native::SandType for #ty #tyg #wh {
            fn sand_type() -> #krate::native::Type {
                #krate::native::Type::bound::<Self>(::std::vec![#(<#args as #krate::native::SandType>::sand_type()),*])
            }
        }
    }
}

fn split_conversion_generics(input: &DeriveInput, krate: &Path, type_params: &TypeParams) -> (Tokens, Tokens, Tokens, Tokens, Tokens) {
    let (imp, tyg, wh) = type_params.split_for_impl(input, |_| quote!(#krate::native::SandType));
    let (_, _, from) = type_params.split_for_impl(input, |p| quote!(for<'a> #krate::native::FromValue<Item<'a> = #p>));
    let (_, _, into) = type_params.split_for_impl(input, |_| quote!(#krate::native::IntoValue));
    (imp, tyg, wh, from, into)
}

fn parse_variant_field_name(f: &Field, ident: &Ident) -> syn::Result<LitStr> {
    let mut name = LitStr::new(&ident.unraw().to_string(), ident.span());
    for attr in f.attrs.iter().filter(|a| a.path().is_ident("sand")) {
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("name") {
                return Err(meta.error("expected `name`"));
            }
            name = meta.value()?.parse()?;
            Ok(())
        })?;
    }
    Ok(name)
}

fn sand_enum(input: &DeriveInput) -> syn::Result<Tokens> {
    let ty = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(ty, "`SandEnum` can only be derived for enums"));
    };
    let attrs = parse_type_attrs(input, "SandEnum")?;
    let type_params = TypeParams::parse(input, "SandEnum", &attrs.krate)?;
    reject_member_attrs(&input.data)?;
    if data.variants.is_empty() {
        return Err(Error::new_spanned(ty, "a Sand enum must have at least one variant"));
    }
    let krate = &attrs.krate;
    let mut variants = Vec::new();
    let mut indexes = Vec::new();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut consts = Vec::new();
    let mut checks = Vec::new();
    for (i, v) in data.variants.iter().enumerate() {
        let (ident, vname, vdoc) = (&v.ident, v.ident.unraw().to_string(), doc_expr(&v.attrs));
        checks.push(assert_type_name(krate, "variant", &vname, ident.span()));
        let mut infos = Vec::new();
        let mut names = Vec::new();
        let mut gets = Vec::new();
        let mut puts = Vec::new();
        let mut binds = Vec::new();
        let mut snames = Vec::new();
        for (k, f) in v.fields.iter().enumerate() {
            let (fname, fty) = (f.ident.as_ref(), &f.ty);
            let canon = type_params.substitute(krate, fty);
            let sname = match fname {
                Some(n) => {
                    let lit = parse_variant_field_name(f, n)?;
                    if snames.contains(&lit.value()) {
                        return Err(Error::new_spanned(&lit, format!("the Sand payload name `{}` is used twice", lit.value())));
                    }
                    snames.push(lit.value());
                    checks.push(assert_field_name(krate, &lit, true));
                    quote!(::std::option::Option::Some(#lit))
                }
                None => quote!(::std::option::Option::None),
            };
            infos.push(quote! { #krate::native::VariantField { name: #sname, ty: <#canon as #krate::native::SandType>::sand_type } });
            let bind = format_ident!("f{k}");
            let get = quote! { #krate::native::field_from_value::<#fty>(vm, #krate::native::variant_field(vm, v, #k)?, conv)? };
            match fname {
                Some(n) => {
                    gets.push(quote!(#n: #get));
                    binds.push(quote!(#n: #bind));
                }
                None => {
                    gets.push(get);
                    binds.push(quote!(#bind));
                }
            }
            names.push(bind.clone());
            puts.push(quote!(#krate::native::IntoValue::into_value(#bind, vm)?));
        }
        variants.push(quote! {
            #krate::native::EnumVariant { name: #vname, doc: #vdoc, fields: &[#(#infos),*] }
        });
        let (pattern, build) = match &v.fields {
            Fields::Unit => (quote!(#ty::#ident), quote!(#ty::#ident)),
            Fields::Unnamed(_) => (quote!(#ty::#ident(#(#binds),*)), quote!(#ty::#ident(#(#gets),*))),
            Fields::Named(_) => (quote!(#ty::#ident { #(#binds),* }), quote!(#ty::#ident { #(#gets),* })),
        };
        let skip = match &v.fields {
            Fields::Unit => quote!(#ty::#ident),
            Fields::Unnamed(_) => quote!(#ty::#ident(..)),
            Fields::Named(_) => quote!(#ty::#ident { .. }),
        };
        indexes.push(quote! { #skip => #i });
        reads.push(quote! { #i => #build });
        let write = match names.is_empty() {
            true => quote!(|_| ::std::result::Result::Ok([])),
            false => quote!(move |vm| ::std::result::Result::Ok([#(#puts),*])),
        };
        writes.push(quote! { #pattern => #krate::native::variant_into_value::<Self, _>(vm, #i, #write) });
        consts.push(quote! { #pattern => #krate::native::variant_const::<Self>(#i, [#(#krate::native::IntoValue::to_const(#names)),*]) });
    }
    let named = expand_named_type(input, &attrs, &type_params, quote! { #krate::native::Binding::of_enum() });
    let sand_type = expand_sand_type(input, krate, &type_params);
    let (imp, tyg, wh, wh_from, wh_into) = split_conversion_generics(input, krate, &type_params);
    let reflection = quote! {
        #named
        #sand_type
        #(#checks)*
        impl #imp #krate::native::EnumType for #ty #tyg #wh {
            const VARIANTS: &'static [#krate::native::EnumVariant] = &[#(#variants),*];
            fn index(&self) -> usize {
                match self {
                    #(#indexes),*
                }
            }
        }
    };
    if attrs.manual_conversion {
        return Ok(reflection);
    }
    Ok(quote! {
        #reflection
        impl #imp #krate::native::FromValue for #ty #tyg #wh_from {
            type Item<'a> = Self;
            #[allow(unused_variables)]
            fn from_value(
                vm: &#krate::vm::Vm,
                v: #krate::Value,
                conv: &mut #krate::native::Conversion,
            ) -> ::std::result::Result<Self, #krate::vm::Ctl> {
                #krate::native::enum_from_value::<Self>(vm, v, conv, |index, conv| {
                    ::std::result::Result::Ok(match index {
                        #(#reads,)*
                        _ => return ::std::result::Result::Err(#krate::native::mismatch::<Self>()),
                    })
                })
            }
        }
        impl #imp #krate::native::IntoValue for #ty #tyg #wh_into {
            fn into_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::Value, #krate::vm::Ctl> {
                match self {
                    #(#writes),*
                }
            }
            fn to_const(&self) -> ::std::option::Option<#krate::native::Constant> {
                match self {
                    #(#consts),*
                }
            }
        }
    })
}

fn sand_object(input: &DeriveInput) -> syn::Result<Tokens> {
    let ty = &input.ident;
    if matches!(input.data, Data::Union(_)) {
        return Err(Error::new_spanned(ty, "`SandObject` cannot be derived for unions"));
    }
    let attrs = parse_type_attrs(input, "SandObject")?;
    let empty_params = TypeParams::empty(input, "SandObject")?;
    reject_member_attrs(&input.data)?;
    let krate = &attrs.krate;
    let named = expand_named_type(input, &attrs, &empty_params, quote! { #krate::native::Binding::of_object() });
    let sand_type = expand_sand_type(input, krate, &empty_params);
    Ok(quote! {
        #named
        #sand_type
        impl #krate::native::ObjectType for #ty {}
        impl #krate::native::SandType for &#ty {
            fn sand_type() -> #krate::native::Type {
                <#ty as #krate::native::SandType>::sand_type()
            }
        }
        impl<'r> #krate::native::FromValue for &'r #ty {
            type Item<'a> = &'a #ty;
            fn from_value<'a>(
                vm: &'a #krate::vm::Vm,
                v: #krate::Value,
                _: &mut #krate::native::Conversion,
            ) -> ::std::result::Result<&'a #ty, #krate::vm::Ctl> {
                #krate::native::Val::<#ty>::from_raw(v).get(&vm.heap)
            }
        }
        impl #krate::native::IntoValue for #ty {
            fn into_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::Value, #krate::vm::Ctl> {
                vm.new_object(self).map(#krate::native::Val::raw)
            }
        }
    })
}

fn sand_struct(input: &DeriveInput) -> syn::Result<Tokens> {
    let ty = &input.ident;
    let Data::Struct(data) = &input.data else {
        return Err(Error::new_spanned(ty, "`SandStruct` can only be derived for structs"));
    };
    let attrs = parse_type_attrs(input, "SandStruct")?;
    let type_params = TypeParams::parse(input, "SandStruct", &attrs.krate)?;
    let krate = &attrs.krate;
    let fields = match &data.fields {
        Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
        Fields::Unit => Vec::new(),
        Fields::Unnamed(f) => return Err(Error::new_spanned(f, "`SandStruct` requires named fields")),
    };
    let mut infos = Vec::new();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut names = Vec::new();
    let mut consts = Vec::new();
    let mut checks = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let ident = f.ident.as_ref().expect("named field");
        let (mut public, mut var) = (true, false);
        let mut name = LitStr::new(&ident.unraw().to_string(), ident.span());
        let mut default: Option<Option<Expr>> = None;
        for attr in f.attrs.iter().filter(|a| a.path().is_ident("sand")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    name = meta.value()?.parse::<LitStr>()?;
                } else if meta.path.is_ident("private") {
                    public = false;
                } else if meta.path.is_ident("var") {
                    var = true;
                } else if meta.path.is_ident("default") {
                    default = Some(if meta.input.peek(syn::Token![=]) { Some(meta.value()?.parse()?) } else { None });
                } else {
                    return Err(meta.error("expected `name`, `private`, `var` or `default`"));
                }
                Ok(())
            })?;
        }
        if names.contains(&name.value()) {
            return Err(Error::new_spanned(&name, format!("the Sand field name `{}` is used twice", name.value())));
        }
        names.push(name.value());
        checks.push(assert_field_name(krate, &name, true));
        let (fty, doc) = (&f.ty, doc_expr(&f.attrs));
        let canon = type_params.substitute(krate, fty);
        let default = match default {
            None => quote!(::std::option::Option::None),
            Some(value) => {
                let value = value.map_or_else(|| quote!(<#canon as ::std::default::Default>::default()), |e| quote!(#e));
                quote_spanned! {fty.span()=>
                    ::std::option::Option::Some(|| {
                        let value: #canon = ::std::convert::Into::into(#value);
                        #krate::native::IntoValue::to_const(&value)
                    })
                }
            }
        };
        infos.push(quote! {
            #krate::native::StructField {
                name: #name,
                doc: #doc,
                public: #public,
                var: #var,
                ty: <#canon as #krate::native::SandType>::sand_type,
                default: #default,
            }
        });
        reads.push(quote! { #ident: #krate::native::field_from_value::<#fty>(vm, fields[#i], conv)? });
        writes.push(quote! { #krate::native::IntoValue::into_value(self.#ident, vm)? });
        consts.push(quote! { #krate::native::IntoValue::to_const(&self.#ident) });
    }
    let named = expand_named_type(input, &attrs, &type_params, quote! { #krate::native::Binding::of_struct() });
    let sand_type = expand_sand_type(input, krate, &type_params);
    let (imp, tyg, wh, wh_from, wh_into) = split_conversion_generics(input, krate, &type_params);
    let reflection = quote! {
        #named
        #sand_type
        #(#checks)*
        impl #imp #krate::native::StructType for #ty #tyg #wh {
            const FIELDS: &'static [#krate::native::StructField] = &[#(#infos),*];
        }
    };
    if attrs.manual_conversion {
        return Ok(reflection);
    }
    Ok(quote! {
        #reflection
        impl #imp #krate::native::FromValue for #ty #tyg #wh_from {
            type Item<'a> = Self;
            #[allow(unused_variables)]
            fn from_value(
                vm: &#krate::vm::Vm,
                v: #krate::Value,
                conv: &mut #krate::native::Conversion,
            ) -> ::std::result::Result<Self, #krate::vm::Ctl> {
                #krate::native::struct_from_value(vm, v, conv, |fields, conv| ::std::result::Result::Ok(#ty { #(#reads),* }))
            }
        }
        impl #imp #krate::native::IntoValue for #ty #tyg #wh_into {
            fn into_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::Value, #krate::vm::Ctl> {
                #krate::native::struct_into_value::<Self>(vm, move |vm| ::std::result::Result::Ok(::std::boxed::Box::new([#(#writes),*])))
            }
            fn to_const(&self) -> ::std::option::Option<#krate::native::Constant> {
                #krate::native::struct_const::<Self>([#(#consts),*])
            }
        }
    })
}

fn sand_value(input: &DeriveInput) -> syn::Result<Tokens> {
    let ty = &input.ident;
    if matches!(input.data, Data::Union(_)) {
        return Err(Error::new_spanned(ty, "`SandValue` cannot be derived for unions"));
    }
    let attrs = parse_type_attrs(input, "SandValue")?;
    let empty_params = TypeParams::empty(input, "SandValue")?;
    reject_member_attrs(&input.data)?;
    let krate = &attrs.krate;
    let named = expand_named_type(input, &attrs, &empty_params, quote! { #krate::native::Binding::of_value() });
    Ok(quote! {
        #named
        #krate::__value_type_impls!(#ty);
    })
}

fn doc_expr(attrs: &[Attribute]) -> Tokens {
    let mut parts = Vec::new();
    for a in attrs {
        let Meta::NameValue(nv) = &a.meta else { continue };
        if !nv.path.is_ident("doc") {
            continue;
        }
        if !parts.is_empty() {
            parts.push(quote! { "\n" });
        }
        parts.push(match &nv.value {
            Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => {
                let line = s.value();
                let line = line.strip_prefix(' ').unwrap_or(&line);
                quote! { #line }
            }
            e => quote! { #e },
        });
    }
    quote! { ::std::concat!(#(#parts),*).trim_ascii() }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Derive = fn(&DeriveInput) -> syn::Result<Tokens>;

    fn expand(f: Derive, src: &str) -> String {
        let input: DeriveInput = syn::parse_str(src).unwrap();
        f(&input).map(|t| t.to_string()).unwrap_or_else(|e| format!("error: {e}"))
    }

    #[test]
    fn the_crate_attribute_sets_the_path_of_every_item() {
        let derives: [(Derive, &str); 5] = [
            (sand_enum, "enum E { A, B(i64), C { x: T } }"),
            (sand_object, "struct O;"),
            (sand_struct, "struct S<T> { #[sand(default)] x: i64, y: T }"),
            (sand_value, "struct V(i64);"),
            (sand_enum, "enum G<T> { A(T) }"),
        ];
        for (f, item) in derives {
            let out = expand(f, &format!("#[sand(module = \"m\", crate = ::renamed)] {item}"));
            assert!(out.contains(":: renamed :: native"), "{out}");
            assert!(!out.contains("sandlang"), "{out}");
        }
    }

    #[test]
    fn docs_are_concatenated_and_trimmed_at_compile_time() {
        let out = expand(sand_enum, "#[sand(module = \"m\")]\n/// One.\n///\n///   Two.\nenum E { A }");
        assert!(out.contains(":: std :: concat ! (\"One.\" , \"\\n\" , \"\" , \"\\n\" , \"  Two.\") . trim_ascii ()"), "{out}");
        let out = expand(
            sand_struct,
            "#[sand(module = \"m\")]\n/// Head.\n#[doc = include_str!(\"x.md\")]\nstruct S { #[doc = include_str!(\"f.md\")] x: i64 }",
        );
        assert!(out.contains(":: std :: concat ! (\"Head.\" , \"\\n\" , include_str ! (\"x.md\")) . trim_ascii ()"), "{out}");
        assert!(out.contains(":: std :: concat ! (include_str ! (\"f.md\")) . trim_ascii ()"), "{out}");
    }

    #[test]
    fn struct_fields_take_attributes() {
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S { #[sand(private, var)] r#type: i64, y: bool }");
        assert!(out.contains("name : \"type\" , doc : :: std :: concat ! () . trim_ascii () , public : false , var : true"), "{out}");
        assert!(out.contains("name : \"y\" , doc : :: std :: concat ! () . trim_ascii () , public : true , var : false"), "{out}");
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S { #[sand(hidden)] x: i64 }");
        assert_eq!(out, "error: expected `name`, `private`, `var` or `default`");
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S { #[sand(name = \"kind\")] r#type: i64 }");
        assert!(out.contains("name : \"kind\""), "{out}");
    }

    #[test]
    fn field_defaults_are_constants_of_the_field_type() {
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S { #[sand(default)] x: i64, #[sand(default = 2)] y: i64, z: i64 }");
        assert!(
            out.contains("let value : i64 = :: std :: convert :: Into :: into (< i64 as :: std :: default :: Default > :: default ())"),
            "{out}"
        );
        assert!(out.contains("let value : i64 = :: std :: convert :: Into :: into (2) ;"), "{out}");
        assert!(out.contains(":: sandlang :: native :: IntoValue :: to_const (& value)"), "{out}");
        assert_eq!(out.matches("default : :: std :: option :: Option :: None").count(), 1, "{out}");
    }

    #[test]
    fn generic_types_define_sand_type_parameters() {
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct Pair<A, B> { a: A, b: Vec<B>, c: Option<(A, B)> }");
        assert!(out.contains("const PARAMS : & 'static [& 'static str] = & [\"A\" , \"B\"]"), "{out}");
        assert!(out.contains("enum Instance { } :: std :: any :: TypeId :: of :: < Instance > ()"), "{out}");
        assert!(out.contains("ty : < :: sandlang :: native :: Param < 0u8 > as :: sandlang :: native :: SandType > :: sand_type"), "{out}");
        assert!(out.contains("ty : < Vec < :: sandlang :: native :: Param < 1u8 > > as"), "{out}");
        assert!(out.contains("Option < (:: sandlang :: native :: Param < 0u8 > , :: sandlang :: native :: Param < 1u8 >) >"), "{out}");
        assert!(out.contains("A : 'static + for < 'a > :: sandlang :: native :: FromValue < Item < 'a > = A >"), "{out}");
        let out = expand(sand_enum, "#[sand(module = \"m\")] enum E<T> { A(T), B(other::T) }");
        assert!(out.contains("ty : < other :: T as"), "{out}");
    }

    #[test]
    fn enum_variants_take_payloads() {
        let out = expand(sand_enum, "#[sand(module = \"m\")] enum E { A, B(i64, String), C { w: f64 } }");
        assert!(out.contains("EnumVariant { name : \"A\" , doc : :: std :: concat ! () . trim_ascii () , fields : & [] }"), "{out}");
        assert!(out.contains("VariantField { name : :: std :: option :: Option :: None , ty : < i64 as"), "{out}");
        assert!(out.contains("VariantField { name : :: std :: option :: Option :: Some (\"w\") , ty : < f64 as"), "{out}");
        assert!(out.contains("E :: B (..) => 1usize"), "{out}");
        assert!(out.contains("E :: C { w : f0 } => :: sandlang :: native :: variant_into_value :: < Self , _ > (vm , 2usize"), "{out}");
        let out = expand(sand_enum, "#[sand(module = \"m\")] enum E { A { #[sand(name = \"kind\")] r#type: i64 } }");
        assert!(out.contains("VariantField { name : :: std :: option :: Option :: Some (\"kind\")"), "{out}");
        assert!(out.contains("E :: A { r#type : f0 }"), "{out}");
    }

    #[test]
    fn names_are_checked_by_constants() {
        let out = expand(sand_enum, "#[sand(module = \"m\", name = \"Self\")] enum E { A, b, C { r#fn: i64 } }");
        assert!(out.contains(":: sandlang :: native :: is_type_name (\"Self\")"), "{out}");
        assert!(out.contains(":: sandlang :: native :: is_type_name (\"A\")"), "{out}");
        assert!(out.contains(":: sandlang :: native :: is_type_name (\"b\")"), "{out}");
        assert!(out.contains(":: sandlang :: native :: is_name (\"fn\")"), "{out}");
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S<t> { r#fn: i64, #[sand(name = \"two words\")] y: t }");
        assert!(out.contains(":: sandlang :: native :: is_name (\"fn\")"), "{out}");
        assert!(out.contains(":: sandlang :: native :: is_name (\"two words\")"), "{out}");
        assert!(out.contains(":: sandlang :: native :: is_type_name (\"t\")"), "{out}");
    }

    #[test]
    fn invalid_derives_are_rejected() {
        let cases: [(Derive, &str, &str); 14] = [
            (sand_struct, "#[sand(module = \"m\")] struct S(i64);", "`SandStruct` requires named fields"),
            (sand_struct, "#[sand(module = \"m\")] enum S { A }", "`SandStruct` can only be derived for structs"),
            (sand_struct, "#[sand(module = \"m\")] struct S<'a> { x: &'a str }", "lifetime or const parameters"),
            (sand_enum, "#[sand(module = \"m\")] enum E<const N: usize> { A }", "lifetime or const parameters"),
            (sand_object, "#[sand(module = \"m\")] struct O<T>(T);", "type or lifetime parameters"),
            (sand_struct, "struct S { x: i64 }", "requires `#[sand(module = \"...\")]`"),
            (
                sand_struct,
                "#[sand(module = \"m\")] struct S { x: i64, #[sand(name = \"x\")] y: i64 }",
                "the Sand field name `x` is used twice",
            ),
            (sand_value, "#[sand(module = \"m\")] struct V { #[sand(private)] x: i64 }", "applies only to the type"),
            (sand_enum, "#[sand(module = \"m\")] enum E { #[sand(name = \"B\")] A }", "applies only to the type"),
            (sand_enum, "#[sand(module = \"m\")] enum E { A(#[sand(name = \"x\")] i64) }", "and to named payload fields"),
            (sand_enum, "#[sand(module = \"m\")] enum E { A { #[sand(var)] x: i64 } }", "expected `name`"),
            (
                sand_enum,
                "#[sand(module = \"m\")] enum E { A { #[sand(name = \"y\")] x: i64, y: i64 } }",
                "the Sand payload name `y` is used twice",
            ),
            (
                sand_enum,
                "#[sand(module = \"m\", public)] enum E { A }",
                "expected `module`, `name`, `private`, `crate` or `manual_conversion`",
            ),
            (sand_object, "#[sand(module = \"m\", manual_conversion)] struct O;", "expected `module`, `name`, `private` or `crate`"),
        ];
        for (f, src, msg) in cases {
            let out = expand(f, src);
            assert!(out.starts_with("error: ") && out.contains(msg), "{src}: {out}");
        }
    }

    #[test]
    fn manual_conversion_omits_conversion_impls() {
        let cases: [(Derive, &str, &str); 2] =
            [(sand_enum, "enum E { A(i64) }", "EnumType for E"), (sand_struct, "struct S { x: i64 }", "StructType for S")];
        for (f, item, reflection) in cases {
            let derived = expand(f, &format!("#[sand(module = \"m\")] {item}"));
            let manual = expand(f, &format!("#[sand(module = \"m\", manual_conversion)] {item}"));
            for out in [&derived, &manual] {
                assert!(out.contains("NamedType for") && out.contains("SandType for") && out.contains(reflection), "{out}");
            }
            assert!(derived.contains("FromValue for") && derived.contains("IntoValue for"), "{derived}");
            assert!(!manual.contains("FromValue for") && !manual.contains("IntoValue for"), "{manual}");
        }
    }
}
