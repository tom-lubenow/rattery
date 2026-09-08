//! A `server_fn` client that speaks `wasi:http@0.3`.
//!
//! The host decides where these requests may go: by default only the origin
//! the app was loaded from, the terminal equivalent of the same-origin policy.

use std::pin::Pin;
use std::rc::Rc;

use bytes::Bytes;
use futures::{Sink, Stream, StreamExt, TryStreamExt};
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, Method, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use send_wrapper::SendWrapper;
use server_fn::client::{Client, get_server_url};
use server_fn::error::{FromServerFnError, IntoAppError, ServerFnErrorErr};
use server_fn::request::ClientReq;
use server_fn::response::ClientRes;
use wasip3::http_compat::{IncomingResponseBody, http_from_wasi_response, http_into_wasi_request};

use crate::wasi::websocket::{Error as WsError, Message, WebSocket};

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
type OutgoingBody = UnsyncBoxBody<Bytes, BoxError>;

/// The client `#[rattery_app::server]` functions use on `wasm32-wasip2`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerFnClient;

/// A request waiting to be sent.
pub struct Request {
    method: Method,
    url: String,
    content_type: Option<String>,
    accepts: String,
    body: OutgoingBody,
}

/// Placeholder: multipart bodies are not supported by this client yet.
#[derive(Debug, Default, Clone, Copy)]
pub struct FormData;

/// A response received through `wasi:http`.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    url: String,
    body: SendWrapper<IncomingResponseBody>,
}

fn request_error<E: FromServerFnError>(msg: impl ToString) -> E {
    ServerFnErrorErr::Request(msg.to_string()).into_app_error()
}

fn absolute(path: &str) -> String {
    format!("{}{}", get_server_url(), path)
}

fn check_method<E: FromServerFnError>(method: &Method, allowed: &[Method]) -> Result<(), E> {
    if allowed.contains(method) {
        Ok(())
    } else {
        Err(E::from_server_fn_error(
            ServerFnErrorErr::UnsupportedRequestMethod(method.to_string()),
        ))
    }
}

fn empty_body() -> OutgoingBody {
    Empty::new()
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}

fn bytes_body(bytes: Bytes) -> OutgoingBody {
    Full::new(bytes)
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}

impl<E: FromServerFnError> ClientReq<E> for Request {
    type FormData = FormData;

