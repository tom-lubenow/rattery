//! WebSockets on behalf of the guest: tokio-tungstenite behind the
//! `rattery:tui/websocket` interface, with the origin policy, the embedder's
//! request policy, the cookie jar, and the queue and message limits applied
//! in both directions.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use http::header::{COOKIE, ORIGIN};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message as WsMessage, WebSocketConfig};
use url::Url;

use crate::bindings::websocket::{Error, Message};
use crate::http::{CookieJar, Decision, OriginPolicy, RequestInfo, RequestKind, RequestPolicy};
use crate::terminal::PhaseHook;
use crate::{Limits, Phase};

/// A bounded queue of messages: at most `capacity` messages and `bytes`
/// bytes together.
struct Queue {
    items: VecDeque<WsMessage>,
    bytes: usize,
    capacity: usize,
    max_bytes: usize,
    /// Incoming only: messages dropped because the app was not reading.
    dropped: u64,
    closed: bool,
}

impl Queue {
    fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            capacity: capacity.max(1),
            max_bytes: max_bytes.max(1),
            dropped: 0,
            closed: false,
        }
    }

    fn has_room_for(&self, size: usize) -> bool {
        self.items.len() < self.capacity && self.bytes + size <= self.max_bytes
    }

    fn push_back(&mut self, message: WsMessage) {
        self.bytes += message.len();
        self.items.push_back(message);
    }

    fn pop_front(&mut self) -> Option<WsMessage> {
        let message = self.items.pop_front()?;
        self.bytes -= message.len();
        Some(message)
    }
}

/// State shared between the socket resource, its connection task, and the
/// `send` and `receive` calls in flight.
pub struct Shared {
    incoming: Mutex<Queue>,
    incoming_notify: Notify,
    outgoing: Mutex<Queue>,
    /// Woken when the connection task drained something (room for `send`).
    room_notify: Notify,
    /// Woken when `send` queued something (work for the connection task).
    work_notify: Notify,
    failure: Mutex<Option<Error>>,
}

impl Shared {
    fn fail(&self, failure: Error) {
        let mut slot = self.failure.lock().unwrap();
        if slot.is_none() {
            *slot = Some(failure);
        }
        drop(slot);
        self.outgoing.lock().unwrap().closed = true;
        self.incoming_notify.notify_waiters();
        self.incoming_notify.notify_one();
        self.room_notify.notify_waiters();
        self.work_notify.notify_one();
    }

    fn failure(&self) -> Option<Error> {
        self.failure.lock().unwrap().clone()
    }

    /// Queue an incoming message; when the app is not reading, the oldest
    /// messages give way rather than the queue growing. A message that could
    /// never fit is a protocol violation and ends the connection.
    fn push_incoming(&self, message: WsMessage) -> Result<(), Error> {
        let mut queue = self.incoming.lock().unwrap();
        if message.len() > queue.max_bytes {
            return Err(Error::Protocol(format!(
                "message of {} bytes is over the queue limit of {} bytes",
                message.len(),
                queue.max_bytes
            )));
        }
        while !queue.has_room_for(message.len()) && queue.pop_front().is_some() {
            queue.dropped += 1;
        }
        queue.push_back(message);
        drop(queue);
        self.incoming_notify.notify_one();
        Ok(())
    }

    /// Wait for the next message. Backs `socket.receive`.
    pub async fn next_message(&self) -> Result<Message, Error> {
        loop {
            let notified = self.incoming_notify.notified();
            if let Some(message) = self.incoming.lock().unwrap().pop_front() {
                return Ok(convert_incoming(message));
            }
            if let Some(failure) = self.failure() {
                return Err(failure);
            }
            notified.await;
        }
    }

