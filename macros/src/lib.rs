//! Derive macros for `sandlang`, re-exported at the root of the `sandlang` crate.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::ext::IdentExt;
use syn::{Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, Lit, LitStr, Meta, parse_macro_input};

/// Derives `EnumType`, `SandType`, `FromValue`, and `IntoValue` for a unit
/// enum so that native functions can accept and return it.
///
/// Required attribute:
/// - `#[sand(module = "<module>")]` sets the Sand module that owns the enum.
///
/// Optional attribute:
/// - `#[sand(name = "<Name>")]` overrides the Sand enum name.
#[proc_macro_derive(SandEnum, attributes(sand))]
pub fn derive_sand_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    sand_enum(&input).unwrap_or_else(Error::into_compile_error).into()
}

/// Derives `ObjectType`, `SandType`, `FromValue` for `&T`, and `IntoValue`
/// for a struct or enum so that native functions can take objects of it
/// by reference and return new ones. The type must also implement
/// `NativeObject`.
///
/// Required attribute:
/// - `#[sand(module = "<module>")]` sets the Sand module that owns the type.
///
/// Optional attributes:
/// - `#[sand(name = "<Name>")]` overrides the Sand type name.
/// - `#[sand(private)]` hides the type from other modules.
#[proc_macro_derive(SandObject, attributes(sand))]
pub fn derive_sand_object(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    sand_object(&input).unwrap_or_else(Error::into_compile_error).into()
}

struct Attrs {
    module: LitStr,
    name: LitStr,
    private: bool,
}

fn attrs(input: &DeriveInput, derive: &str, allow_private: bool) -> syn::Result<Attrs> {
    let ty = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(&input.generics, format!("`{derive}` cannot be derived for a type with type parameters")));
    }
    let (mut module, mut name, mut private) = (None, LitStr::new(&ty.unraw().to_string(), ty.span()), false);
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("sand")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("module") {
                module = Some(meta.value()?.parse::<LitStr>()?);
            } else if meta.path.is_ident("name") {
                name = meta.value()?.parse()?;
            } else if meta.path.is_ident("private") && allow_private {
                private = true;
            } else if allow_private {
                return Err(meta.error("expected `module`, `name` or `private`"));
            } else {
                return Err(meta.error("expected `module` or `name`"));
            }
            Ok(())
        })?;
    }
    let Some(module) = module else {
        return Err(Error::new(Span::call_site(), format!("`{derive}` requires `#[sand(module = \"...\")]`")));
    };
    let n = name.value();
    let capitalized = n.starts_with(|c: char| c.is_ascii_uppercase()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !capitalized {
        return Err(Error::new_spanned(&name, format!("the Sand type name `{n}` is not a capitalized identifier")));
    }
    for attrs in members(&input.data) {
        if let Some(a) = attrs.iter().find(|a| a.path().is_ident("sand")) {
            return Err(Error::new_spanned(a, "`#[sand(...)]` applies only to the type"));
        }
    }
    Ok(Attrs { module, name, private })
}

