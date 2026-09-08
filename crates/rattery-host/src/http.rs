//! Outbound HTTP policy: the terminal's same-origin rule, plus optional CORS.
//!
//! An app may talk to its own origin and to any origins the embedder allowed.
//! With CORS enabled, other origins can opt in per response with
//! `Access-Control-Allow-Origin`, as they do for browsers. Everything else is
//! refused before a socket is opened.

use std::future::Future;

use anyhow::{Context, Result, bail};
use http::header::{HeaderValue, ORIGIN};
use http::uri::Scheme;
use http_body_util::BodyExt;
use url::Url;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks, default_send_request};

/// Which origins an app may reach over HTTP.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    /// The app's own origin, normalised, sent as the `Origin` request header.
    app_origin: Option<String>,
    allow_all: bool,
    /// Normalised `scheme://host:port` strings.
    allowed: Vec<String>,
    cors: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Same origin or allow-listed: send and deliver the response.
    Allow,
    /// Cross-origin with CORS on: send, deliver only if the response opts in.
    Cors,
    /// Refuse without sending.
    Deny,
}

impl OriginPolicy {
    /// `app_origin` is always allowed; `extra` adds more.
    pub fn new(
        app_origin: Option<&str>,
        extra: &[String],
        allow_all: bool,
        cors: bool,
    ) -> Result<Self> {
        let app_origin = app_origin
            .map(|o| normalize_origin(o).with_context(|| format!("invalid origin {o:?}")))
            .transpose()?;
        let mut allowed: Vec<String> = app_origin.iter().cloned().collect();
        for candidate in extra {
            allowed.push(
                normalize_origin(candidate)
                    .with_context(|| format!("invalid origin {candidate:?}"))?,
            );
        }
        Ok(Self {
            app_origin,
            allow_all,
            allowed,
            cors,
        })
    }

    pub fn app_origin(&self) -> Option<&str> {
        self.app_origin.as_deref()
    }

    pub fn decide(&self, uri: &http::Uri) -> Decision {
        if self.allow_all {
            return Decision::Allow;
        }
        let Some(host) = uri.host() else {
            return Decision::Deny;
        };
        let scheme = uri.scheme().cloned().unwrap_or(Scheme::HTTP);
        let key = origin_key(&scheme, host, uri.port_u16());
        if self.allowed.contains(&key) {
            Decision::Allow
        } else if self.cors && self.app_origin.is_some() {
            Decision::Cors
        } else {
            Decision::Deny
        }
    }

    pub fn allows(&self, uri: &http::Uri) -> bool {
        self.decide(uri) == Decision::Allow
    }
}

/// Normalise to `scheme://host:port` with the default port made explicit.
pub fn normalize_origin(origin: &str) -> Result<String> {
    let url = Url::parse(origin)?;
    let scheme = match url.scheme() {
        "http" => Scheme::HTTP,
        "https" => Scheme::HTTPS,
        other => bail!("unsupported scheme {other:?}, expected http or https"),
    };
    let host = url.host_str().context("origin has no host")?;
    Ok(origin_key(&scheme, host, url.port()))
}

fn origin_key(scheme: &Scheme, host: &str, port: Option<u16>) -> String {
    let port = port.unwrap_or(if *scheme == Scheme::HTTPS { 443 } else { 80 });
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    format!("{scheme}://{host}:{port}")
}

/// The `Origin` header value for an app origin: no default port, as browsers send it.
fn origin_header_value(normalized: &str) -> Option<HeaderValue> {
    let (scheme, rest) = normalized.split_once("://")?;
    let (host, port) = rest.rsplit_once(':')?;
    let default_port = if scheme == "https" { "443" } else { "80" };
    let value = if port == default_port {
        format!("{scheme}://{host}")
    } else {
        normalized.to_owned()
    };
    HeaderValue::from_str(&value).ok()
}

/// Does a CORS response permit `app_origin` to read it?
fn cors_permits(headers: &http::HeaderMap, app_origin: &str) -> bool {
    let Some(value) = headers.get("access-control-allow-origin") else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let value = value.trim();
    if value == "*" {
        return true;
    }
    normalize_origin(value).ok().as_deref() == Some(app_origin)
}