    /// Queue an outgoing message, waiting for room. Backs `socket.send`.
    /// A message larger than the whole queue is refused rather than waited
    /// for forever.
    pub async fn send(&self, message: Message) -> Result<(), Error> {
        let message = match message {
            Message::Text(text) => WsMessage::Text(text.into()),
            Message::Binary(bytes) => WsMessage::Binary(bytes.into()),
        };
        let max_bytes = self.outgoing.lock().unwrap().max_bytes;
        if message.len() > max_bytes {
            return Err(Error::Protocol(format!(
                "message of {} bytes is over the queue limit of {max_bytes} bytes",
                message.len()
            )));
        }
        loop {
            let notified = self.room_notify.notified();
            if let Some(failure) = self.failure() {
                return Err(failure);
            }
            {
                let mut queue = self.outgoing.lock().unwrap();
                if queue.has_room_for(message.len()) {
                    queue.push_back(message);
                    drop(queue);
                    self.work_notify.notify_one();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub fn close(&self) {
        self.outgoing.lock().unwrap().closed = true;
        self.work_notify.notify_one();
    }

    /// The next outgoing message, or `None` once closed and drained.
    async fn next_outgoing(&self) -> Option<WsMessage> {
        loop {
            let notified = self.work_notify.notified();
            {
                let mut queue = self.outgoing.lock().unwrap();
                if let Some(message) = queue.pop_front() {
                    drop(queue);
                    self.room_notify.notify_waiters();
                    return Some(message);
                }
                if queue.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

fn convert_incoming(message: WsMessage) -> Message {
    match message {
        WsMessage::Text(text) => Message::Text(text.to_string()),
        WsMessage::Binary(bytes) => Message::Binary(bytes.to_vec()),
        other => Message::Binary(other.into_data().to_vec()),
    }
}

/// The host side of a `websocket.socket` resource. Dropping it closes the
/// connection and stops its task.
pub struct WsSocket {
    shared: Arc<Shared>,
    task: tokio::task::AbortHandle,
    max_message_bytes: usize,
    _guard: crate::http::PolicyGuard,
    /// The socket slot; released with the resource however it ends.
    _slot: tokio::sync::OwnedSemaphorePermit,
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
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        url: &str,
        policy: &OriginPolicy,
        cookies: Option<&CookieJar>,
        request_policy: Option<&Arc<dyn RequestPolicy>>,
        on_phase: Option<&PhaseHook>,
        limits: &Limits,
        slot: tokio::sync::OwnedSemaphorePermit,
        tasks: &Arc<Mutex<Vec<JoinHandle<()>>>>,
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

        // tungstenite requires max_write_buffer_size > write_buffer_size.
        let write_buffer = (128 << 10).min(limits.websocket_queue_bytes);
        let config = WebSocketConfig::default()
            .max_message_size(Some(limits.websocket_message_bytes))
            .max_frame_size(Some(limits.websocket_message_bytes))
            .write_buffer_size(write_buffer)
            .max_write_buffer_size(limits.websocket_queue_bytes.max(write_buffer) + 1);
        let (stream, response) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|e| Error::Connect(e.to_string()))?;
        if let Some(jar) = cookies {
            jar.store_response(&http_url, response.headers());
        }

        let shared = Arc::new(Shared {
            incoming: Mutex::new(Queue::new(
                limits.websocket_queue,
                limits.websocket_queue_bytes,
            )),
            incoming_notify: Notify::new(),
            outgoing: Mutex::new(Queue::new(
                limits.websocket_queue,
                limits.websocket_queue_bytes,
            )),
            room_notify: Notify::new(),
            work_notify: Notify::new(),
            failure: Mutex::new(None),
        });
        let task = tokio::spawn(run_connection(stream, shared.clone()));
        let abort = task.abort_handle();
        tasks.lock().unwrap().push(task);
        Ok(Self {
            shared,
            task: abort,
            max_message_bytes: limits.websocket_message_bytes,
            _guard: guard,
            _slot: slot,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Refuse a message over the size limit before it is queued.
    pub fn check_size(&self, message: &Message) -> Result<(), Error> {
        let size = match message {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
        };
        if size > self.max_message_bytes {
            return Err(Error::Protocol(format!(
                "message of {size} bytes is over the limit of {} bytes",
                self.max_message_bytes
            )));
        }
        Ok(())
    }

    pub fn close(&self) {
        self.shared.close();
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
) {
    let (mut sink, mut source) = stream.split();
    let failure = loop {
        tokio::select! {
            next = shared.next_outgoing() => match next {
                Some(message) => {
                    if let Err(err) = sink.send(message).await {
                        break Error::Protocol(err.to_string());
                    }
                }
                None => {
                    let _ = sink.close().await;
                    break Error::Closed(None);
                }
            },
            incoming = source.next() => match incoming {
                Some(Ok(message @ (WsMessage::Text(_) | WsMessage::Binary(_)))) => {
                    if let Err(err) = shared.push_incoming(message) {
                        break err;
                    }
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
    shared.fail(failure);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(capacity: usize, max_bytes: usize) -> Shared {
        Shared {
            incoming: Mutex::new(Queue::new(capacity, max_bytes)),
            incoming_notify: Notify::new(),
            outgoing: Mutex::new(Queue::new(capacity, max_bytes)),
            room_notify: Notify::new(),
            work_notify: Notify::new(),
            failure: Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn oversized_messages_are_refused_in_both_directions() {
        let s = shared(4, 10);
        // Outgoing: too big for the queue is an error, not an endless wait.
        assert!(s.send(Message::Binary(vec![0; 11])).await.is_err());
        assert!(s.send(Message::Binary(vec![0; 10])).await.is_ok());
        // Incoming: too big ends the connection rather than overfilling.
        assert!(
            s.push_incoming(WsMessage::Binary(vec![0; 11].into()))
                .is_err()
        );
        assert!(
            s.push_incoming(WsMessage::Binary(vec![0; 6].into()))
                .is_ok()
        );
        assert!(
            s.push_incoming(WsMessage::Binary(vec![0; 6].into()))
                .is_ok()
        );
        let q = s.incoming.lock().unwrap();
        assert_eq!(q.items.len(), 1, "the older message gave way");
        assert_eq!(q.dropped, 1);
        assert!(q.bytes <= 10);
    }

    #[tokio::test]
    async fn send_waits_for_room_and_resumes() {
        let s = Arc::new(shared(1, 100));
        s.send(Message::Binary(vec![0; 5])).await.unwrap();
        let waiter = tokio::spawn({
            let s = s.clone();
            async move { s.send(Message::Binary(vec![0; 5])).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "queue is full, send must wait");
        assert!(s.next_outgoing().await.is_some());
        assert!(waiter.await.unwrap().is_ok());
    }

    #[test]
    fn queue_bounds_count_and_bytes() {
        let mut q = Queue::new(2, 10);
        assert!(q.has_room_for(6));
        q.push_back(WsMessage::Binary(vec![0; 6].into()));
        assert!(!q.has_room_for(5), "bytes cap");
        assert!(q.has_room_for(4));
        q.push_back(WsMessage::Binary(vec![0; 1].into()));
        assert!(!q.has_room_for(1), "count cap");
        q.pop_front();
        assert!(q.has_room_for(3));
        assert_eq!(q.bytes, 1);
    }
}
