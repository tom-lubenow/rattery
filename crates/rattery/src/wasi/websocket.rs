//! WebSockets, provided by the host and subject to its origin policy and
//! cookie jar. `#[rattery::server]` functions using the `Websocket` protocol
//! are built on this; it is also usable directly.

use std::fmt;

use crate::bindings::websocket as w;

/// A message on a websocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

impl From<w::Message> for Message {
    fn from(message: w::Message) -> Self {
        match message {
            w::Message::Text(text) => Message::Text(text),
            w::Message::Binary(bytes) => Message::Binary(bytes),
        }
    }
}

impl From<Message> for w::Message {
    fn from(message: Message) -> Self {
        match message {
            Message::Text(text) => w::Message::Text(text),
            Message::Binary(bytes) => w::Message::Binary(bytes),
        }
    }
}

/// Why a websocket could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The host's origin policy refused the connection.
    Denied,
    /// Connecting or the handshake failed.
    Connect(String),
    /// The connection is closed, with the peer's reason if it gave one.
    Closed(Option<String>),
    /// A protocol or transport error after the connection opened.
    Protocol(String),
}

impl From<w::Error> for Error {
    fn from(error: w::Error) -> Self {
        match error {
            w::Error::Denied => Error::Denied,
            w::Error::Connect(e) => Error::Connect(e),
            w::Error::Closed(reason) => Error::Closed(reason),
            w::Error::Protocol(e) => Error::Protocol(e),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Denied => write!(
                f,
                "websocket connection refused by the host's origin policy"
            ),
            Error::Connect(e) => write!(f, "websocket connection failed: {e}"),
            Error::Closed(Some(reason)) => write!(f, "websocket closed: {reason}"),
            Error::Closed(None) => write!(f, "websocket closed"),
            Error::Protocol(e) => write!(f, "websocket error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// An open websocket connection.
pub struct WebSocket {
    socket: w::Socket,
}

impl WebSocket {
    /// Connect to `url` (`ws://`, `wss://`, `http://`, or `https://`; a path
    /// like `/api/chat` is resolved against the app's origin) and complete
    /// the handshake.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let url = if url.starts_with('/') {
            format!("{}{url}", server_fn::client::get_server_url())
        } else {
            url.to_owned()
        };
        let socket = w::Socket::connect(url).await?;
        Ok(Self { socket })
    }

    /// Wait for the next message. `Err(Closed)` once the peer closes.
    pub async fn next(&self) -> Result<Message, Error> {
        Ok(self.socket.receive().await?.into())
    }

    /// Queue a message to send.
    pub fn send(&self, message: impl Into<Message>) -> Result<(), Error> {
        self.socket
            .send(&message.into().into())
            .map_err(Error::from)
    }

    /// Close the connection.
    pub fn close(&self) {
        self.socket.close();
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Message::Text(text)
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Message::Text(text.to_owned())
    }
}

impl From<Vec<u8>> for Message {
    fn from(bytes: Vec<u8>) -> Self {
        Message::Binary(bytes)
    }
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocket").finish()
    }
}
