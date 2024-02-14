use anyhow::Result;
use convert_case::{Case, Casing};
use openapiv3::{ArrayType, OpenAPI, Schema, SchemaKind};
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{quote, TokenStreamExt};
use regex::{Captures, Regex};
use syn::Path;

use mir::{ArgIdent, Class, Field, File, Function, Ident, Import, ImportItem, Literal, Visibility};
pub use typ::*;
pub use example::*;
pub use ident::*;
use ln_core::extractor::is_primitive;
use hir::{HirSpec, NewType, Parameter, ParamKey, Record, StrEnum, Struct, Ty, Doc, HirField};
use crate::rust::format;

mod example;
mod ident;
mod typ;

pub trait ToRustIdent {
    fn to_rust_struct(&self) -> Ident;
    fn to_rust_ident(&self) -> Ident;
}

impl ToRustIdent for String {
    fn to_rust_struct(&self) -> Ident {
        sanitize_struct(self)
    }

    fn to_rust_ident(&self) -> Ident {
        sanitize_ident(self)
    }
}

impl ToRustIdent for &str {
    fn to_rust_struct(&self) -> Ident {
        sanitize_struct(self)
    }

    fn to_rust_ident(&self) -> Ident {
        sanitize_ident(self)
    }
}

pub fn sanitize_filename(s: &str) -> String {
    sanitize(s)
}

pub fn sanitize_ident(s: &str) -> Ident {
    Ident(sanitize(s))
}

/// Use this for codegen structs: Function, Class, etc.
pub trait ToRustCode {
    fn to_rust_code(self) -> TokenStream;
}