pub struct OriginHooks {
    policy: OriginPolicy,
}

impl OriginHooks {
    pub fn new(policy: OriginPolicy) -> Self {
        Self { policy }
    }
}

type SendFuture = Box<
    dyn Future<
            Output = Result<
                (
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = Result<(), Error>> + Send>,
                ),
                Error,
            >,
        > + Send,
>;

impl WasiHttpHooks for OriginHooks {
    fn send_request(
        &mut self,
        mut request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> SendFuture {
        let decision = self.policy.decide(request.uri());
        if decision == Decision::Deny {
            return Box::new(async { Err(Error::HttpRequestDenied) });
        }
        let app_origin = self.policy.app_origin().map(str::to_owned);
        if let Some(value) = app_origin.as_deref().and_then(origin_header_value) {
            request.headers_mut().insert(ORIGIN, value);
        }
        Box::new(async move {
            let (response, io) = default_send_request(request, options).await?;
            if decision == Decision::Cors {
                let permitted = app_origin
                    .as_deref()
                    .is_some_and(|origin| cors_permits(response.headers(), origin));
                if !permitted {
                    return Err(Error::HttpRequestDenied);
                }
            }
            Ok((
                response.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn Future<Output = Result<(), Error>> + Send>,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> http::Uri {
        s.parse().unwrap()
    }

    #[test]
    fn same_origin_allowed_and_default_ports_normalised() {
        let policy = OriginPolicy::new(Some("http://localhost:3000"), &[], false, false).unwrap();
        assert!(policy.allows(&uri("http://localhost:3000/api/x")));
        assert!(!policy.allows(&uri("http://localhost:3001/api/x")));
        assert!(!policy.allows(&uri("https://localhost:3000/api/x")));

        let policy = OriginPolicy::new(Some("https://example.com"), &[], false, false).unwrap();
        assert!(policy.allows(&uri("https://example.com:443/")));
        assert!(policy.allows(&uri("https://EXAMPLE.com/")));
        assert!(!policy.allows(&uri("http://example.com/")));
    }

    #[test]
    fn extra_origins_and_allow_all() {
        let extra = vec!["https://api.example.com".to_owned()];
        let policy =
            OriginPolicy::new(Some("http://localhost:3000"), &extra, false, false).unwrap();
        assert!(policy.allows(&uri("https://api.example.com/v1")));
        assert!(!policy.allows(&uri("https://other.example.com/v1")));

        let policy = OriginPolicy::new(None, &[], true, false).unwrap();
        assert!(policy.allows(&uri("https://anything.example/")));

        let policy = OriginPolicy::new(None, &[], false, false).unwrap();
        assert!(!policy.allows(&uri("http://localhost/")));
    }

    #[test]
    fn cors_decisions() {
        let policy = OriginPolicy::new(Some("http://localhost:3000"), &[], false, true).unwrap();
        assert_eq!(
            policy.decide(&uri("http://localhost:3000/")),
            Decision::Allow
        );
        assert_eq!(
            policy.decide(&uri("https://api.example.com/")),
            Decision::Cors
        );

        // CORS needs an app origin to present.
        let policy = OriginPolicy::new(None, &[], false, true).unwrap();
        assert_eq!(
            policy.decide(&uri("https://api.example.com/")),
            Decision::Deny
        );

        let mut headers = http::HeaderMap::new();
        assert!(!cors_permits(&headers, "http://localhost:3000"));
        headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        assert!(cors_permits(&headers, "http://localhost:3000"));
        headers.insert(
            "access-control-allow-origin",
            HeaderValue::from_static("http://localhost:3000"),
        );
        assert!(cors_permits(&headers, "http://localhost:3000"));
        assert!(!cors_permits(&headers, "http://localhost:3001"));
    }

    #[test]
    fn origin_header_drops_default_port() {
        assert_eq!(
            origin_header_value("https://example.com:443").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            origin_header_value("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn rejects_bad_origins() {
        assert!(OriginPolicy::new(Some("ftp://x"), &[], false, false).is_err());
        assert!(OriginPolicy::new(Some("not a url"), &[], false, false).is_err());
    }
}
