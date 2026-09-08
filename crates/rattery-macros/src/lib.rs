//! Proc macros for [rattery](https://github.com/tom-lubenow/rattery).
//!
//! `#[rattery::server]` is `server_fn`'s `#[server]` attribute with two
//! defaults filled in: the client is `rattery::ServerFnClient` (HTTP over
//! `wasi:http`, sandboxed to the app's origin) and the API prefix is `/api`.
//! Every option `#[server]` accepts still works, including `client = ...`
//! and `prefix = "..."` to override those defaults.

use proc_macro::TokenStream;
use quote::ToTokens;
use server_fn_macro::ServerFnCall;

/// Declare a server function callable from a rattery app.
///
/// ```ignore
/// #[rattery::server]
/// pub async fn add(a: i64, b: i64) -> Result<i64, ServerFnError> {
///     Ok(a + b)
/// }
/// ```
#[proc_macro_attribute]
pub fn server(args: TokenStream, body: TokenStream) -> TokenStream {
    match ServerFnCall::parse("/api", args.into(), body.into()) {
        Err(e) => e.to_compile_error().into(),
        Ok(mut call) => {
            if call.get_args().client.is_none() {
                call.get_args_mut().client = Some(syn::parse_quote!(::rattery::ServerFnClient));
            }
            call.default_server_fn_path(Some(syn::parse_quote!(::rattery::server_fn)))
                .to_token_stream()
                .into()
        }
    }
}
