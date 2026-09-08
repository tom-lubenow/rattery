//! Fetching the app component, from a URL like a browser would or from disk.

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use url::Url;

use crate::Source;

pub struct Loaded {
    pub bytes: Vec<u8>,
    /// The app's own origin, if it has one.
    pub origin: Option<String>,
    pub description: String,
    /// Validators from the HTTP response, used to poll for new versions.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub async fn load(source: &Source) -> Result<Loaded> {
    match source {
        Source::Url(url) => {
            let client = reqwest::Client::new();
            let fetched = fetch_if_changed(&client, url, None, None, None)
                .await?
                .expect("an unconditional fetch always yields a body");
            Ok(Loaded {
                origin: Some(origin_of(url)),
                description: url.to_string(),
                ..fetched
            })
        }
        Source::Path(path) => Ok(Loaded {
            bytes: tokio::fs::read(path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?,
            origin: None,
            description: path.display().to_string(),
            etag: None,
            last_modified: None,
        }),
        Source::Bytes(bytes) => Ok(Loaded {
            bytes: bytes.clone(),
            origin: None,
            description: format!("{} bytes in memory", bytes.len()),
            etag: None,
            last_modified: None,
        }),
    }
}

pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Fetch `url` unless the server says it is unchanged. `previous` lets us
/// detect changes even from servers that send no validators.
pub async fn fetch_if_changed(
    client: &reqwest::Client,
    url: &Url,
    etag: Option<&str>,
    last_modified: Option<&str>,
    previous: Option<&[u8]>,
) -> Result<Option<Loaded>> {
    let mut request = client.get(url.clone());
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
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to fetch {url}"))?;
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let etag = header(ETAG);
    let last_modified = header(LAST_MODIFIED);
    let bytes = response.bytes().await?.to_vec();
    if previous.is_some_and(|previous| previous == bytes.as_slice()) {
        return Ok(None);
    }
    Ok(Some(Loaded {
        bytes,
        origin: Some(origin_of(url)),
        description: url.to_string(),
        etag,
        last_modified,
    }))
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
}
