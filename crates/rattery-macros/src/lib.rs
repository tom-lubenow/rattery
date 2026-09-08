//! Proc macros for [rattery](https://github.com/tom-lubenow/rattery).
//!
//! `#[rattery_app::server]` is `server_fn`'s `#[server]` attribute with two
//! defaults filled in: the client is `rattery::ServerFnClient` (HTTP over
//! `wasi:http`, sandboxed to the app's origin) and the API prefix is `/api`.
//! Every option `#[server]` accepts still works, including `client = ...`
//! and `prefix = "..."` to override those defaults.

use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use server_fn_macro::ServerFnCall;

/// Declare a server function callable from a rattery app.
///
/// ```ignore
/// #[rattery_app::server]
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
                call.get_args_mut().client = Some(syn::parse_quote!(::rattery_app::ServerFnClient));
            }
            let multipart = is_multipart_input(&call).then(|| multipart_impls(&call));
            let mut tokens = call
                .default_server_fn_path(Some(syn::parse_quote!(::rattery_app::server_fn)))
                .to_token_stream();
            tokens.extend(multipart);
            tokens.into()
        }
    }
}

/// `input = MultipartFormData` (by name, wherever it is imported from).
fn is_multipart_input(call: &ServerFnCall) -> bool {
    match &call.get_args().input {
        Some(syn::Type::Path(path)) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "MultipartFormData"),
        _ => false,
    }
}

/// `server_fn` can only implement its request traits for encodings it owns,
/// so for rattery's multipart encoding the impls are emitted here, on the
/// generated input struct, which lives in the caller's crate. They lean on the
/// `From` impls the macro generates for single-field inputs.
fn multipart_impls(call: &ServerFnCall) -> proc_macro2::TokenStream {
    let struct_name = call.struct_name();
    quote! {
        impl<__Req, __E> ::rattery_app::server_fn::codec::IntoReq<::rattery_app::multipart::MultipartFormData, __Req, __E>
            for #struct_name
        where
            __Req: ::rattery_app::server_fn::request::ClientReq<__E>,
            __E: ::rattery_app::server_fn::error::FromServerFnError,
        {
            fn into_req(self, path: &str, accepts: &str) -> Result<__Req, __E> {
                let data: ::rattery_app::multipart::MultipartData = self.into();
                ::rattery_app::multipart::into_req::<__Req, __E>(data, path, accepts)
            }
        }

        impl<__Req, __E> ::rattery_app::server_fn::codec::FromReq<::rattery_app::multipart::MultipartFormData, __Req, __E>
            for #struct_name
        where
            __Req: ::rattery_app::server_fn::request::Req<__E> + Send + 'static,
            __E: ::rattery_app::server_fn::error::FromServerFnError + Send + Sync,
        {
            async fn from_req(req: __Req) -> Result<Self, __E> {
                let data = ::rattery_app::multipart::from_req::<__Req, __E>(req).await?;
                Ok(Self::from(data))
            }
        }
    }
}
