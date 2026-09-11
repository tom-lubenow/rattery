//! Fetching the app component: from a URL like a browser would, from disk,
//! from memory, or through an embedder's resolver. Every path enforces the
//! size and download-time limits.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use url::Url;

use crate::{Limits, Resolved, Resolver, Source};

pub struct Loaded {
    pub bytes: Vec<u8>,
    /// `bytes` is native code from [`crate::precompile`], not a component.
    pub precompiled: bool,
    /// The app's own origin, if it has one. For a URL, the origin of the
    /// final URL after redirects, not the one requested.
    pub origin: Option<String>,
    /// The final URL, query included.
    pub location: Option<String>,
    pub description: String,
    /// Validators from the HTTP response or the resolver, used to poll for
    /// new versions.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub async fn load(source: &Source, limits: &Limits) -> Result<Loaded> {
    match source {
        Source::Url(url) => {
            let client = client(limits)?;
            match fetch_if_changed(&client, url, None, None, None, limits).await? {
                Fetch::New(loaded) => Ok(loaded),
                _ => unreachable!("an unconditional fetch always yields a body"),
            }
        }
        Source::Path(path) => {
            let metadata = tokio::fs::metadata(path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;
            check_size(metadata.len(), limits, &path.display().to_string())?;
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;
            check_size(bytes.len() as u64, limits, &path.display().to_string())?;
            Ok(Loaded {
                bytes,
                precompiled: false,
                origin: None,
                location: None,
                description: path.display().to_string(),
                etag: None,
                last_modified: None,
            })
        }
        Source::Bytes(bytes) | Source::Precompiled(bytes) => {
            check_size(bytes.len() as u64, limits, "the embedded component")?;
            Ok(Loaded {
                bytes: bytes.clone(),
                precompiled: matches!(source, Source::Precompiled(_)),
                origin: None,
                location: None,
                description: format!("{} bytes in memory", bytes.len()),
                etag: None,
                last_modified: None,
            })
        }
        Source::Resolver(resolver) => {
            let resolved = resolver
                .resolve(None)
                .await
                .context("the resolver failed")?
                .context("the resolver produced no component")?;
            Ok(from_resolved(resolved, limits)?)
        }
    }
}

pub fn from_resolved(resolved: Resolved, limits: &Limits) -> Result<Loaded> {
    check_size(
        resolved.bytes.len() as u64,
        limits,
        "the resolved component",
    )?;
    Ok(Loaded {
        precompiled: false,
        bytes: resolved.bytes,
        origin: resolved.origin,
        location: resolved.location,
        description: "a resolved component".into(),
        etag: resolved.version,
        last_modified: None,
    })
}

fn check_size(len: u64, limits: &Limits, what: &str) -> Result<()> {
    if len > limits.component_bytes as u64 {
        bail!(
            "{what} is {len} bytes, over the component size limit of {} bytes",
            limits.component_bytes
        );
    }
    Ok(())
}

/// A client that follows redirects only within one origin and gives up after
/// the download timeout.
pub fn client(limits: &Limits) -> Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .first()
            .map(|first| first.origin() == attempt.url().origin())
            .unwrap_or(true);
        if !same_origin {
            return attempt.error("cross-origin redirect refused");
        }
        if attempt.previous().len() > 10 {
            return attempt.error("too many redirects");
        }
        attempt.follow()
    });
    reqwest::Client::builder()
        .redirect(policy)
        .timeout(limits.download_timeout)
        .build()
        .context("failed to build an HTTP client")
}

pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The server answered `426 Upgrade Required` to a component fetch: it has
/// no build for this host's ABI.
#[derive(Debug, Clone)]
pub struct UpgradeRequired {
    /// The ABI the server named in its `rattery-abi` response header.
    pub required: Option<String>,
    pub message: String,
}

