// SPDX-License-Identifier: MPL-2.0

//! OSTD's attributes for user space: a kernel test of the carried code is a host test.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Marks a test: a `#[test]` on the host, whose body runs in a task of its own, as OSTD's
/// test runner runs each test.
#[proc_macro_attribute]
pub fn ktest(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = parse_macro_input!(item as ItemFn);
    quote! {
        #[test]
        #(#attrs)*
        #vis #sig {
            ::ostd::task::__run_ktest(move || #block)
        }
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

/// Marks the kernel's panic handler. In user space `moruna-sys` holds the program's panic
/// handler and the build aborts on panic, so the function is kept and not called.
#[proc_macro_attribute]
pub fn panic_handler(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    quote! {
        #[expect(dead_code)]
        #f
    }
    .into()
}

/// Marks the kernel's global frame allocator. The personality's frames come from
/// `ostd::mm::FrameAllocOptions` over `moruna-sys`'s heap, so the item is kept and unused.
#[proc_macro_attribute]
pub fn global_frame_allocator(_attr: TokenStream, item: TokenStream) -> TokenStream {
    keep_unused(item)
}

/// Marks the kernel's global heap allocator: `moruna-sys`'s is the program's, so the item
/// is kept and unused.
#[proc_macro_attribute]
pub fn global_heap_allocator(_attr: TokenStream, item: TokenStream) -> TokenStream {
    keep_unused(item)
}

/// Marks the slab allocator's slot map: kept and unused, as the allocator is.
#[proc_macro_attribute]
pub fn global_heap_allocator_slot_map(_attr: TokenStream, item: TokenStream) -> TokenStream {
    keep_unused(item)
}

fn keep_unused(item: TokenStream) -> TokenStream {
    let item = proc_macro2::TokenStream::from(item);
    quote! {
        #[expect(dead_code)]
        #item
    }
    .into()
}
