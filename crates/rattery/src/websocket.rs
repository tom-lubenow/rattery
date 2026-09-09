//! WebSockets on behalf of the guest: tokio-tungstenite behind the
//! `rattery:tui/websocket` interface, with the origin policy, the embedder's
//! request policy, the cookie jar, and the queue and message limits applied.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use http::header::{COOKIE, ORIGIN};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message as WsMessage, WebSocketConfig};
use url::Url;

use crate::bindings::websocket::{Error, Message};
use crate::http::{CookieJar, Decision, OriginPolicy, RequestInfo, RequestKind, RequestPolicy};
use crate::terminal::PhaseHook;
use crate::{Limits, Phase};

#[derive(Default)]
struct State {
    incoming: VecDeque<Message>,
    /// Messages dropped because the app was not reading.
    dropped: u64,
    failure: Option<Error>,
}

/// State shared between the socket resource, its reader task, and any
/// `receive` call in flight.
pub struct Shared {
    state: Mutex<State>,
    notify: Notify,
    queue_capacity: usize,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.notify.notify_one();
    }

    /// Queue an incoming message; when the app is not reading, the oldest
    /// message gives way rather than the queue growing.
    fn push(&self, message: Message) {
        self.update(|s| {
            if s.incoming.len() >= self.queue_capacity {
                s.incoming.pop_front();
                s.dropped += 1;
            }
            s.incoming.push_back(message);
        });
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

/// The host side of a `websocket.socket` resource. Dropping it closes the
/// connection and stops its reader task.
pub struct WsSocket {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    task: JoinHandle<()>,
    max_message_bytes: usize,
    _guard: crate::http::PolicyGuard,
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
    /// Connect and complete the handshake, applying the origin policy, the
    /// request policy, and the cookie jar.
    pub async fn connect(
        url: &str,
        policy: &OriginPolicy,
        cookies: Option<&CookieJar>,
        request_policy: Option<&Arc<dyn RequestPolicy>>,
        on_phase: Option<&PhaseHook>,
        limits: &Limits,
    ) -> Result<Self, Error> {
        let http_url = as_http_url(url)
            .ok_or_else(|| Error::Connect(format!("invalid websocket URL {url:?}")))?;
        let uri: http::Uri = http_url
            .as_str()
            .parse()
            .map_err(|e: http::uri::InvalidUri| Error::Connect(e.to_string()))?;
        if policy.decide(&uri) == Decision::Deny {
            if let Some(hook) = on_phase {
                hook(Phase::RequestDenied {
                    url: url.to_owned(),
                    reason: "origin not allowed".into(),
                });
            }
            return Err(Error::Denied);
        }
        let ws_url =
            as_ws_url(&http_url).ok_or_else(|| Error::Connect("invalid websocket URL".into()))?;

        let request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::Connect(e.to_string()))?;
        let (mut parts, ()) = request.into_parts();
        if let Some(origin) = policy
            .app_origin()
            .and_then(crate::http::origin_header_value)
        {
            parts.headers.insert(ORIGIN, origin);
        }
        parts.headers.remove(COOKIE);
        if let Some(value) = cookies.and_then(|jar| jar.request_header(&http_url)) {
            parts.headers.insert(COOKIE, value);
        }
        let info = RequestInfo {
            kind: RequestKind::Websocket,
            app_origin: policy.app_origin().map(str::to_owned),
            cross_origin: policy.is_cross_origin(&uri),
        };
        let guard = crate::http::apply_policy(request_policy, on_phase, &mut parts, &info)
            .await
            .map_err(|_| Error::Denied)?;
        let request = http::Request::from_parts(parts, ());

        let config = WebSocketConfig::default()
            .max_message_size(Some(limits.websocket_message_bytes))
            .max_frame_size(Some(limits.websocket_message_bytes));
        let (stream, response) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|e| Error::Connect(e.to_string()))?;
        if let Some(jar) = cookies {
            jar.store_response(&http_url, response.headers());
        }

        let shared = Arc::new(Shared {
            state: Mutex::default(),
            notify: Notify::new(),
            queue_capacity: limits.websocket_queue.max(1),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_connection(stream, shared.clone(), rx));
        Ok(Self {
            shared,
            outgoing: tx,
            task,
            max_message_bytes: limits.websocket_message_bytes,
            _guard: guard,
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
        let size = match &message {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
        };
        if size > self.max_message_bytes {
            return Err(Error::Protocol(format!(
                "message of {size} bytes is over the limit of {} bytes",
                self.max_message_bytes
            )));
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
        self.task.abort();
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
                Some(Ok(WsMessage::Text(text))) => shared.push(Message::Text(text.to_string())),
                Some(Ok(WsMessage::Binary(bytes))) => shared.push(Message::Binary(bytes.to_vec())),
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
