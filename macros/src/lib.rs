//! Derive macros for `sandlang`, re-exported at the root of the `sandlang` crate.
//!
//! Each derive binds a Rust type to a Sand type of a native module, and takes
//! these attributes on the type:
//! - `#[sand(module = "<module>")]` (required) sets the Sand module that owns
//!   the type.
//! - `#[sand(name = "<Name>")]` overrides the Sand type name (the Rust
//!   identifier by default).
//! - `#[sand(private)]` makes the type private to its module.
//! - `#[sand(crate = <path>)]` sets the path of the `sandlang` crate, for a
//!   crate that renames the dependency (`::sandlang` by default).
//!
//! Doc comments become Sand documentation. `#[doc = include_str!("...")]`
//! attributes are supported.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::quote;
use syn::ext::IdentExt;
use syn::{Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, Lit, LitStr, Meta, Path, parse_macro_input, parse_quote};

/// Derives `NamedType`, `EnumType`, `SandType`, `FromValue`, and `IntoValue`
/// for a unit enum, so that native functions can accept and return it.
#[proc_macro_derive(SandEnum, attributes(sand))]
pub fn derive_sand_enum(input: TokenStream) -> TokenStream {
    expand(input, sand_enum)
}

/// Derives `NamedType`, `ObjectType`, `SandType`, `FromValue` (for `&T`), and
/// `IntoValue` for a struct or enum, so that native functions can borrow object
/// handles as `&T` and return new objects by value. The type must also
/// implement `NativeObject`.
#[proc_macro_derive(SandObject, attributes(sand))]
pub fn derive_sand_object(input: TokenStream) -> TokenStream {
    expand(input, sand_object)
}

/// Derives `NamedType`, `StructType`, `SandType`, `FromValue`, and `IntoValue`
/// for a struct with named fields, so that native functions can accept and
/// return it as a Sand struct with the same fields. Each field type must
/// implement `FromValue` (with an owned `Item`) and `IntoValue`.
///
/// Field attributes:
/// - `#[sand(name = "<name>")]` overrides the Sand field name (the Rust
///   identifier by default), for a Rust name that is a Sand keyword.
/// - `#[sand(private)]` makes the field private to the module.
/// - `#[sand(var)]` makes the field mutable (`var`) in Sand.
#[proc_macro_derive(SandStruct, attributes(sand))]
pub fn derive_sand_struct(input: TokenStream) -> TokenStream {
    expand(input, sand_struct)
}

/// Derives `NamedType`, `SandType`, `FromValue`, and `IntoValue` for a type
/// that implements `ValueType` by hand.
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
}

fn parse_type_attrs(input: &DeriveInput, derive: &str) -> syn::Result<TypeAttrs> {
    let ty = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &input.generics,
            format!("`{derive}` cannot be derived for a type with type or lifetime parameters"),
        ));
    }
    let mut module = None;
    let mut name = LitStr::new(&ty.unraw().to_string(), ty.span());
    let mut public = true;
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
            } else {
                return Err(meta.error("expected `module`, `name`, `private` or `crate`"));
            }
            Ok(())
        })?;
    }
    let Some(module) = module else {
        return Err(Error::new(Span::call_site(), format!("`{derive}` requires `#[sand(module = \"...\")]`")));
    };
    let type_name = name.value();
    let capitalized =
        type_name.starts_with(|c: char| c.is_ascii_uppercase()) && type_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !capitalized {
        return Err(Error::new_spanned(&name, format!("the Sand type name `{type_name}` is not a capitalized identifier")));
    }
    Ok(TypeAttrs { module, name, public, krate, doc: doc_expr(&input.attrs) })
}

