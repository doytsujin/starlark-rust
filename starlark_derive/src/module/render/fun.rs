/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use gazebo::prelude::*;
use proc_macro2::TokenStream;
use quote::quote_spanned;

use crate::module::{
    typ::{SpecialParam, StarArg, StarArgPassStyle, StarArgSource, StarFun, StarFunSource},
    util::{ident_string, mut_token},
};

impl StarFun {
    fn type_expr(&self) -> TokenStream {
        match &self.type_attribute {
            Some(x) => quote_spanned! {
                self.span()=>
                std::option::Option::Some({
                    const TYPE_N: usize = #x.len();
                    static TYPE: starlark::values::StarlarkStrNRepr<TYPE_N> =
                        starlark::values::StarlarkStrNRepr::new(#x);
                    TYPE.unpack()
                })
            },
            None => {
                quote_spanned! {
                    self.span()=>
                    std::option::Option::None
                }
            }
        }
    }
}

pub(crate) fn render_fun(x: StarFun) -> syn::Result<TokenStream> {
    let span = x.span();

    let name_str = ident_string(&x.name);
    let signature = render_signature(&x)?;
    let documentation = render_documentation(&x)?;
    let binding = render_binding(&x);
    let is_method = x.is_method();

    let typ = x.type_expr();

    let StarFun {
        name,
        attrs,
        heap,
        eval,
        return_type,
        speculative_exec_safe,
        body,
        ..
    } = x;

    let signature_arg = signature.as_ref().map(
        |_| quote_spanned! {span=> __signature: &starlark::eval::ParametersSpec<starlark::values::FrozenValue>,},
    );
    let signature_val = signature
        .as_ref()
        .map(|_| quote_spanned! {span=> __signature});
    let signature_val_ref = signature
        .as_ref()
        .map(|_| quote_spanned! {span=> &__signature});

    let (this_param, this_arg, builder_set) = if is_method {
        (
            quote_spanned! {span=> __this: starlark::values::Value<'v>, },
            quote_spanned! {span=> __this, },
            quote_spanned! {span=>
                #[allow(clippy::redundant_closure)]
                globals_builder.set_method(
                    #name_str,
                    #speculative_exec_safe,
                    __documentation_renderer,
                    #typ,
                    move |eval, __this, parameters| {#name(eval, __this, parameters, #signature_val_ref)},
                );
            },
        )
    } else {
        (
            quote_spanned! {span=> },
            quote_spanned! {span=> },
            quote_spanned! {span=>
                #[allow(clippy::redundant_closure)]
                globals_builder.set_function(
                    #name_str,
                    #speculative_exec_safe,
                    __documentation_renderer,
                    #typ,
                    move |eval, parameters| {#name(eval, parameters, #signature_val_ref)},
                );
            },
        )
    };

    let let_eval = if let Some(SpecialParam { ident, ty }) = eval {
        Some(quote_spanned! {span=>
            let #ident: #ty = eval;
        })
    } else {
        None
    };
    let let_heap = if let Some(SpecialParam { ident, ty }) = heap {
        Some(quote_spanned! {span=>
            let #ident: #ty = eval.heap();
        })
    } else {
        None
    };

    Ok(quote_spanned! {
        span=>
        #( #attrs )*
        #[allow(non_snake_case)] // Starlark doesn't have this convention
        fn #name<'v>(
            eval: &mut starlark::eval::Evaluator<'v, '_>,
            #this_param
            parameters: &starlark::eval::Arguments<'v, '_>,
            #signature_arg
        ) -> anyhow::Result<starlark::values::Value<'v>> {
            fn inner<'v>(
                #[allow(unused_variables)]
                eval: &mut starlark::eval::Evaluator<'v, '_>,
                #this_param
                __args: &starlark::eval::Arguments<'v, '_>,
                #signature_arg
            ) -> #return_type {
                #binding
                #[allow(unused_variables)]
                let heap = eval.heap();
                #let_eval
                #let_heap
                #body
            }
            match inner(eval, #this_arg parameters, #signature_val) {
                Ok(v) => Ok(eval.heap().alloc(v)),
                Err(e) => Err(e),
            }
        }
        {
            #signature
            #documentation
            #builder_set
        }
    })
}