impl ToRustCode for Function<TokenStream> {
    fn to_rust_code(self) -> TokenStream {
        let Function {
            name,
            args,
            body,
            doc,
            async_,
            annotations,
            ret,
            public,
            generic,
        } = self;
        let annotations = annotations
            .into_iter()
            .map(|a| syn::parse_str::<syn::Expr>(&a).unwrap());
        let doc = doc.to_rust_code();
        let vis = if public { quote!(pub) } else { quote!() };
        let async_ = if async_ { quote!(async) } else { quote!() };
        let args = args.into_iter().map(|a| {
            let name = a.name.unwrap_ident();
            let ty = &a.ty;
            quote! { #name: #ty }
        });
        let return_fragment = if ret.is_empty() {
            quote!()
        } else {
            quote!(-> #ret)
        };
        quote! {
            #(#[ #annotations ])*
            #doc
            #vis #async_ fn #name(#(#args),*) #return_fragment {
                #body
            }
        }
    }
}

fn pub_tok(public: bool) -> TokenStream {
    if public {
        quote!(pub)
    } else {
        quote!()
    }
}

impl ToRustCode for Visibility {
    fn to_rust_code(self) -> TokenStream {
        match self {
            Visibility::Public => quote!(pub),
            Visibility::Private => quote!(),
            Visibility::Crate => quote!(pub(crate)),
        }
    }
}

pub fn codegen_function(func: Function<TokenStream>, self_arg: TokenStream) -> TokenStream {
    let name = func.name;
    let args = func.args.into_iter().map(|a| {
        let name = a.name.unwrap_ident();
        let ty = a.ty;
        quote! { #name: #ty }
    });
    let ret = func.ret;
    let async_ = if func.async_ {
        quote! { async }
    } else {
        quote! {}
    };
    let public = pub_tok(func.public);
    let body = &func.body;
    quote! {
        #public #async_ fn #name(#self_arg #(#args),*) -> #ret {
            #body
        }
    }
}


impl ToRustCode for Class<TokenStream> {
    fn to_rust_code(self) -> TokenStream {
        let is_pub = pub_tok(self.public);
        let fields = self.instance_fields.iter().map(|f| {
            let name = &f.name.to_rust_ident();
            let ty = &f.ty;
            let public = f.visibility.to_rust_code();
            quote! { #public #name: #ty }
        });
        let instance_methods = self.instance_methods.into_iter().map(|m|
            codegen_function(m, quote! { self , })
        );
        let mut_self_instance_methods = self.mut_self_instance_methods.into_iter().map(|m| {
            codegen_function(m, quote! { mut self , })
        });
        let class_methods = self.class_methods.into_iter().map(|m| {
            codegen_function(m, TokenStream::new())
        });

        let doc = self.doc.to_rust_code();
        let lifetimes = if self.lifetimes.is_empty() {
            quote! {}
        } else {
            let lifetimes = self.lifetimes.iter().map(|l| {
                let name = syn::Lifetime::new(l, Span::call_site());
                quote! { # name }
            });
            quote! { < # ( # lifetimes), * > }
        };
        let decorator = self.decorators;
        let name = self.name;
        quote! {
            #doc
            #(
                #decorator
            )*
            #is_pub struct #name #lifetimes {
                #(#fields,)*
            }
            impl #lifetimes #name #lifetimes{
                #(#instance_methods)*
                #(#mut_self_instance_methods)*
                #(#class_methods)*
            }
        }
    }
}

impl ToRustCode for Field<TokenStream> {
    fn to_rust_code(self) -> TokenStream {
        let name = self.name.to_rust_ident();
        let ty = if self.optional {
            let ty = self.ty;
            quote! { Option<#ty> }
        } else {
            self.ty
        };
        let vis = self.visibility.to_rust_code();
        let doc = self.doc.to_rust_code();
        let decorators = self.decorators;
        quote! {
            #doc
            #(
                #decorators
            )*
            #vis #name: #ty,
        }
    }
}

impl ToRustCode for ImportItem {
    fn to_rust_code(self) -> TokenStream {
        if let Some(alias) = self.alias {
            let alias = syn::Ident::new(&alias, Span::call_site());
            let path = syn::parse_str::<syn::Path>(&self.name).unwrap();
            quote! { #path as #alias }
        } else if &self.name == "*" {
            quote! { * }
        } else {
            let path = syn::parse_str::<syn::Path>(&self.name).unwrap();
            quote! { #path }
        }
    }
}

impl ToRustCode for Import {
    fn to_rust_code(mut self) -> TokenStream {
        fn inner(import: Import) -> TokenStream {
            let Import {
                path,
                alias,
                imports,
                vis,
                feature,
            } = import;
            if path.ends_with('*') {
                let path = syn::parse_str::<Path>(&path[..path.len() - 3]).unwrap();
                return quote! { use #path::*; };
            }
            let path = syn::parse_str::<Path>(&path).expect(&format!("Failed to parse as syn::Path: {}", &path));
            let vis = vis.to_rust_code();
            if let Some(alias) = alias {
                let alias = syn::parse_str::<syn::Ident>(&alias).unwrap();
                quote!( #vis use #path as #alias; )
            } else if !imports.is_empty() {
                let imports = imports.into_iter().map(|i| i.to_rust_code());
                quote!( #vis use #path::{#(#imports),*}; )
            } else {
                quote! { #vis use #path; }
            }
        }
        let feature = std::mem::take(&mut self.feature).map(|f| {
            let f = syn::Ident::new(&f, Span::call_site());
            quote!(#[cfg(feature = #f)])
        }).unwrap_or_default();
        let import = inner(self);
        quote!(#feature #import)
    }
}

impl ToRustCode for File<TokenStream> {
    fn to_rust_code(self) -> TokenStream {
        let File {
            imports,
            classes,
            doc,
            functions,
            code,
            package,
            declaration,
        } = self;
        let imports = imports.into_iter().map(|i| i.to_rust_code());
        let doc = doc.to_rust_code();
        let functions = functions.into_iter().map(|f| f.to_rust_code());
        let classes = classes.into_iter().map(|c| c.to_rust_code());
        let code = code.unwrap_or_else(TokenStream::new);
        quote! {
            #doc
            #(#imports)*
            #(#functions)*
            #(#classes)*
            #code
        }
    }
}

impl ToRustCode for Option<Doc> {
    fn to_rust_code(self) -> TokenStream {
        match self {
            None => TokenStream::new(),
            Some(Doc(doc)) => {
                let doc = doc.trim();
                quote!(#[doc = #doc])
            },
        }
    }
}

pub fn to_rust_example_value(ty: &Ty, name: &str, spec: &HirSpec, use_ref_value: bool) -> Result<TokenStream> {
    let s = match ty {
        Ty::String => {
            let s = format!("your {}", name.to_case(Case::Lower));
            if use_ref_value {
                quote!(#s)
            } else {
                quote!(#s.to_owned())
            }
        }
        Ty::Integer { .. } => quote!(1),
        Ty::Float => quote!(1.0),
        Ty::Boolean => quote!(true),
        Ty::Array(inner) => {
            let use_ref_value = if !inner.is_reference_type() {
                false
            } else {
                use_ref_value
            };
            let inner = to_rust_example_value(inner, name, spec, use_ref_value)?;
            if use_ref_value {
                quote!(&[#inner])
            } else {
                quote!(vec![#inner])
            }
        }
        Ty::Model(model) => {
            let record = spec.get_record(model)?;
            let force_ref = model.ends_with("Required");
            match record {
                Record::Struct(Struct { name: _name, fields, nullable, docs: _docs }) => {
                    let fields = fields.iter().map(|(name, field)| {
                        let not_ref = !force_ref || field.optional;
                        let mut value = to_rust_example_value(&field.ty, name, spec, !not_ref)?;
                        let name = name.to_rust_ident();
                        if field.optional {
                            value = quote!(Some(#value));
                        }
                        Ok(quote!(#name: #value))
                    }).collect::<Result<Vec<_>, anyhow::Error>>()?;
                    let model = model.to_rust_struct();
                    quote!(#model{#(#fields),*})
                }
                Record::NewType(NewType { name, fields, docs: _docs }) => {
                    let fields = fields.iter().map(|f| {
                        to_rust_example_value(&f.ty, name, spec, false)
                    }).collect::<Result<Vec<_>, _>>()?;
                    let name = name.to_rust_struct();
                    quote!(#name(#(#fields),*))
                }
                Record::Enum(StrEnum { name, variants, docs: _docs }) => {
                    let variant = variants.first().unwrap();
                    let variant = variant.to_rust_struct();
                    let model = model.to_rust_struct();
                    quote!(#model::#variant)
                }
                Record::TypeAlias(name, HirField { ty, optional, .. }) => {
                    let not_ref = !force_ref || !optional;
                    let ty = to_rust_example_value(ty, name, spec, not_ref)?;
                    if *optional {
                        quote!(Some(#ty))
                    } else {
                        quote!(#ty)
                    }
                }
            }
        }
        Ty::Unit => quote!(()),
        Ty::Any => quote!(serde_json::json!({})),
        Ty::Date { .. } => quote!(chrono::Utc::now().date_naive()),
        Ty::DateTime { .. } => quote!(chrono::Utc::now()),
        Ty::Currency { .. } => quote!(rust_decimal_macros::dec!(100.01))
    };
    Ok(s)
}

impl ToRustCode for Literal<String> {
    fn to_rust_code(self) -> TokenStream {
        let s = self.0;
        quote!(#s)
    }
}

impl ToRustCode for ParamKey {
    fn to_rust_code(self) -> TokenStream {
        match self {
            ParamKey::Key(s) => quote!(#s),
            ParamKey::RepeatedKey(mut s) => {
                s += "[]";
                quote!(#s)
            }
        }
    }
}

/// If you can use reference types to represent the data (e.g. &str instead of String)
pub fn is_referenceable(schema: &Schema, spec: &OpenAPI) -> bool {
    match &schema.kind {
        SchemaKind::Type(openapiv3::Type::String(_)) => true,
        SchemaKind::Type(openapiv3::Type::Array(ArrayType {
                                                    items: Some(inner), ..
                                                })) => {
            let inner = inner.resolve(spec);
            is_primitive(inner, spec)
        }
        SchemaKind::AllOf { all_of } => {
            all_of.len() == 1 && is_primitive(all_of[0].resolve(spec), spec)
        }
        _ => false,
    }
}

fn rewrite_names(s: &str) -> String {
    // custom logic for Github openapi spec lol
    if s == "+1" {
        return "PlusOne".to_string();
    } else if s == "-1" {
        return "MinusOne".to_string();
    }
    s.replace('/', "_")
        .replace(['@', '\'', '+'], "")
        .replace(':', " ")
        .replace('.', "_")
}

fn sanitize(s: impl AsRef<str>) -> String {
    let s = s.as_ref();
    let original = s;
    let s = rewrite_names(s);
    let regex = Regex::new("[a-z]_[0-9]").unwrap();
    let mut s = s.to_case(Case::Snake);
    s = regex
        .replace_all(&s, |c: &Captures| {
            let mut c = c.get(0).unwrap().as_str().to_string();
            c.remove(1);
            c
        })
        .into();
    if is_restricted(&s) {
        s += "_"
    }
    if s.chars().next().unwrap().is_numeric() {
        s = format!("_{}", s)
    }
    assert_valid_ident(&s, original);
    s
}

fn sanitize_struct(s: impl AsRef<str>) -> Ident {
    let s = s.as_ref();
    let original = s;
    let s = rewrite_names(s);
    let mut s = s.to_case(Case::Pascal);
    if is_restricted(&s) {
        s += "Struct"
    }
    assert_valid_ident(&s, &original);
    Ident(s)
}

fn assert_valid_ident(s: &str, original: &str) {
    if s.chars().next().map(|c| c.is_numeric()).unwrap_or_default() {
        panic!("Numeric identifier: {}", original)
    }
    if s.contains('.') {
        panic!("Dot in identifier: {}", original)
    }
}

#[cfg(test)]
mod tests {
    use mir::{Ident, import, Import};

    use crate::rust::codegen::{ToRustCode, ToRustIdent};

    #[test]
    fn test_to_ident() {
        assert_eq!("meta/root".to_rust_ident().0, "meta_root");
    }

    #[test]
    fn test_to_ident1() {
        assert_eq!(
            "get-phone-checks-v0.1".to_rust_ident().0,
            "get_phone_checks_v0_1"
        );
    }

    #[test]
    fn test_star() {
        let i = import!("super::*");
        assert_eq!(i.to_rust_code().to_string(), "use super :: * ;");
        let i = Import::new("super", vec!["*"]);
        assert_eq!(i.to_rust_code().to_string(), "use super :: { * } ;");
    }

    #[test]
    fn test_import() {
        let import = import!("plaid::model::LinkTokenCreateRequestUser");
        assert_eq!(
            import.to_rust_code().to_string(),
            "use plaid :: model :: LinkTokenCreateRequestUser ;"
        );
        let import = import!("plaid::model", LinkTokenCreateRequestUser, Foobar);
        assert_eq!(
            import.to_rust_code().to_string(),
            "use plaid :: model :: { LinkTokenCreateRequestUser , Foobar } ;"
        );

        let import = Import::alias("plaid::model", "foobar");
        assert_eq!(
            import.to_rust_code().to_string(),
            "use plaid :: model as foobar ;"
        );

        let import = Import::package("foo_bar");
        assert_eq!(
            import.to_rust_code().to_string(),
            "use foo_bar ;"
        );
    }
}

pub fn is_restricted(s: &str) -> bool {
    [
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
    ].contains(&s)
}

pub fn serde_rename(one: &str, two: &Ident) -> TokenStream {
    if one != &two.0 {
        quote!(#[serde(rename = #one)])
    } else {
        TokenStream::new()
    }
}