/// Rejects `#[sand(...)]` on the variants and fields of a type whose derive
/// takes no member attributes.
fn no_member_attrs(data: &Data) -> syn::Result<()> {
    fn fields(f: &Fields) -> impl Iterator<Item = &Attribute> {
        f.iter().flat_map(|f| &f.attrs)
    }
    let attrs: Vec<&Attribute> = match data {
        Data::Struct(s) => fields(&s.fields).collect(),
        Data::Enum(e) => e.variants.iter().flat_map(|v| v.attrs.iter().chain(fields(&v.fields))).collect(),
        Data::Union(u) => u.fields.named.iter().flat_map(|f| &f.attrs).collect(),
    };
    match attrs.into_iter().find(|a| a.path().is_ident("sand")) {
        Some(a) => Err(Error::new_spanned(a, "`#[sand(...)]` applies only to the type")),
        None => Ok(()),
    }
}

/// The `NamedType` implementation and the `SandType` implementation of a derived type.
fn named_type(ty: &syn::Ident, a: &TypeAttrs, binding: Tokens) -> Tokens {
    let TypeAttrs { module, name, public, krate, doc } = a;
    quote! {
        impl #krate::native::NamedType for #ty {
            const MODULE: &'static str = #module;
            const NAME: &'static str = #name;
            const DOC: &'static str = #doc;
            const PUBLIC: bool = #public;
            fn binding() -> #krate::native::Binding {
                #binding
            }
        }
    }
}

fn sand_type(ty: &syn::Ident, krate: &Path) -> Tokens {
    quote! {
        impl #krate::native::SandType for #ty {
            fn sand_type() -> #krate::native::Type {
                #krate::native::Type::bound::<Self>(
                    <Self as #krate::native::NamedType>::MODULE,
                    <Self as #krate::native::NamedType>::NAME,
                )
            }
        }
    }
}

