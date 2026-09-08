//! Multipart form data for file uploads.
//!
//! `server_fn`'s own multipart encoding is built on the browser's `FormData`,
//! so rattery provides its own: [`MultipartFormData`] is the encoding,
//! [`MultipartData`] is what the server function receives, and [`FormData`]
//! is how an app builds one.
//!
//! ```ignore
//! use rattery_app::multipart::{FormData, MultipartData, MultipartFormData};
//!
//! #[rattery_app::server(input = MultipartFormData)]
//! pub async fn upload(data: MultipartData) -> Result<String, ServerFnError> {
//!     let mut multipart = data.into_inner().expect("server side");
//!     while let Some(field) = multipart.next_field().await? {
//!         // field.name(), field.file_name(), field.bytes().await?
//!     }
//!     Ok("done".into())
//! }
//!
//! // in the app
//! let form = FormData::new()
//!     .text("note", "from rattery")
//!     .file("report", "report.txt", "text/plain", b"hello".to_vec());
//! let reply = upload(form.into()).await?;
//! ```
//!
//! The server function must take exactly one argument of type
//! [`MultipartData`]; `#[rattery_app::server]` generates the request plumbing.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use bytes::Bytes;
use futures::StreamExt;
use http::Method;
use server_fn::ContentType;
use server_fn::codec::Encoding;
use server_fn::error::{FromServerFnError, IntoAppError, ServerFnErrorErr, ServerFnErrorWrapper};
use server_fn::request::{ClientReq, Req};

/// The `multipart/form-data` encoding for `#[rattery_app::server(input = MultipartFormData)]`.
pub struct MultipartFormData;

impl ContentType for MultipartFormData {
    const CONTENT_TYPE: &'static str = "multipart/form-data";
}

impl Encoding for MultipartFormData {
    const METHOD: Method = Method::POST;
}

/// One part of a form: a text field or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// A form being built on the client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormData {
    parts: Vec<Part>,
}

impl FormData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text field.
    pub fn text(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parts.push(Part {
            name: name.into(),
            file_name: None,
            content_type: None,
            data: value.into().into_bytes(),
        });
        self
    }

    /// Add a file.
    pub fn file(
        mut self,
        name: impl Into<String>,
        file_name: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        self.parts.push(Part {
            name: name.into(),
            file_name: Some(file_name.into()),
            content_type: Some(content_type.into()),
            data: data.into(),
        });
        self
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Serialise as `multipart/form-data`. Returns the boundary and the body.
    pub fn encode(&self) -> (String, Bytes) {
        let boundary = boundary();
        let mut body = Vec::new();
        for part in &self.parts {
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            body.extend_from_slice(escape(&part.name).as_bytes());
            body.push(b'"');
            if let Some(file_name) = &part.file_name {
                body.extend_from_slice(b"; filename=\"");
                body.extend_from_slice(escape(file_name).as_bytes());
                body.push(b'"');
            }
            body.extend_from_slice(b"\r\n");
            if let Some(content_type) = &part.content_type {
                body.extend_from_slice(b"Content-Type: ");
                body.extend_from_slice(content_type.as_bytes());
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(&part.data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");
        (boundary, Bytes::from(body))
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ")
}

/// A boundary that will not appear in any sane payload. Not a security
/// boundary: it only has to be unique per request.
fn boundary() -> String {
    let random = RandomState::new();
    let mut out = String::from("rattery-");
    for _ in 0..2 {
        let mut hasher = random.build_hasher();
        hasher.write_u64(out.len() as u64);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

enum Inner {
    Client(FormData),
    Server(multer::Multipart<'static>),
}

/// The argument of a multipart server function: a form built by the app on
/// the client, the parsed stream of parts on the server.
pub struct MultipartData(Inner);

impl std::fmt::Debug for MultipartData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Inner::Client(form) => f.debug_tuple("MultipartData::Client").field(form).finish(),
            Inner::Server(_) => f.debug_tuple("MultipartData::Server").finish(),
        }
    }
}

impl MultipartData {
    /// The parts, as a [`multer::Multipart`] stream. `Some` on the server,
    /// `None` on the client.
    pub fn into_inner(self) -> Option<multer::Multipart<'static>> {
        match self.0 {
            Inner::Server(multipart) => Some(multipart),
            Inner::Client(_) => None,
        }
    }

    /// The form the app built. `Some` on the client, `None` on the server.
    pub fn into_form(self) -> Option<FormData> {
        match self.0 {
            Inner::Client(form) => Some(form),
            Inner::Server(_) => None,
        }
    }
}

impl From<FormData> for MultipartData {
    fn from(form: FormData) -> Self {
        Self(Inner::Client(form))
    }
}

/// Used by `#[rattery_app::server]`: build the request for a form.
#[doc(hidden)]
pub fn into_req<Request, E>(data: MultipartData, path: &str, accepts: &str) -> Result<Request, E>
where
    Request: ClientReq<E>,
    E: FromServerFnError,
{
    let form = data.into_form().ok_or_else(|| {
        ServerFnErrorErr::Request("multipart data can only be sent from the client".into())
            .into_app_error()
    })?;
    let (boundary, body) = form.encode();
    let content_type = format!("multipart/form-data; boundary={boundary}");
    Request::try_new_post_bytes(path, &content_type, accepts, body)
}

/// Used by `#[rattery_app::server]`: parse the request on the server.
#[doc(hidden)]
pub async fn from_req<Request, E>(req: Request) -> Result<MultipartData, E>
where
    Request: Req<E> + Send + 'static,
    E: FromServerFnError + Send + Sync,
{
    let boundary = req
        .to_content_type()
        .and_then(|ct| multer::parse_boundary(ct).ok())
        .ok_or_else(|| {
            ServerFnErrorErr::Deserialization("missing multipart boundary".into()).into_app_error()
        })?;
    let stream = req.try_into_stream()?;
    let multipart = multer::Multipart::new(
        stream.map(|chunk| chunk.map_err(|e| ServerFnErrorWrapper(E::de(e)))),
        boundary,
    );
    Ok(MultipartData(Inner::Server(multipart)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_fields_and_files() {
        let form = FormData::new().text("note", "hi \"there\"").file(
            "report",
            "r.txt",
            "text/plain",
            b"hello".to_vec(),
        );
        let (boundary, body) = form.encode();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        // Names are escaped; values are sent as they are.
        assert!(
            text.contains("Content-Disposition: form-data; name=\"note\"\r\n\r\nhi \"there\"\r\n")
        );
        assert!(text.contains(
            "name=\"report\"; filename=\"r.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n"
        ));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert_ne!(boundary, FormData::new().encode().0);
    }
}