impl std::fmt::Display for UpgradeRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.required {
            Some(required) => write!(
                f,
                "the server requires ABI {required}; this host provides {}; upgrade the host",
                crate::ABI
            )?,
            None => write!(
                f,
                "the server requires an upgrade of the host ({})",
                crate::ABI
            )?,
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for UpgradeRequired {}

/// The outcome of a conditional fetch.
pub enum Fetch {
    /// `304`, or nothing to compare against changed.
    NotModified,
    /// `200` with exactly `previous` again, under these validators: the
    /// caller should remember them, or the next fetch downloads it again.
    Same {
        etag: Option<String>,
        last_modified: Option<String>,
    },
    New(Loaded),
}

/// Fetch `url` unless the server says it is unchanged. `previous` lets us
/// detect changes even from servers that send no validators.
pub async fn fetch_if_changed(
    client: &reqwest::Client,
    url: &Url,
    etag: Option<&str>,
    last_modified: Option<&str>,
    previous: Option<&[u8]>,
    limits: &Limits,
) -> Result<Fetch> {
    let mut request = client
        .get(url.clone())
        .header(crate::ABI_HEADER, crate::ABI);
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if let Some(last_modified) = last_modified {
        request = request.header(IF_MODIFIED_SINCE, last_modified);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(Fetch::NotModified);
    }
    if response.status() == StatusCode::UPGRADE_REQUIRED {
        // Both the header and the body come from the server: bound them.
        let required = response
            .headers()
            .get(crate::ABI_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| crate::sanitize::text(v).chars().take(256).collect());
        // Read no more of the body than can be shown.
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            body.extend_from_slice(&chunk[..chunk.len().min(1024 - body.len())]);
            if body.len() >= 1024 {
                break;
            }
        }
        let message: String = crate::sanitize::text(String::from_utf8_lossy(&body).trim())
            .chars()
            .take(256)
            .collect();
        return Err(anyhow::Error::new(UpgradeRequired { required, message })
            .context(format!("failed to fetch {url}")));
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to fetch {url}"))?;
    if let Some(len) = response.content_length() {
        check_size(len, limits, url.as_ref())?;
    }
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let etag = header(ETAG);
    let last_modified = header(LAST_MODIFIED);
    // Privileges come from where the bytes came from.
    let final_url = response.url().clone();

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to fetch {url}"))?;
        check_size((bytes.len() + chunk.len()) as u64, limits, url.as_ref())?;
        bytes.extend_from_slice(&chunk);
    }
    if previous.is_some_and(|previous| previous == bytes.as_slice()) {
        return Ok(Fetch::Same {
            etag,
            last_modified,
        });
    }
    Ok(Fetch::New(Loaded {
        precompiled: false,
        bytes,
        origin: Some(origin_of(&final_url)),
        location: Some(final_url.to_string()),
        description: final_url.to_string(),
        etag,
        last_modified,
    }))
}

/// Re-read a file source; `None` when it still holds `previous`.
pub async fn read_if_changed(
    path: &Path,
    previous: &[u8],
    limits: &Limits,
) -> Result<Option<Loaded>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    check_size(metadata.len(), limits, &path.display().to_string())?;
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    check_size(bytes.len() as u64, limits, &path.display().to_string())?;
    if bytes == previous {
        return Ok(None);
    }
    Ok(Some(Loaded {
        bytes,
        precompiled: false,
        origin: None,
        location: None,
        description: path.display().to_string(),
        etag: None,
        last_modified: None,
    }))
}

/// Poll a resolver for a new version.
pub async fn resolve_if_changed(
    resolver: &Arc<dyn Resolver>,
    current: Option<&str>,
    previous: &[u8],
    limits: &Limits,
) -> Result<Option<Loaded>> {
    match resolver.resolve(current).await? {
        Some(resolved) if resolved.bytes != previous => Ok(Some(from_resolved(resolved, limits)?)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_url() {
        let url = Url::parse("http://127.0.0.1:3000/app.wasm?x=1").unwrap();
        assert_eq!(origin_of(&url), "http://127.0.0.1:3000");
        let url = Url::parse("https://apps.example.com/counter/app.wasm").unwrap();
        assert_eq!(origin_of(&url), "https://apps.example.com");
    }

    #[test]
    fn from_source_picks_url_or_path() {
        use crate::App;
        assert!(matches!(
            App::from_source("http://x/app.wasm").unwrap().source,
            Source::Url(_)
        ));
        assert!(matches!(
            App::from_source("https://x/app.wasm").unwrap().source,
            Source::Url(_)
        ));
        assert!(matches!(
            App::from_source("file:///tmp/app.wasm").unwrap().source,
            Source::Path(_)
        ));
        assert!(matches!(
            App::from_source("target/app.wasm").unwrap().source,
            Source::Path(_)
        ));
        assert!(App::from_url("ftp://x/app.wasm").is_err());
    }

    #[test]
    fn size_limit() {
        let limits = Limits {
            component_bytes: 10,
            ..Limits::default()
        };
        assert!(check_size(11, &limits, "x").is_err());
        assert!(check_size(10, &limits, "x").is_ok());
    }
}
