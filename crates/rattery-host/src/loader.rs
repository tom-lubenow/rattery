//! Fetching the app component, from a URL like a browser would or from disk.

use anyhow::{Context, Result};
use url::Url;

pub struct Loaded {
    pub bytes: Vec<u8>,
    /// Where server functions go: the URL's origin, or `--origin`.
    pub origin: Option<String>,
    pub description: String,
}

pub async fn load(source: &str, origin_override: Option<&str>) -> Result<Loaded> {
    if let Some(url) = parse_http_url(source) {
        let bytes = fetch(&url).await?;
        let origin = origin_override
            .map(str::to_owned)
            .or_else(|| Some(origin_of(&url)));
        return Ok(Loaded {
            bytes,
            origin,
            description: url.to_string(),
        });
    }

    let bytes = tokio::fs::read(source)
        .await
        .with_context(|| format!("failed to read {source}"))?;
    Ok(Loaded {
        bytes,
        origin: origin_override.map(str::to_owned),
        description: source.to_owned(),
    })
}

fn parse_http_url(source: &str) -> Option<Url> {
    let url = Url::parse(source).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

async fn fetch(url: &Url) -> Result<Vec<u8>> {
    let response = reqwest::get(url.clone())
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to fetch {url}"))?;
    Ok(response.bytes().await?.to_vec())
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
    fn only_http_urls_are_fetched() {
        assert!(parse_http_url("http://x/app.wasm").is_some());
        assert!(parse_http_url("https://x/app.wasm").is_some());
        assert!(parse_http_url("file:///tmp/app.wasm").is_none());
        assert!(parse_http_url("target/app.wasm").is_none());
        assert!(parse_http_url("./app.wasm").is_none());
    }
}