    fn try_new_req_query(
        path: &str,
        content_type: &str,
        accepts: &str,
        query: &str,
        method: Method,
    ) -> Result<Self, E> {
        check_method(
            &method,
            &[
                Method::GET,
                Method::DELETE,
                Method::HEAD,
                Method::POST,
                Method::PATCH,
                Method::PUT,
            ],
        )?;
        let mut url = absolute(path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        Ok(Self {
            method,
            url,
            content_type: Some(content_type.to_owned()),
            accepts: accepts.to_owned(),
            body: empty_body(),
        })
    }

    fn try_new_req_text(
        path: &str,
        content_type: &str,
        accepts: &str,
        body: String,
        method: Method,
    ) -> Result<Self, E> {
        check_method(&method, &[Method::POST, Method::PUT, Method::PATCH])?;
        Ok(Self {
            method,
            url: absolute(path),
            content_type: Some(content_type.to_owned()),
            accepts: accepts.to_owned(),
            body: bytes_body(Bytes::from(body)),
        })
    }

    fn try_new_req_bytes(
        path: &str,
        content_type: &str,
        accepts: &str,
        body: Bytes,
        method: Method,
    ) -> Result<Self, E> {
        check_method(&method, &[Method::POST, Method::PUT, Method::PATCH])?;
        Ok(Self {
            method,
            url: absolute(path),
            content_type: Some(content_type.to_owned()),
            accepts: accepts.to_owned(),
            body: bytes_body(body),
        })
    }

    fn try_new_req_form_data(
        _path: &str,
        _accepts: &str,
        _content_type: &str,
        _body: Self::FormData,
        _method: Method,
    ) -> Result<Self, E> {
        Err(request_error(
            "form-data bodies are not supported by rattery's client yet",
        ))
    }

    fn try_new_req_multipart(
        _path: &str,
        _accepts: &str,
        _body: Self::FormData,
        _method: Method,
    ) -> Result<Self, E> {
        Err(request_error(
            "multipart bodies are not supported by rattery's client yet",
        ))
    }

    fn try_new_req_streaming(
        path: &str,
        accepts: &str,
        content_type: &str,
        body: impl Stream<Item = Bytes> + Send + 'static,
        method: Method,
    ) -> Result<Self, E> {
        check_method(&method, &[Method::POST, Method::PUT, Method::PATCH])?;
        let body =
            StreamBody::new(body.map(|chunk| Ok::<_, BoxError>(http_body::Frame::data(chunk))))
                .boxed_unsync();
        Ok(Self {
            method,
            url: absolute(path),
            content_type: Some(content_type.to_owned()),
            accepts: accepts.to_owned(),
            body,
        })
    }
}

/// A `Sink` over a `!Send` sink; sound because the guest is single threaded.
struct SendSink<S>(SendWrapper<S>);

impl<S: Sink<Bytes> + Unpin> Sink<Bytes> for SendSink<S> {
    type Error = S::Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), S::Error> {
        Pin::new(&mut *self.get_mut().0).start_send(item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_close(cx)
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

    fn send(req: Request) -> impl Future<Output = Result<Response, E>> + Send {
        // wasi resources are !Send; the guest is single-threaded so this is sound.
        SendWrapper::new(async move {
            let mut builder = http::Request::builder()
                .method(req.method)
                .uri(req.url.as_str())
                .header(ACCEPT, req.accepts.as_str());
            if let Some(content_type) = &req.content_type {
                builder = builder.header(CONTENT_TYPE, content_type.as_str());
            }
            let request = builder.body(req.body).map_err(request_error::<E>)?;
            let request = http_into_wasi_request(request)
                .map_err(|e| request_error::<E>(format!("{e:?}")))?;
            let response = wasip3::http::client::send(request)
                .await
                .map_err(|e| request_error::<E>(format!("{e:?}")))?;
            let response = http_from_wasi_response(response)
                .map_err(|e| request_error::<E>(format!("{e:?}")))?;
            let (parts, body) = response.into_parts();
            Ok(Response {
                status: parts.status,
                headers: parts.headers,
                url: req.url,
                body: SendWrapper::new(body),
            })
        })
    }

    fn open_websocket(
        path: &str,
    ) -> impl Future<
        Output = Result<
            (
                impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static,
                impl Sink<Bytes> + Send + 'static,
            ),
            E,
        >,
    > + Send {
        let path = path.to_owned();
        SendWrapper::new(async move {
            let socket = WebSocket::connect(&path).await.map_err(|err| {
                E::from_server_fn_error(ServerFnErrorErr::Request(err.to_string()))
            })?;
            let socket = Rc::new(socket);

            let stream = futures::stream::unfold(socket.clone(), |socket| async move {
                let item = match socket.next().await {
                    Ok(Message::Text(text)) => Ok(Bytes::from(text)),
                    Ok(Message::Binary(bytes)) => Ok(Bytes::from(bytes)),
                    Err(WsError::Closed(_)) => return None,
                    Err(err) => Err(OutputStreamError::from_server_fn_error(
                        ServerFnErrorErr::Request(err.to_string()),
                    )
                    .ser()),
                };
                Some((item, socket))
            });
            let stream = SendWrapper::new(Box::pin(stream));

            let sink = futures::sink::unfold(socket, |socket, bytes: Bytes| async move {
                socket
                    .send(Message::Binary(bytes.to_vec()))
                    .map_err(|err| ServerFnErrorErr::Request(err.to_string()))?;
                Ok::<_, ServerFnErrorErr>(socket)
            });
            let sink = SendSink(SendWrapper::new(Box::pin(sink)));
            Ok((stream, sink))
        })
    }

    fn spawn(future: impl Future<Output = ()> + Send + 'static) {
        wit_bindgen::spawn_local(future);
    }
}

fn response_error<E: FromServerFnError>(msg: impl ToString) -> E {
    ServerFnErrorErr::Deserialization(msg.to_string()).into_app_error()
}

impl<E: FromServerFnError> ClientRes<E> for Response {
    fn try_into_string(self) -> impl Future<Output = Result<String, E>> + Send {
        SendWrapper::new(async move {
            let bytes = self
                .body
                .take()
                .collect()
                .await
                .map_err(|e| response_error::<E>(format!("{e:?}")))?;
            String::from_utf8(bytes.to_bytes().to_vec()).map_err(response_error::<E>)
        })
    }

    fn try_into_bytes(self) -> impl Future<Output = Result<Bytes, E>> + Send {
        SendWrapper::new(async move {
            self.body
                .take()
                .collect()
                .await
                .map(|collected| collected.to_bytes())
                .map_err(|e| response_error::<E>(format!("{e:?}")))
        })
    }

    fn try_into_stream(
        self,
    ) -> Result<impl Stream<Item = Result<Bytes, Bytes>> + Send + Sync + 'static, E> {
        let stream = http_body_util::BodyStream::new(self.body.take())
            .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) })
            .map_err(|e| {
                E::from_server_fn_error(ServerFnErrorErr::Response(format!("{e:?}"))).ser()
            });
        Ok(SendWrapper::new(stream))
    }

    fn status(&self) -> u16 {
        self.status.as_u16()
    }

    fn status_text(&self) -> String {
        self.status.to_string()
    }

    fn location(&self) -> String {
        self.headers
            .get(http::header::LOCATION)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_else(|| self.url.clone())
    }

    fn has_redirect(&self) -> bool {
        self.headers.contains_key(http::header::LOCATION)
    }
}