fn sand_enum(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ty = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(ty, "`SandEnum` can only be derived for enums"));
    };
    let Attrs { module, name, .. } = attrs(input, "SandEnum", false)?;
    if data.variants.is_empty() {
        return Err(Error::new_spanned(ty, "a Sand enum must have at least one variant"));
    }
    let enum_doc = extract_doc_comment(&input.attrs);
    let mut variants = Vec::new();
    let mut arms = Vec::new();
    for (i, v) in data.variants.iter().enumerate() {
        if !matches!(v.fields, Fields::Unit) {
            return Err(Error::new_spanned(&v.fields, "`SandEnum` variants cannot have fields"));
        }
        let (ident, vname, vdoc) = (&v.ident, v.ident.unraw().to_string(), extract_doc_comment(&v.attrs));
        variants.push(quote! { ::sandlang::native::EnumVariant { name: #vname, doc: #vdoc, value: #ty::#ident } });
        arms.push(quote! { #ty::#ident => #i });
    }
    Ok(quote! {
        impl ::sandlang::native::EnumType for #ty {
            const MODULE: &'static str = #module;
            const NAME: &'static str = #name;
            const DOC: &'static str = #enum_doc;
            const VARIANTS: &'static [::sandlang::native::EnumVariant<Self>] = &[#(#variants),*];
            fn index(self) -> usize {
                match self {
                    #(#arms),*
                }
            }
        }
        impl ::sandlang::native::SandType for #ty {
            fn sand_type() -> ::sandlang::native::Type {
                ::sandlang::native::Type::bound::<Self>(#module, #name)
            }
        }
        impl ::sandlang::native::FromValue for #ty {
            type Item<'a> = Self;
            fn from_value(
                vm: &::sandlang::vm::Vm,
                v: ::sandlang::Value,
                _: &mut ::sandlang::native::Conversion,
            ) -> ::std::result::Result<Self, ::sandlang::vm::Ctl> {
                vm.enum_value(v)
            }
        }
        impl ::sandlang::native::IntoValue for #ty {
            fn into_value(self, vm: &mut ::sandlang::vm::Vm) -> ::std::result::Result<::sandlang::Value, ::sandlang::vm::Ctl> {
                vm.new_enum(self)
            }
        }
    })
}

fn sand_object(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ty = &input.ident;
    if matches!(input.data, Data::Union(_)) {
        return Err(Error::new_spanned(ty, "`SandObject` cannot be derived for unions"));
    }
    let Attrs { module, name, private } = attrs(input, "SandObject", true)?;
    let doc = extract_doc_comment(&input.attrs);
    let public = !private;
    Ok(quote! {
        impl ::sandlang::native::ObjectType for #ty {
            const MODULE: &'static str = #module;
            const NAME: &'static str = #name;
            const DOC: &'static str = #doc;
            const PUBLIC: bool = #public;
        }
        impl ::sandlang::native::SandType for #ty {
            fn sand_type() -> ::sandlang::native::Type {
                ::sandlang::native::Type::bound::<Self>(#module, #name)
            }
        }
        impl ::sandlang::native::SandType for &#ty {
            fn sand_type() -> ::sandlang::native::Type {
                <#ty as ::sandlang::native::SandType>::sand_type()
            }
        }
        impl<'r> ::sandlang::native::FromValue for &'r #ty {
            type Item<'a> = &'a #ty;
            fn from_value<'a>(
                vm: &'a ::sandlang::vm::Vm,
                v: ::sandlang::Value,
                _: &mut ::sandlang::native::Conversion,
            ) -> ::std::result::Result<&'a #ty, ::sandlang::vm::Ctl> {
                ::sandlang::native::Val::<#ty>::from_raw(v).get(&vm.heap)
            }
        }
        impl ::sandlang::native::IntoValue for #ty {
            fn into_value(self, vm: &mut ::sandlang::vm::Vm) -> ::std::result::Result<::sandlang::Value, ::sandlang::vm::Ctl> {
                vm.new_object(self).map(::sandlang::native::Val::raw)
            }
        }
    })
}

/// The attributes of the variants and fields of a type.
fn members(data: &Data) -> Vec<&[Attribute]> {
    fn fields(f: &Fields) -> impl Iterator<Item = &[Attribute]> {
        f.iter().map(|f| &f.attrs[..])
    }
    match data {
        Data::Struct(s) => fields(&s.fields).collect(),
        Data::Enum(e) => e.variants.iter().flat_map(|v| std::iter::once(&v.attrs[..]).chain(fields(&v.fields))).collect(),
        Data::Union(u) => u.fields.named.iter().map(|f| &f.attrs[..]).collect(),
    }
}

fn extract_doc_comment(attrs: &[Attribute]) -> String {
    let lines = attrs.iter().filter_map(|a| match &a.meta {
        Meta::NameValue(nv) if nv.path.is_ident("doc") => match &nv.value {
            Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    });
    let lines: Vec<String> = lines.map(|l| l.strip_prefix(' ').map(str::to_string).unwrap_or(l)).collect();
    lines.join("\n").trim().to_string()
}
