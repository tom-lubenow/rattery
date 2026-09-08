//! Outbound HTTP policy: the terminal's same-origin rule.
//!
//! An app may talk to the origin it was loaded from, plus any origins the user
//! allowed on the command line. Everything else is refused before a socket is
//! opened.

use std::future::Future;

use anyhow::{Context, Result, bail};
use http::uri::Scheme;
use http_body_util::BodyExt;
use url::Url;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks, default_send_request};

#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    allow_all: bool,
    /// Normalised `scheme://host:port` strings.
    allowed: Vec<String>,
}

impl OriginPolicy {
    pub fn new(origin: Option<&str>, extra: &[String], allow_all: bool) -> Result<Self> {
        let mut allowed = Vec::new();
        for candidate in origin.into_iter().chain(extra.iter().map(String::as_str)) {
            allowed.push(
                normalize_origin(candidate)
                    .with_context(|| format!("invalid origin {candidate:?}"))?,
            );
        }
        Ok(Self { allow_all, allowed })
    }

    pub fn allows(&self, uri: &http::Uri) -> bool {
        if self.allow_all {
            return true;
        }
        let Some(host) = uri.host() else { return false };
        let scheme = uri.scheme().cloned().unwrap_or(Scheme::HTTP);
        let key = origin_key(&scheme, host, uri.port_u16());
        self.allowed.contains(&key)
    }
}

fn normalize_origin(origin: &str) -> Result<String> {
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
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> SendFuture {
        if !self.policy.allows(request.uri()) {
            return Box::new(async { Err(Error::HttpRequestDenied) });
        }
        Box::new(async move {
            let (response, io) = default_send_request(request, options).await?;
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
        let policy = OriginPolicy::new(Some("http://localhost:3000"), &[], false).unwrap();
        assert!(policy.allows(&uri("http://localhost:3000/api/x")));
        assert!(!policy.allows(&uri("http://localhost:3001/api/x")));
        assert!(!policy.allows(&uri("https://localhost:3000/api/x")));

        let policy = OriginPolicy::new(Some("https://example.com"), &[], false).unwrap();
        assert!(policy.allows(&uri("https://example.com:443/")));
        assert!(policy.allows(&uri("https://EXAMPLE.com/")));
        assert!(!policy.allows(&uri("http://example.com/")));
    }

    #[test]
    fn extra_origins_and_allow_all() {
        let extra = vec!["https://api.example.com".to_owned()];
        let policy = OriginPolicy::new(Some("http://localhost:3000"), &extra, false).unwrap();
        assert!(policy.allows(&uri("https://api.example.com/v1")));
        assert!(!policy.allows(&uri("https://other.example.com/v1")));

        let policy = OriginPolicy::new(None, &[], true).unwrap();
        assert!(policy.allows(&uri("https://anything.example/")));

        let policy = OriginPolicy::new(None, &[], false).unwrap();
        assert!(!policy.allows(&uri("http://localhost/")));
    }

    #[test]
    fn rejects_bad_origins() {
        assert!(OriginPolicy::new(Some("ftp://x"), &[], false).is_err());
        assert!(OriginPolicy::new(Some("not a url"), &[], false).is_err());
    }
}
