//! No-op profiling attribute used by the pinned GPUI dependency.

#[proc_macro_attribute]
pub fn instrument(
    _attributes: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    item
}
