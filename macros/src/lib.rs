//! Derive macros that bind Rust types to Sand types.
//!
//! Each type requires a `#[sand(module = "<module>")]` attribute and accepts
//! optional type attributes:
//!
//! - `#[sand(name = "<Name>")]` overrides the Sand type name.
//! - `#[sand(private)]` makes the type private.
//! - `#[sand(crate = <path>)]` overrides the `::sandlang` crate path.
//! - `#[sand(manual_conversion)]` omits the `FromVmValue` and `IntoVmValue` implementations. If the
//!   VM form of the type differs from its Rust shape, also add `no_value` or implement `Encode` by
//!   hand, so that `encode` returns the copy that `Vm::to_value` returns.
//! - `#[sand(no_value)]` states that the type has no `Value` form, and omits the `Encode`
//!   implementation. Use it for a type with a field that has no `Value` form, such as a `VmVal`
//!   handle.
//!
//! The macros copy Rust doc comments into Sand documentation.

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

/// Binds a Rust enum to a Sand enum.
///
/// Implements `NamedType`, `EnumType`, `SandType`, `FromVmValue`, `IntoVmValue`,
/// and `Encode`. Supports unit, tuple, and struct variants, as well as type
/// parameters. Each payload type must implement `IntoVmValue`, owned
/// `FromVmValue`, and `Encode`.
///
/// Optional attributes:
///
/// - `#[sand(name = "<name>")]` on a struct variant field overrides its Sand field name.
#[proc_macro_derive(SandEnum, attributes(sand))]
pub fn derive_sand_enum(input: TokenStream) -> TokenStream {
    expand(input, sand_enum)
}

/// Binds a Rust struct to a Sand struct.
///
/// Implements `NamedType`, `StructType`, `SandType`, `FromVmValue`, `IntoVmValue`,
/// and `Encode`. Each field type must implement `IntoVmValue`, owned
/// `FromVmValue`, and `Encode`. Rust type parameters define Sand type parameters.
///
/// Optional field attributes:
///
/// - `#[sand(name = "<name>")]`: overrides the Sand field name.
/// - `#[sand(private)]`: makes the field private.
/// - `#[sand(var)]`: makes the field mutable.
/// - `#[sand(default)]` or `#[sand(default = <expr>)]`: sets a default value from the `Default` trait or `Into::into(<expr>)`.
///   The field default is the `Encode` form of the value. It must be a Sand constant of the field
///   type, so it cannot hold a native value type. `HostModules::new` reports a default that is not
///   such a constant as a compile error.
#[proc_macro_derive(SandStruct, attributes(sand))]
pub fn derive_sand_struct(input: TokenStream) -> TokenStream {
    expand(input, sand_struct)
}

/// Binds a Rust type to an opaque Sand reference type.
///
/// Implements `NamedType`, `ObjectType`, `SandType`, `FromVmValue`, and `IntoVmValue`.
/// The type must implement `NativeObject`.
#[proc_macro_derive(SandObject, attributes(sand))]
pub fn derive_sand_object(input: TokenStream) -> TokenStream {
    expand(input, sand_object)
}

/// Binds a Rust type to an opaque Sand value type.
///
/// Implements `NamedType`, `SandType`, `FromVmValue`, `IntoVmValue`, and
/// `Encode`. The type must implement `ValueType`.
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
    no_value: bool,
}

fn parse_type_attrs(input: &DeriveInput, derive: &str) -> syn::Result<TypeAttrs> {
    let ty = &input.ident;
    let mut module = None;
    let mut name = LitStr::new(&ty.unraw().to_string(), ty.span());
    let mut public = true;
    let mut manual_conversion = false;
    let mut no_value = false;
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
            } else if meta.path.is_ident("no_value") && matches!(derive, "SandEnum" | "SandStruct") {
                no_value = true;
            } else if matches!(derive, "SandEnum" | "SandStruct") {
                return Err(meta.error("expected `module`, `name`, `private`, `crate`, `manual_conversion` or `no_value`"));
            } else {
                return Err(meta.error("expected `module`, `name`, `private` or `crate`"));
            }
            Ok(())
        })?;
    }
    let Some(module) = module else {
        return Err(Error::new(Span::call_site(), format!("`{derive}` requires `#[sand(module = \"...\")]`")));
    };
    Ok(TypeAttrs { module, name, public, krate, doc: doc_expr(&input.attrs), manual_conversion, no_value })
}

