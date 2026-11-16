// SPDX-License-Identifier: MPL-2.0

//! OSTD's attributes for user space: a kernel test of the carried code is a host test.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Marks a test: a `#[test]` on the host.
#[proc_macro_attribute]
pub fn ktest(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    quote! {
        #[test]
        #f
    }
    .into()
}

/// Marks the early command-line parser. The personality sets its log level itself, so
/// the parser is kept and not called.
#[proc_macro_attribute]
pub fn early_cmdline_parser(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    quote! {
        #[expect(dead_code)]
        #f
    }
    .into()
}
