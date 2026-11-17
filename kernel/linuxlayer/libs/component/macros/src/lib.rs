// SPDX-License-Identifier: MPL-2.0

//! `#[init_component]` in user space.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Ident, ItemFn, parse_macro_input};

/// Registers the function as a component's init, at the stage the attribute names
/// (`process` or `kthread`; bootstrap when none), for `component::init_all`.
#[proc_macro_attribute]
pub fn init_component(attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    let stage = if attr.is_empty() {
        quote!(::component::InitStage::Bootstrap)
    } else {
        let stage = parse_macro_input!(attr as Ident);
        match stage.to_string().as_str() {
            "process" => quote!(::component::InitStage::Process),
            "kthread" => quote!(::component::InitStage::Kthread),
            _ => {
                return syn::Error::new(stage.span(), "the stage is `process` or `kthread`")
                    .to_compile_error()
                    .into();
            }
        }
    };
    let name = &f.sig.ident;
    quote! {
        #f

        ::component::submit! {
            ::component::ComponentInit::new(#stage, ::core::module_path!(), #name)
        }
    }
    .into()
}
