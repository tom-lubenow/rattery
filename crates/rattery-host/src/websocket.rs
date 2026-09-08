//! WebSockets on behalf of the guest: tokio-tungstenite behind the
//! `rattery:tui/websocket` interface, with the origin policy and cookie jar
//! applied to the handshake.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::bindings::websocket::{Error, Message};
use crate::http::{CookieJar, Decision, OriginPolicy};
use futures::{SinkExt, StreamExt};
use http::header::{COOKIE, ORIGIN};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use url::Url;

#[derive(Default)]
struct State {
    incoming: VecDeque<Message>,
    failure: Option<Error>,
}

/// State shared between the socket resource, its reader task, and any
/// `receive` call in flight.
pub struct Shared {
    state: Mutex<State>,
    notify: Notify,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// Wait for the next message. Backs `socket.receive`.
    pub async fn next_message(&self) -> Result<Message, Error> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().unwrap();
                if let Some(message) = state.incoming.pop_front() {
                    return Ok(message);
                }
                if let Some(failure) = &state.failure {
                    return Err(failure.clone());
                }
            }
            notified.await;
        }
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
    /// Connect and complete the handshake, applying the origin policy and
    /// attaching the cookie jar.
    pub async fn connect(
        url: &str,
        policy: &OriginPolicy,
        cookies: Option<&CookieJar>,
    ) -> Result<Self, Error> {
        let http_url = as_http_url(url)
            .ok_or_else(|| Error::Connect(format!("invalid websocket URL {url:?}")))?;
        let uri: http::Uri = http_url
            .as_str()
            .parse()
            .map_err(|e: http::uri::InvalidUri| Error::Connect(e.to_string()))?;
        if policy.decide(&uri) == Decision::Deny {
            return Err(Error::Denied);
        }
        let ws_url =
            as_ws_url(&http_url).ok_or_else(|| Error::Connect("invalid websocket URL".into()))?;

        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::Connect(e.to_string()))?;
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

        let (stream, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        if let Some(jar) = cookies {
            jar.store_response(&http_url, response.headers());
        }

        let shared = Arc::new(Shared {
            state: Mutex::default(),
            notify: Notify::new(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_connection(stream, shared.clone(), rx));
        Ok(Self {
            shared,
            outgoing: tx,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
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
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    shared: Arc<Shared>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
) {
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
    shared.update(|s| s.failure = Some(failure));
}