fn sand_enum(input: &DeriveInput) -> syn::Result<Tokens> {
    let ty = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(ty, "`SandEnum` can only be derived for enums"));
    };
    let attrs = parse_type_attrs(input, "SandEnum")?;
    no_member_attrs(&input.data)?;
    if data.variants.is_empty() {
        return Err(Error::new_spanned(ty, "a Sand enum must have at least one variant"));
    }
    let krate = &attrs.krate;
    let mut variants = Vec::new();
    let mut arms = Vec::new();
    for (i, v) in data.variants.iter().enumerate() {
        if !matches!(v.fields, Fields::Unit) {
            return Err(Error::new_spanned(&v.fields, "`SandEnum` variants cannot have fields"));
        }
        let (ident, vname, vdoc) = (&v.ident, v.ident.unraw().to_string(), doc_expr(&v.attrs));
        variants.push(quote! { #krate::native::EnumVariant { name: #vname, doc: #vdoc, value: #ty::#ident } });
        arms.push(quote! { #ty::#ident => #i });
    }
    let named = named_type(ty, &attrs, quote! { #krate::native::Binding::of_enum::<Self>() });
    let sand_type = sand_type(ty, krate);
    Ok(quote! {
        #named
        #sand_type
        impl #krate::native::EnumType for #ty {
            const VARIANTS: &'static [#krate::native::EnumVariant<Self>] = &[#(#variants),*];
            fn index(self) -> usize {
                match self {
                    #(#arms),*
                }
            }
        }
        impl #krate::native::FromValue for #ty {
            type Item<'a> = Self;
            fn from_value(
                vm: &#krate::vm::Vm,
                v: #krate::Value,
                _: &mut #krate::native::Conversion,
            ) -> ::std::result::Result<Self, #krate::vm::Ctl> {
                vm.enum_value(v)
            }
        }
        impl #krate::native::IntoValue for #ty {
            fn into_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::Value, #krate::vm::Ctl> {
                vm.new_enum(self)
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
    no_member_attrs(&input.data)?;
    let krate = &attrs.krate;
    let named = named_type(ty, &attrs, quote! { #krate::native::Binding::of_object::<Self>() });
    let sand_type = sand_type(ty, krate);
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
    let krate = &attrs.krate;
    let fields = match &data.fields {
        Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
        Fields::Unit => Vec::new(),
        Fields::Unnamed(f) => return Err(Error::new_spanned(f, "`SandStruct` requires named fields")),
    };
    let mut infos = Vec::new();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let ident = f.ident.as_ref().expect("named field");
        let (mut public, mut var, mut name) = (true, false, ident.unraw().to_string());
        for attr in f.attrs.iter().filter(|a| a.path().is_ident("sand")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    name = meta.value()?.parse::<LitStr>()?.value();
                } else if meta.path.is_ident("private") {
                    public = false;
                } else if meta.path.is_ident("var") {
                    var = true;
                } else {
                    return Err(meta.error("expected `name`, `private` or `var`"));
                }
                Ok(())
            })?;
        }
        let (fty, doc) = (&f.ty, doc_expr(&f.attrs));
        infos.push(quote! {
            #krate::native::StructField {
                name: #name,
                doc: #doc,
                public: #public,
                var: #var,
                ty: <#fty as #krate::native::SandType>::sand_type,
            }
        });
        reads.push(quote! { #ident: #krate::native::field_value::<#fty>(vm, fields[#i], conv)? });
        writes.push(quote! { #krate::native::IntoValue::into_value(self.#ident, vm)? });
    }
    let named = named_type(ty, &attrs, quote! { #krate::native::Binding::of_struct::<Self>() });
    let sand_type = sand_type(ty, krate);
    Ok(quote! {
        #named
        #sand_type
        impl #krate::native::StructType for #ty {
            const FIELDS: &'static [#krate::native::StructField] = &[#(#infos),*];
        }
        impl #krate::native::FromValue for #ty {
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
        impl #krate::native::IntoValue for #ty {
            fn into_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::Value, #krate::vm::Ctl> {
                vm.nested(|vm| {
                    let fields: ::std::boxed::Box<[#krate::Value]> = ::std::boxed::Box::new([#(#writes),*]);
                    vm.new_struct::<Self>(fields)
                })
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
    no_member_attrs(&input.data)?;
    let krate = &attrs.krate;
    let named = named_type(ty, &attrs, quote! { #krate::native::Binding::of_value::<Self>() });
    Ok(quote! {
        #named
        #krate::__value_type_impls!(#ty);
    })
}

/// A `&'static str` expression for the doc comment in `attrs`: the lines of
/// `///` comments without their first space, joined by newlines and trimmed.
/// A `#[doc = ...]` value that is not a string literal (such as
/// `include_str!("...")`) is included through `concat!`.
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
        let derives: [(Derive, &str); 4] =
            [(sand_enum, "enum E { A }"), (sand_object, "struct O;"), (sand_struct, "struct S { x: i64 }"), (sand_value, "struct V(i64);")];
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
        assert_eq!(out, "error: expected `name`, `private` or `var`");
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S { #[sand(name = \"kind\")] r#type: i64 }");
        assert!(out.contains("name : \"kind\""), "{out}");
    }

    #[test]
    fn invalid_derives_are_rejected() {
        let cases: [(Derive, &str, &str); 7] = [
            (sand_struct, "#[sand(module = \"m\")] struct S(i64);", "`SandStruct` requires named fields"),
            (sand_struct, "#[sand(module = \"m\")] enum S { A }", "`SandStruct` can only be derived for structs"),
            (sand_struct, "#[sand(module = \"m\")] struct S<'a> { x: &'a str }", "type or lifetime parameters"),
            (sand_struct, "struct S { x: i64 }", "requires `#[sand(module = \"...\")]`"),
            (sand_value, "#[sand(module = \"m\", name = \"lower\")] struct V(i64);", "is not a capitalized identifier"),
            (sand_value, "#[sand(module = \"m\")] struct V { #[sand(private)] x: i64 }", "applies only to the type"),
            (sand_enum, "#[sand(module = \"m\", public)] enum E { A }", "expected `module`, `name`, `private` or `crate`"),
        ];
        for (f, src, msg) in cases {
            let out = expand(f, src);
            assert!(out.starts_with("error: ") && out.contains(msg), "{src}: {out}");
        }
    }
}
