//! A stub client for native builds.
//!
//! The `#[rattery::server]` macro names `rattery::ServerFnClient` in generated
//! code on every target. On the server the function body runs directly and the
//! client is never used, so this implementation only needs to exist, and to
//! fail loudly if something does call it.

use bytes::Bytes;
use futures::{Sink, Stream};
use http::Method;
use server_fn::client::Client;
use server_fn::error::{FromServerFnError, IntoAppError, ServerFnErrorErr};
use server_fn::request::ClientReq;
use server_fn::response::ClientRes;
use std::convert::Infallible;
use std::future::Future;

/// The client `#[rattery::server]` functions name on native targets.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerFnClient;

/// Never constructed on native targets.
pub struct Request(Infallible);

/// Never constructed on native targets.
pub struct Response(Infallible);

/// Never constructed on native targets.
#[derive(Debug, Default, Clone, Copy)]
pub struct FormData;

fn unavailable<E: FromServerFnError>() -> E {
    ServerFnErrorErr::Request(
        "rattery::ServerFnClient only sends requests from inside the rattery host \
         (target wasm32-wasip2); on native targets call the server function body directly"
            .into(),
    )
    .into_app_error()
}

impl<E: FromServerFnError> ClientReq<E> for Request {
    type FormData = FormData;

    fn try_new_req_query(_: &str, _: &str, _: &str, _: &str, _: Method) -> Result<Self, E> {
        Err(unavailable())
    }

    fn try_new_req_text(_: &str, _: &str, _: &str, _: String, _: Method) -> Result<Self, E> {
        Err(unavailable())
    }

    fn try_new_req_bytes(_: &str, _: &str, _: &str, _: Bytes, _: Method) -> Result<Self, E> {
        Err(unavailable())
    }

    fn try_new_req_form_data(
        _: &str,
        _: &str,
        _: &str,
        _: Self::FormData,
        _: Method,
    ) -> Result<Self, E> {
        Err(unavailable())
    }

    fn try_new_req_multipart(_: &str, _: &str, _: Self::FormData, _: Method) -> Result<Self, E> {
        Err(unavailable())
    }

    fn try_new_req_streaming(
        _: &str,
        _: &str,
        _: &str,
        _: impl Stream<Item = Bytes> + Send + 'static,
        _: Method,
    ) -> Result<Self, E> {
        Err(unavailable())
    }
}

impl<E: FromServerFnError> ClientRes<E> for Response {
    async fn try_into_string(self) -> Result<String, E> {
        match self.0 {}
    }

    async fn try_into_bytes(self) -> Result<Bytes, E> {
        match self.0 {}
    }

    #[allow(unreachable_code)]
    fn try_into_stream(
        self,
    ) -> Result<impl Stream<Item = Result<Bytes, Bytes>> + Send + Sync + 'static, E> {
        Ok::<futures::stream::Empty<Result<Bytes, Bytes>>, E>(match self.0 {})
    }

    fn status(&self) -> u16 {
        match self.0 {}
    }

    fn status_text(&self) -> String {
        match self.0 {}
    }

    fn location(&self) -> String {
        match self.0 {}
    }

    fn has_redirect(&self) -> bool {
        match self.0 {}
    }
}

impl<E, InputStreamError, OutputStreamError> Client<E, InputStreamError, OutputStreamError>
    for ServerFnClient
where
    E: FromServerFnError,
    InputStreamError: FromServerFnError,
    OutputStreamError: FromServerFnError,
{
    type Request = Request;
    type Response = Response;

    async fn send(req: Request) -> Result<Response, E> {
        match req.0 {}
    }

    async fn open_websocket(
        _path: &str,
    ) -> Result<
        (
            impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static,
            impl Sink<Bytes> + Send + 'static,
        ),
        E,
    > {
        Err::<
            (
                futures::stream::Empty<Result<Bytes, Bytes>>,
                futures::sink::Drain<Bytes>,
            ),
            E,
        >(unavailable())
    }

    fn spawn(future: impl Future<Output = ()> + Send + 'static) {
        // No runtime of our own on native targets; nothing here ever calls this.
        drop(future);
    }
}
