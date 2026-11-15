use proc_macro::TokenStream;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;

/// Marks a function as a test that `cargo test` runs inside a BEAM.
///
/// The function takes no argument or a `rustler::Env`, and returns a
/// `rustler_beam_test::Outcome`. See the documentation of `rustler_beam_test`.
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if let Some(token) = proc_macro2::TokenStream::from(attr).into_iter().next() {
        return syn::Error::new(token.span(), "this attribute takes no arguments")
            .into_compile_error()
            .into();
    }
    let mut function = syn::parse_macro_input!(item as syn::ItemFn);

    // `cargo test` reads these two from the `#[test]` function.
    let (test_attrs, other_attrs) = std::mem::take(&mut function.attrs)
        .into_iter()
        .partition(|attr| attr.path().is_ident("ignore") || attr.path().is_ident("should_panic"));
    function.attrs = other_attrs;

    let name = &function.sig.ident;
    let call = match function.sig.inputs.len() {
        0 => quote!(|_env| #name()),
        1 => quote!(|env| #name(env)),
        _ => {
            return syn::Error::new(
                function.sig.inputs.span(),
                "a test takes no argument or a `rustler::Env`",
            )
            .into_compile_error()
            .into()
        }
    };
    // A failure panics at the test function, not at the attribute.
    let run = quote_spanned!(name.span()=> ::rustler_beam_test::__private::run);

    quote! {
        #[cfg(any(test, feature = "beam_test"))]
        const _: () = {
            #function
            ::rustler_beam_test::__nif!(#name, #call);
        };

        #[cfg(test)]
        #[test]
        #(#test_attrs)*
        fn #name() {
            #run(&::rustler_beam_test::__private::Test {
                name: concat!(module_path!(), "::", stringify!(#name)),
                manifest_dir: env!("CARGO_MANIFEST_DIR"),
            })
        }
    }
    .into()
}