struct TypeParams {
    ty: Ident,
    params: Vec<Ident>,
}

impl TypeParams {
    fn empty(input: &DeriveInput, derive: &str) -> syn::Result<Self> {
        match input.generics.params.is_empty() {
            true => Ok(Self { ty: input.ident.clone(), params: Vec::new() }),
            false => Err(Error::new_spanned(
                &input.generics,
                format!("`{derive}` cannot be derived for a type with type or lifetime parameters"),
            )),
        }
    }

    fn parse(input: &DeriveInput, derive: &str) -> syn::Result<Self> {
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
        Ok(Self { ty: input.ident.clone(), params })
    }

    /// Replaces `Self` and each type parameter in `ty` with `Param<N>`.
    fn replace_self(&self, krate: &Path, ty: &Type) -> Type {
        self.substitute(ty, &|i| parse_quote!(#krate::native::ty::Param<#i>))
    }

    /// Replaces each type parameter in `ty`, also in `Self`, with the
    /// uninhabited `Never`. A value of the result holds no value of a type
    /// parameter.
    fn replace_params_with_never(&self, krate: &Path, ty: &Type) -> Type {
        self.substitute(ty, &|_| parse_quote!(#krate::native::ty::Never))
    }

    /// Replaces type parameter `N` in `ty` with `param(N)`, and `Self` with the
    /// type whose arguments are `param(0)`, `param(1)`, and so on.
    fn substitute(&self, ty: &Type, param: &dyn Fn(u8) -> Type) -> Type {
        struct Subst<'a>(&'a TypeParams, &'a dyn Fn(u8) -> Type);
        impl VisitMut for Subst<'_> {
            fn visit_type_mut(&mut self, t: &mut Type) {
                if let Type::Path(p) = t
                    && p.qself.is_none()
                    && let Some(id) = p.path.get_ident()
                {
                    let (ty, params) = (&self.0.ty, &self.0.params);
                    if id == "Self" {
                        let args = (0..params.len()).map(|i| (self.1)(i as u8));
                        *t = if params.is_empty() { parse_quote!(#ty) } else { parse_quote!(#ty<#(#args),*>) };
                        return;
                    }
                    if let Some(i) = params.iter().position(|x| x == id) {
                        *t = (self.1)(i as u8);
                        return;
                    }
                }
                syn::visit_mut::visit_type_mut(self, t);
            }
        }
        let mut t = ty.clone();
        Subst(self, param).visit_type_mut(&mut t);
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
    let (imp, tyg, wh) = type_params.split_for_impl(input, |_| quote!(#krate::native::ty::SandType));
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
    let (imp, tyg, wh) = type_params.split_for_impl(input, |_| quote!(#krate::native::ty::SandType));
    let args = &type_params.params;
    quote! {
        impl #imp #krate::native::ty::SandType for #ty #tyg #wh {
            fn sand_type() -> #krate::native::ty::Type {
                #krate::native::ty::Type::bound::<Self>(::std::vec![#(<#args as #krate::native::ty::SandType>::sand_type()),*])
            }
        }
    }
}

/// The generics of the reflection and conversion impls of a derived type.
struct ConversionGenerics {
    /// The parameters of each impl.
    params: Tokens,
    /// The type arguments of the derived type.
    args: Tokens,
    /// The where clause of the reflection impls.
    reflection_where: Tokens,
    from_vm_value_where: Tokens,
    into_vm_value_where: Tokens,
    encode_where: Tokens,
}

impl ConversionGenerics {
    fn new(input: &DeriveInput, krate: &Path, type_params: &TypeParams) -> ConversionGenerics {
        let (params, args, reflection_where) = type_params.split_for_impl(input, |_| quote!(#krate::native::ty::SandType));
        let where_clause = |bound: &dyn Fn(&Ident) -> Tokens| type_params.split_for_impl(input, bound).2;
        ConversionGenerics {
            params,
            args,
            reflection_where,
            from_vm_value_where: where_clause(&|p| quote!(for<'a> #krate::native::convert::FromVmValue<Item<'a> = #p>)),
            into_vm_value_where: where_clause(&|_| quote!(#krate::native::convert::IntoVmValue)),
            encode_where: where_clause(&|_| quote!(#krate::native::ty::SandType + #krate::value::Encode)),
        }
    }
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
    let type_params = TypeParams::parse(input, "SandEnum")?;
    reject_member_attrs(&input.data)?;
    if data.variants.is_empty() {
        return Err(Error::new_spanned(ty, "a Sand enum must have at least one variant"));
    }
    let krate = &attrs.krate;
    let mut variants = Vec::new();
    let mut indexes = Vec::new();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut encodes = Vec::new();
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
            let canonical = type_params.replace_self(krate, fty);
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
            infos.push(
                quote! { #krate::native::VariantField { name: #sname, ty: <#canonical as #krate::native::ty::SandType>::sand_type } },
            );
            let bind = format_ident!("f{k}");
            let get = quote! { #krate::native::field_from_vm_value::<#fty>(vm, #krate::native::variant_field(vm, v, #k)?, conv)? };
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
            puts.push(quote!(#krate::native::convert::IntoVmValue::into_vm_value(#bind, vm)?));
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
        writes.push(quote! { #pattern => #krate::native::variant_into_vm_value::<Self, _>(vm, #i, #write) });
        encodes.push(quote! { #pattern => #krate::native::encode_variant::<Self>(&SAND_VARIANT_DESCS, #i, [#(#krate::value::Encode::encode(#names)),*]) });
    }
    let named = expand_named_type(input, &attrs, &type_params, quote! { #krate::native::Binding::of_enum() });
    let sand_type = expand_sand_type(input, krate, &type_params);
    let ConversionGenerics { params, args, reflection_where, from_vm_value_where, into_vm_value_where, encode_where } =
        ConversionGenerics::new(input, krate, &type_params);
    let count = data.variants.len();
    let encode = (!attrs.no_value).then(|| {
        quote! {
            impl #params #krate::value::Encode for #ty #args #encode_where {
                fn encode(&self) -> #krate::value::Value {
                    static SAND_VARIANT_DESCS: [#krate::native::VariantDescCell; #count] =
                        [const { #krate::native::VariantDescCell::new() }; #count];
                    match self {
                        #(#encodes),*
                    }
                }
            }
        }
    });
    let conversion = (!attrs.manual_conversion).then(|| {
        quote! {
            impl #params #krate::native::convert::FromVmValue for #ty #args #from_vm_value_where {
                type Item<'a> = Self;
                #[allow(unused_variables)]
                fn from_vm_value(
                    vm: &#krate::vm::Vm,
                    v: #krate::vm::value::VmValue,
                    conv: &mut #krate::native::convert::Conversion,
                ) -> ::std::result::Result<Self, #krate::vm::Fail> {
                    #krate::native::enum_from_vm_value::<Self>(vm, v, conv, |index, conv| {
                        ::std::result::Result::Ok(match index {
                            #(#reads,)*
                            _ => return ::std::result::Result::Err(#krate::native::convert::mismatch::<Self>()),
                        })
                    })
                }
            }
            impl #params #krate::native::convert::IntoVmValue for #ty #args #into_vm_value_where {
                fn into_vm_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::vm::value::VmValue, #krate::vm::Fail> {
                    match self {
                        #(#writes),*
                    }
                }
            }
        }
    });
    Ok(quote! {
        #named
        #sand_type
        #(#checks)*
        impl #params #krate::native::EnumType for #ty #args #reflection_where {
            const VARIANTS: &'static [#krate::native::EnumVariant] = &[#(#variants),*];
            fn index(&self) -> usize {
                match self {
                    #(#indexes),*
                }
            }
        }
        #conversion
        #encode
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
        impl #krate::native::ty::SandType for &#ty {
            fn sand_type() -> #krate::native::ty::Type {
                <#ty as #krate::native::ty::SandType>::sand_type()
            }
        }
        impl<'r> #krate::native::convert::FromVmValue for &'r #ty {
            type Item<'a> = &'a #ty;
            fn from_vm_value<'a>(
                vm: &'a #krate::vm::Vm,
                v: #krate::vm::value::VmValue,
                _: &mut #krate::native::convert::Conversion,
            ) -> ::std::result::Result<&'a #ty, #krate::vm::Fail> {
                #krate::native::ty::VmVal::<#ty>::from_raw(v).get(&vm.heap)
            }
        }
        impl #krate::native::convert::IntoVmValue for #ty {
            fn into_vm_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::vm::value::VmValue, #krate::vm::Fail> {
                vm.new_object(self).map(#krate::native::ty::VmVal::raw)
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
    let type_params = TypeParams::parse(input, "SandStruct")?;
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
    let mut encodes = Vec::new();
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
        let canonical = type_params.replace_self(krate, fty);
        let default = match default {
            None => quote!(::std::option::Option::None),
            Some(value) => {
                let value_ty = type_params.replace_params_with_never(krate, fty);
                let value = value.map_or_else(|| quote!(<#value_ty as ::std::default::Default>::default()), |e| quote!(#e));
                quote_spanned! {fty.span()=>
                    ::std::option::Option::Some(|| {
                        let value: #value_ty = ::std::convert::Into::into(#value);
                        #krate::value::Encode::encode(&value)
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
                ty: <#canonical as #krate::native::ty::SandType>::sand_type,
                default: #default,
            }
        });
        reads.push(quote! { #ident: #krate::native::field_from_vm_value::<#fty>(vm, fields[#i], conv)? });
        writes.push(quote! { #krate::native::convert::IntoVmValue::into_vm_value(self.#ident, vm)? });
        encodes.push(quote! { #krate::value::Encode::encode(&self.#ident) });
    }
    let named = expand_named_type(input, &attrs, &type_params, quote! { #krate::native::Binding::of_struct() });
    let sand_type = expand_sand_type(input, krate, &type_params);
    let ConversionGenerics { params, args, reflection_where, from_vm_value_where, into_vm_value_where, encode_where } =
        ConversionGenerics::new(input, krate, &type_params);
    let encode = (!attrs.no_value).then(|| {
        quote! {
            impl #params #krate::value::Encode for #ty #args #encode_where {
                fn encode(&self) -> #krate::value::Value {
                    static SAND_STRUCT_DESC: #krate::native::StructDescCell = #krate::native::StructDescCell::new();
                    #krate::native::encode_struct::<Self>(&SAND_STRUCT_DESC, [#(#encodes),*])
                }
            }
        }
    });
    let conversion = (!attrs.manual_conversion).then(|| {
        quote! {
            impl #params #krate::native::convert::FromVmValue for #ty #args #from_vm_value_where {
                type Item<'a> = Self;
                #[allow(unused_variables)]
                fn from_vm_value(
                    vm: &#krate::vm::Vm,
                    v: #krate::vm::value::VmValue,
                    conv: &mut #krate::native::convert::Conversion,
                ) -> ::std::result::Result<Self, #krate::vm::Fail> {
                    #krate::native::struct_from_vm_value(vm, v, conv, |fields, conv| ::std::result::Result::Ok(#ty { #(#reads),* }))
                }
            }
            impl #params #krate::native::convert::IntoVmValue for #ty #args #into_vm_value_where {
                fn into_vm_value(self, vm: &mut #krate::vm::Vm) -> ::std::result::Result<#krate::vm::value::VmValue, #krate::vm::Fail> {
                    #krate::native::struct_into_vm_value::<Self>(vm, move |vm| ::std::result::Result::Ok(::std::boxed::Box::new([#(#writes),*])))
                }
            }
        }
    });
    Ok(quote! {
        #named
        #sand_type
        #(#checks)*
        impl #params #krate::native::StructType for #ty #args #reflection_where {
            const FIELDS: &'static [#krate::native::StructField] = &[#(#infos),*];
        }
        #conversion
        #encode
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
        assert!(
            out.contains("let value : i64 = :: std :: convert :: Into :: into (2) ; :: sandlang :: value :: Encode :: encode (& value)"),
            "{out}"
        );
        assert_eq!(out.matches("default : :: std :: option :: Option :: None").count(), 1, "{out}");
        let out = expand(
            sand_struct,
            "#[sand(module = \"m\")] struct G<T> { #[sand(default)] xs: Vec<T>, #[sand(default)] next: Option<Box<Self>> }",
        );
        assert!(out.contains("let value : Vec < :: sandlang :: native :: ty :: Never > = :: std :: convert :: Into :: into"), "{out}");
        assert!(out.contains("let value : Option < Box < G < :: sandlang :: native :: ty :: Never > > > ="), "{out}");
    }

    #[test]
    fn generic_types_define_sand_type_parameters() {
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct Pair<A, B> { a: A, b: Vec<B>, c: Option<(A, B)> }");
        assert!(out.contains("const PARAMS : & 'static [& 'static str] = & [\"A\" , \"B\"]"), "{out}");
        assert!(out.contains("enum Instance { } :: std :: any :: TypeId :: of :: < Instance > ()"), "{out}");
        assert!(
            out.contains("ty : < :: sandlang :: native :: ty :: Param < 0u8 > as :: sandlang :: native :: ty :: SandType > :: sand_type"),
            "{out}"
        );
        assert!(out.contains("ty : < Vec < :: sandlang :: native :: ty :: Param < 1u8 > > as"), "{out}");
        assert!(
            out.contains("Option < (:: sandlang :: native :: ty :: Param < 0u8 > , :: sandlang :: native :: ty :: Param < 1u8 >) >"),
            "{out}"
        );
        assert!(out.contains("A : 'static + for < 'a > :: sandlang :: native :: convert :: FromVmValue < Item < 'a > = A >"), "{out}");
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
        assert!(out.contains("E :: C { w : f0 } => :: sandlang :: native :: variant_into_vm_value :: < Self , _ > (vm , 2usize"), "{out}");
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
                "expected `module`, `name`, `private`, `crate`, `manual_conversion` or `no_value`",
            ),
            (sand_object, "#[sand(module = \"m\", manual_conversion)] struct O;", "expected `module`, `name`, `private` or `crate`"),
        ];
        for (f, src, msg) in cases {
            let out = expand(f, src);
            assert!(out.starts_with("error: ") && out.contains(msg), "{src}: {out}");
        }
    }

    #[test]
    fn manual_conversion_omits_only_the_vm_conversion_impls() {
        let cases: [(Derive, &str, &str); 2] =
            [(sand_enum, "enum E { A(i64) }", "EnumType for E"), (sand_struct, "struct S { x: i64 }", "StructType for S")];
        for (f, item, reflection) in cases {
            let derived = expand(f, &format!("#[sand(module = \"m\")] {item}"));
            let manual = expand(f, &format!("#[sand(module = \"m\", manual_conversion)] {item}"));
            let bare = expand(f, &format!("#[sand(module = \"m\", manual_conversion, no_value)] {item}"));
            for out in [&derived, &manual, &bare] {
                assert!(out.contains("NamedType for") && out.contains("SandType for") && out.contains(reflection), "{out}");
            }
            assert!(
                derived.contains("FromVmValue for") && derived.contains("IntoVmValue for") && derived.contains("Encode for"),
                "{derived}"
            );
            assert!(
                !manual.contains("FromVmValue for") && !manual.contains("IntoVmValue for") && manual.contains("Encode for"),
                "{manual}"
            );
            assert!(!bare.contains("FromVmValue for") && !bare.contains("IntoVmValue for") && !bare.contains("Encode for"), "{bare}");
        }
    }

    #[test]
    fn no_value_omits_only_the_encode_impl() {
        for (f, item) in [(sand_enum as Derive, "enum E { A(i64) }"), (sand_struct, "struct S { x: i64 }")] {
            let out = expand(f, &format!("#[sand(module = \"m\", no_value)] {item}"));
            assert!(out.contains("FromVmValue for") && out.contains("IntoVmValue for") && !out.contains("Encode for"), "{out}");
        }
    }

    #[test]
    fn encode_names_each_field_and_variant_payload() {
        let out = expand(sand_struct, "#[sand(module = \"m\")] struct S<T> { x: i64, y: T }");
        assert!(out.contains(":: sandlang :: native :: encode_struct :: < Self > (& SAND_STRUCT_DESC , [:: sandlang :: value :: Encode :: encode (& self . x) , :: sandlang :: value :: Encode :: encode (& self . y)])"), "{out}");
        assert!(out.contains("static SAND_STRUCT_DESC : :: sandlang :: native :: StructDescCell"), "{out}");
        assert!(out.contains("T : 'static + :: sandlang :: native :: ty :: SandType + :: sandlang :: value :: Encode"), "{out}");
        let out = expand(sand_enum, "#[sand(module = \"m\")] enum E { A, B(i64), C { w: f64 } }");
        assert!(out.contains("static SAND_VARIANT_DESCS : [:: sandlang :: native :: VariantDescCell ; 3usize]"), "{out}");
        assert!(
            out.contains("E :: A => :: sandlang :: native :: encode_variant :: < Self > (& SAND_VARIANT_DESCS , 0usize , [])"),
            "{out}"
        );
        assert!(out.contains("E :: C { w : f0 } => :: sandlang :: native :: encode_variant :: < Self > (& SAND_VARIANT_DESCS , 2usize , [:: sandlang :: value :: Encode :: encode (f0)])"), "{out}");
    }
}
