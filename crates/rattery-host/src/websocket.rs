//! WebSockets on behalf of the guest: tokio-tungstenite behind the
//! `rattery:tui/websocket` interface, with the origin policy and cookie jar
//! applied to the handshake.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use http::header::{COOKIE, ORIGIN};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use url::Url;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::p2::{DynPollable, Pollable, subscribe};

use crate::bindings::websocket::{Error, Message};
use crate::http::{CookieJar, Decision, OriginPolicy};

#[derive(Default)]
struct State {
    open: bool,
    /// Set when the socket opened and cleared once the guest asked.
    open_unreported: bool,
    incoming: VecDeque<Message>,
    failure: Option<Error>,
}

struct Shared {
    state: Mutex<State>,
    notify: Notify,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.notify.notify_one();
    }

    fn has_news(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.open_unreported || !state.incoming.is_empty() || state.failure.is_some()
    }
}

enum Outgoing {
    Message(WsMessage),
    Close,
}

/// The host side of a `websocket.socket` resource.
pub struct WsSocket {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
}

struct SocketReady(Arc<Shared>);

#[async_trait::async_trait]
impl Pollable for SocketReady {
    async fn ready(&mut self) {
        loop {
            let notified = self.0.notify.notified();
            if self.0.has_news() {
                return;
            }
            notified.await;
        }
    }
}

/// Map a websocket URL onto the http URL the policy and cookie jar understand.
fn as_http_url(url: &str) -> Option<Url> {
    let mut url = Url::parse(url).ok()?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    Some(url)
}

fn as_ws_url(url: &Url) -> Option<Url> {
    let mut url = url.clone();
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).ok()?;
    Some(url)
}

impl WsSocket {
    /// Start connecting. Policy violations and malformed URLs surface as a
    /// failure the guest reads through `receive`, so `connect` never fails.
    pub fn connect(url: &str, policy: &OriginPolicy, cookies: Option<&CookieJar>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            notify: Notify::new(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = Self {
            shared: shared.clone(),
            outgoing: tx,
        };

        let Some(http_url) = as_http_url(url) else {
            shared.update(|s| {
                s.failure = Some(Error::Connect(format!("invalid websocket URL {url:?}")))
            });
            return socket;
        };
        let uri: http::Uri = match http_url.as_str().parse() {
            Ok(uri) => uri,
            Err(err) => {
                shared.update(|s| s.failure = Some(Error::Connect(err.to_string())));
                return socket;
            }
        };
        if policy.decide(&uri) == Decision::Deny {
            shared.update(|s| s.failure = Some(Error::Denied));
            return socket;
        }
        let Some(ws_url) = as_ws_url(&http_url) else {
            shared.update(|s| s.failure = Some(Error::Connect("invalid websocket URL".into())));
            return socket;
        };

        let mut request = match ws_url.as_str().into_client_request() {
            Ok(request) => request,
            Err(err) => {
                shared.update(|s| s.failure = Some(Error::Connect(err.to_string())));
                return socket;
            }
        };
        if let Some(origin) = policy
            .app_origin()
            .and_then(crate::http::origin_header_value)
        {
            request.headers_mut().insert(ORIGIN, origin);
        }
        request.headers_mut().remove(COOKIE);
        if let Some(value) = cookies.and_then(|jar| jar.request_header(&http_url)) {
            request.headers_mut().insert(COOKIE, value);
        }
        let cookies = cookies.cloned();

        tokio::spawn(run_connection(request, http_url, cookies, shared, rx));
        socket
    }

    /// A pollable for this socket. It holds its own handle on the shared
    /// state rather than being a child resource, so the guest may drop the
    /// socket and the pollable in either order.
    pub fn subscribe(
        table: &mut ResourceTable,
        this: &Resource<WsSocket>,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        let shared = table.get(this)?.shared.clone();
        let ready = table.push(SocketReady(shared))?;
        subscribe(table, ready)
    }

    pub fn is_open(&self) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        state.open_unreported = false;
        state.open
    }

    pub fn receive(&self) -> Result<Option<Message>, Error> {
        let mut state = self.shared.state.lock().unwrap();
        state.open_unreported = false;
        if let Some(message) = state.incoming.pop_front() {
            return Ok(Some(message));
        }
        match &state.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(None),
        }
    }

    pub fn send(&self, message: Message) -> Result<(), Error> {
        {
            let state = self.shared.state.lock().unwrap();
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
        }
        let message = match message {
            Message::Text(text) => WsMessage::Text(text.into()),
            Message::Binary(bytes) => WsMessage::Binary(bytes.into()),
        };
        self.outgoing
            .send(Outgoing::Message(message))
            .map_err(|_| Error::Closed(None))
    }

    pub fn close(&self) {
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

impl Drop for WsSocket {
    fn drop(&mut self) {
        self.close();
    }
}

async fn run_connection(
    request: http::Request<()>,
    http_url: Url,
    cookies: Option<CookieJar>,
    shared: Arc<Shared>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
) {
    let (stream, response) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(err) => {
            shared.update(|s| s.failure = Some(Error::Connect(err.to_string())));
            return;
        }
    };
    if let Some(jar) = &cookies {
        jar.store_response(&http_url, response.headers());
    }
    shared.update(|s| {
        s.open = true;
        s.open_unreported = true;
    });

    let (mut sink, mut source) = stream.split();
    let failure = loop {
        tokio::select! {
            next = outgoing.recv() => match next {
                Some(Outgoing::Message(message)) => {
                    if let Err(err) = sink.send(message).await {
                        break Error::Protocol(err.to_string());
                    }
                }
                Some(Outgoing::Close) | None => {
                    let _ = sink.close().await;
                    break Error::Closed(None);
                }
            },
            incoming = source.next() => match incoming {
                Some(Ok(WsMessage::Text(text))) => {
                    shared.update(|s| s.incoming.push_back(Message::Text(text.to_string())));
                }
                Some(Ok(WsMessage::Binary(bytes))) => {
                    shared.update(|s| s.incoming.push_back(Message::Binary(bytes.to_vec())));
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    break Error::Closed(frame.map(|f| f.reason.to_string()));
                }
                Some(Ok(_)) => {} // ping/pong are handled by tungstenite
                Some(Err(err)) => break Error::Protocol(err.to_string()),
                None => break Error::Closed(None),
            },
        }
    };
    shared.update(|s| {
        s.open = false;
        s.failure = Some(failure);
    });
}