// Given __args and __signature (if render_signature was Some)
// create bindings for all the arguments
fn render_binding(x: &StarFun) -> TokenStream {
    let span = x.args_span();
    match x.source {
        StarFunSource::Parameters => {
            let StarArg {
                span,
                attrs,
                name,
                ty,
                ..
            } = &x.args[0];
            let span = *span;
            quote_spanned! { span=> #( #attrs )* let #name : #ty = __args; }
        }
        StarFunSource::ThisParameters => {
            let StarArg {
                span,
                attrs,
                name,
                ty,
                ..
            } = &x.args[1];
            let span = *span;
            let this = render_binding_arg(&x.args[0]);
            quote_spanned! {
                span=>
                #this
                #( #attrs )* let #name : #ty = __args;
            }
        }
        StarFunSource::Argument(arg_count) => {
            let bind_args = x.args.map(render_binding_arg);
            quote_spanned! {
                span=>
                let __args: [_; #arg_count] = __signature.collect_into(__args, eval.heap())?;
                #( #bind_args )*
            }
        }
        StarFunSource::Positional(required, optional) => {
            let bind_args = x.args.map(render_binding_arg);
            if optional == 0 {
                quote_spanned! {
                    span=>
                    __args.no_named_args()?;
                    let __required: [_; #required] = __args.positional(eval.heap())?;
                    #( #bind_args )*
                }
            } else {
                quote_spanned! {
                    span=>
                    __args.no_named_args()?;
                    let (__required, __optional): ([_; #required], [_; #optional]) = __args.optional(eval.heap())?;
                    #( #bind_args )*
                }
            }
        }
        ref s => unreachable!("Unknown StarFunSource: {:?}", s),
    }
}

// Create a binding for an argument given. If it requires an index, take from the index
fn render_binding_arg(arg: &StarArg) -> TokenStream {
    let span = arg.span;
    let name = &arg.name;
    let name_str = ident_string(name);
    let ty = &arg.ty;

    let source = match arg.source {
        StarArgSource::This => quote_spanned! {span=> __this},
        StarArgSource::Argument(i) => quote_spanned! {span=> __args[#i].get()},
        StarArgSource::Required(i) => quote_spanned! {span=> Some(__required[#i])},
        StarArgSource::Optional(i) => quote_spanned! {span=> __optional[#i]},
        ref s => unreachable!("unknown source: {:?}", s),
    };

    // Rust doesn't have powerful enough nested if yet
    let next = if arg.this {
        quote_spanned! { span=> starlark::eval::Arguments::check_this(#source)? }
    } else if arg.is_option() {
        assert!(
            arg.default.is_none(),
            "Can't have Option argument with a default, for `{}`",
            name_str
        );
        quote_spanned! { span=> starlark::eval::Arguments::check_optional(#name_str, #source)? }
    } else if !arg.is_value() && arg.default.is_some() {
        let default = arg
            .default
            .as_ref()
            .unwrap_or_else(|| unreachable!("Checked on the line above"));
        quote_spanned! { span=>
            {
                // Combo
                #[allow(clippy::manual_unwrap_or)]
                #[allow(clippy::unnecessary_lazy_evaluations)]
                #[allow(clippy::redundant_closure)]
                let x = starlark::eval::Arguments::check_optional(#name_str, #source)?.unwrap_or_else(|| #default);
                x
            }
        }
    } else {
        quote_spanned! { span=> starlark::eval::Arguments::check_required(#name_str, #source)? }
    };

    let mutability = mut_token(arg.mutable);
    let attrs = &arg.attrs;
    quote_spanned! {
        span=>
        #( #attrs )*
        let #mutability #name: #ty = #next;
    }
}

// Given the arguments, create a variable `signature` with a `ParametersSpec` object.
// Or return None if you don't need a signature
fn render_signature(x: &StarFun) -> syn::Result<Option<TokenStream>> {
    let span = x.args_span();
    if let StarFunSource::Argument(args_count) = x.source {
        let name_str = ident_string(&x.name);
        let sig_args = render_signature_args(&x.args)?;
        Ok(Some(quote_spanned! {
            span=>
            #[allow(unused_mut)]
            let mut __signature = starlark::eval::ParametersSpec::with_capacity(#name_str.to_owned(), #args_count);
            #sig_args
            let __signature = __signature.finish();
        }))
    } else {
        Ok(None)
    }
}

fn render_documentation(x: &StarFun) -> syn::Result<TokenStream> {
    let span = x.args_span();

    // A signature is not needed to invoke positional-only functions, but we still want
    // information like names, order, type, etc to be available to call '.documentation()' on.
    let args_count = match &x.source {
        StarFunSource::Argument(args_count) => Some(*args_count),
        StarFunSource::Positional(required, optional) => Some(required + optional),
        StarFunSource::Unknown | StarFunSource::Parameters | StarFunSource::ThisParameters => None,
    };
    let name_str = ident_string(&x.name);
    let documentation_signature = match args_count {
        Some(args_count) => {
            let sig_args = render_signature_args(&x.args)?;
            quote_spanned! {
                span=> {
                #[allow(unused_mut)]
                let mut __signature = starlark::eval::ParametersSpec::with_capacity(#name_str.to_owned(), #args_count);
                #sig_args
                __signature.finish()
                }
            }
        }
        None => {
            quote_spanned!(span=> starlark::eval::ParametersSpec::<starlark::values::FrozenValue>::new(#name_str.to_owned()).finish())
        }
    };

    let docs = match x.docstring.as_ref() {
        Some(d) => quote_spanned!(span=> Some(#d)),
        None => quote_spanned!(span=> None),
    };
    let return_type_arg = &x.return_type_arg;
    let parameter_types: Vec<_> = x.args
            .iter()
            .filter(|a| !a.this) // "this" gets ignored when creating the signature, so make sure the indexes match up.
            .enumerate()
            .map(|(i, arg)| {
                let typ = &arg.ty;
                quote_spanned!(span=> (#i, starlark::values::docs::Type { raw_type: stringify!(#typ).to_owned() }) )
            }).collect();

    Ok(quote_spanned!(span=>
        let __documentation_renderer = {
            let signature = #documentation_signature;
            let parameter_types = std::collections::HashMap::from([#(#parameter_types),*]);
            let return_type = Some(
                starlark::values::docs::Type {
                    raw_type: stringify!(#return_type_arg).to_owned()
                }
            );
            starlark::values::function::NativeCallableRawDocs {
                rust_docstring: #docs,
                signature,
                parameter_types,
                return_type,
            }
        };
    ))
}

fn render_signature_args(args: &[StarArg]) -> syn::Result<TokenStream> {
    let mut sig_args = TokenStream::new();
    let mut last_pass_style = StarArgPassStyle::PosOnly;
    for arg in args {
        if arg.is_args() {
            last_pass_style = StarArgPassStyle::NamedOnly;
        } else {
            if arg.pass_style != last_pass_style {
                match arg.pass_style {
                    StarArgPassStyle::PosOnly => {
                        return Err(syn::Error::new(
                            arg.span,
                            "Positional-only parameter after non-positional-only",
                        ));
                    }
                    StarArgPassStyle::PosOrNamed => sig_args.extend(quote_spanned! { arg.span=>
                        __signature.no_more_positional_only_args();
                    }),
                    StarArgPassStyle::NamedOnly => sig_args.extend(quote_spanned! { arg.span=>
                        __signature.no_more_positional_args();
                    }),
                }
            }
            last_pass_style = arg.pass_style;
        }
        sig_args.extend(render_signature_arg(arg));
    }
    Ok(sig_args)
}

// Generate a statement that modifies signature to add a new argument in.
fn render_signature_arg(arg: &StarArg) -> TokenStream {
    let span = arg.span;

    let name_str_full = ident_string(&arg.name);
    let name_str = name_str_full.trim_matches('_');

    if arg.is_args() {
        assert!(arg.default.is_none(), "Can't have *args with a default");
        quote_spanned! { span=> __signature.args();}
    } else if arg.is_kwargs() {
        assert!(arg.default.is_none(), "Can't have **kwargs with a default");
        quote_spanned! { span=> __signature.kwargs();}
    } else if arg.this {
        quote_spanned! { span=> }
    } else if arg.is_option() {
        quote_spanned! { span=> __signature.optional(#name_str);}
    } else if let Some(default) = &arg.default {
        // For things that are type Value, we put them on the frozen heap.
        // For things that aren't type value, use optional and then next_opt/unwrap
        // to avoid the to/from value conversion.
        if arg.is_value() {
            quote_spanned! { span=> __signature.defaulted(#name_str, globals_builder.alloc(#default));}
        } else {
            quote_spanned! { span=> __signature.optional(#name_str);}
        }
    } else {
        quote_spanned! { span=> __signature.required(#name_str);}
    }
}