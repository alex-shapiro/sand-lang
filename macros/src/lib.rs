//! Derive macros for `sandlang`, re-exported at the root of the `sandlang` crate.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Error, Expr, ExprLit, Fields, Lit, LitStr, Meta, parse_macro_input};

/// Derives `EnumType`, `SandType`, `FromValue`, `IntoValue`, and `ListElem` for
/// a unit-only Rust enum so that host functions can accept and return it.
///
/// Required attribute:
/// - `#[sand(module = "<module>")]` sets the Sand module that owns the enum.
///
/// Optional attribute:
/// - `#[sand(name = "<Name>")]` overrides the Sand enum name (defaults to the
///   Rust identifier).
#[proc_macro_derive(SandEnum, attributes(sand))]
pub fn derive_sand_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    sand_enum(&input).unwrap_or_else(Error::into_compile_error).into()
}

fn sand_enum(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ty = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(ty, "`SandEnum` can only be derived for enums"));
    };
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(&input.generics, "a Sand enum cannot have type parameters"));
    }
    if data.variants.is_empty() {
        return Err(Error::new_spanned(ty, "a Sand enum must have at least one variant"));
    }
    let (mut module, mut name) = (None, LitStr::new(&ty.to_string(), ty.span()));
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("sand")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("module") {
                module = Some(meta.value()?.parse::<LitStr>()?);
            } else if meta.path.is_ident("name") {
                name = meta.value()?.parse()?;
            } else {
                return Err(meta.error("expected `module` or `name`"));
            }
            Ok(())
        })?;
    }
    let Some(module) = module else {
        return Err(Error::new(Span::call_site(), "`SandEnum` requires `#[sand(module = \"...\")]`"));
    };
    let enum_doc = extract_doc_comment(&input.attrs);
    let mut variants = Vec::new();
    let mut arms = Vec::new();
    for (i, v) in data.variants.iter().enumerate() {
        if !matches!(v.fields, Fields::Unit) {
            return Err(Error::new_spanned(&v.fields, "`SandEnum` variants cannot have fields"));
        }
        let (ident, vname, vdoc) = (&v.ident, v.ident.to_string(), extract_doc_comment(&v.attrs));
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
            fn sand_type(cx: &mut ::sandlang::native::TypeContext) -> ::std::string::String {
                cx.enum_type::<Self>()
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
        impl ::sandlang::native::ListElem for #ty {}
    })
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
