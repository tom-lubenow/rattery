//! Outbound HTTP: the origin policy, the embedder's request policy,
//! concurrency and size limits, and the cookie jar.
//!
//! An app may talk to its own origin and to origins the embedder allowed.
//! Everything else is refused before a socket is opened. There is no CORS
//! mode: browser-style cross-origin access needs preflight and credential
//! semantics this host does not implement, so cross-origin is allow-list only.

use std::any::Any;
use std::future::Future;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use cookie_store::CookieStore;
use futures::future::BoxFuture;
use http::header::{COOKIE, HeaderValue, ORIGIN, SET_COOKIE};
use http::uri::Scheme;
use http_body_util::{BodyExt, Limited};
use tokio::sync::Semaphore;
use url::Url;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks, default_send_request};

use crate::{Limits, Phase};

/// Which origins an app may reach over HTTP and websockets.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    /// The app's own origin, normalised, sent as the `Origin` request header.
    app_origin: Option<String>,
    allow_all: bool,
    /// Normalised `scheme://host:port` strings.
    allowed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Same origin or allow-listed: send and deliver the response.
    Allow,
    /// Refuse without sending.
    Deny,
}

impl OriginPolicy {
    /// `app_origin` is always allowed; `extra` adds more.
    pub fn new(app_origin: Option<&str>, extra: &[String], allow_all: bool) -> Result<Self> {
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
        })
    }

    pub fn app_origin(&self) -> Option<&str> {
        self.app_origin.as_deref()
    }

    pub fn decide(&self, uri: &http::Uri) -> Decision {
        if self.allow_all {
            return Decision::Allow;
        }
        match self.origin_key_of(uri) {
            Some(key) if self.allowed.contains(&key) => Decision::Allow,
            _ => Decision::Deny,
        }
    }

    pub fn allows(&self, uri: &http::Uri) -> bool {
        self.decide(uri) == Decision::Allow
    }

    /// True if `uri` is not the app's own origin.
    pub fn is_cross_origin(&self, uri: &http::Uri) -> bool {
        match (&self.app_origin, self.origin_key_of(uri)) {
            (Some(app), Some(key)) => *app != key,
            _ => true,
        }
    }

    fn origin_key_of(&self, uri: &http::Uri) -> Option<String> {
        let host = uri.host()?;
        let scheme = uri.scheme().cloned().unwrap_or(Scheme::HTTP);
        Some(origin_key(&scheme, host, uri.port_u16()))
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
pub fn origin_header_value(normalized: &str) -> Option<HeaderValue> {
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

// ---------------------------------------------------------------------------
// Request policy: the embedder's hook into every outbound request.

/// What kind of request a policy is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Http,
    /// The websocket handshake.
    Websocket,
}

/// Context handed to a [`RequestPolicy`].
#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub kind: RequestKind,
    /// The app's own origin, normalised.
    pub app_origin: Option<String>,
    /// True if the request leaves the app's origin.
    pub cross_origin: bool,
}

/// Why a policy refused a request. The app sees a generic denial; the reason
/// reaches the embedder through [`Phase::RequestDenied`].
#[derive(Debug, Clone)]
pub struct PolicyError {
    pub reason: String,
}

impl PolicyError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for PolicyError {}

/// Something kept alive until the request completes, for example a
/// concurrency permit.
pub type PolicyGuard = Option<Box<dyn Any + Send>>;

/// An asynchronous hook on every outbound request the app makes, after the
/// origin policy allowed it and before it is sent. Use it for route
/// authorisation, injecting or refreshing credentials, per-route limits, or
/// auditing. Implement it for a type and pass it to
/// [`App::request_policy`](crate::App::request_policy).
pub trait RequestPolicy: Send + Sync + 'static {
    /// Inspect or edit the request head. Return `Err` to refuse it, or a
    /// guard to hold until the response is complete.
    fn on_request<'a>(
        &'a self,
        request: &'a mut http::request::Parts,
        info: &'a RequestInfo,
    ) -> BoxFuture<'a, Result<PolicyGuard, PolicyError>>;

    /// Observe the response head.
    fn on_response<'a>(
        &'a self,
        _response: &'a http::response::Parts,
        _info: &'a RequestInfo,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

// ---------------------------------------------------------------------------
// Cookie jar.

/// Cookies the app's servers set, kept the way a browser keeps them: the app
/// never sees `Cookie` or `Set-Cookie` headers, the host attaches and records
/// them. Optionally persisted to a private JSON file between runs.
///
/// The file is created with owner-only permissions in an owner-only
/// directory, is never followed through a symbolic link, is shared between
/// processes under a lock, and is replaced atomically and durably.
#[derive(Clone)]
pub struct CookieJar {
    store: Arc<Mutex<CookieStore>>,
    path: Option<PathBuf>,
    max_per_host: usize,
    max_cookie_bytes: usize,
}

/// Cookies kept per host before new ones are refused.
pub const DEFAULT_COOKIES_PER_HOST: usize = 64;
/// Longest `Set-Cookie` header value accepted.
pub const DEFAULT_COOKIE_BYTES: usize = 4096;

impl CookieJar {
    /// An in-memory jar that is forgotten when the app ends.
    pub fn ephemeral() -> Self {
        Self {
            store: Arc::new(Mutex::new(CookieStore::new())),
            path: None,
            max_per_host: DEFAULT_COOKIES_PER_HOST,
            max_cookie_bytes: DEFAULT_COOKIE_BYTES,
        }
    }

    /// A jar loaded from and saved to `path` (created on first save).
    /// Refuses a path that is a symbolic link; a corrupt file starts empty.
    pub fn at(path: PathBuf) -> Result<Self> {
        if is_symlink(&path) {
            bail!(
                "cookie jar {} is a symbolic link; refusing to use it",
                path.display()
            );
        }
        let store = with_jar_lock(&path, || load_store(&path))?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            path: Some(path),
            max_per_host: DEFAULT_COOKIES_PER_HOST,
            max_cookie_bytes: DEFAULT_COOKIE_BYTES,
        })
    }

    /// The default location: `rattery/cookies.json` in the user's local data dir.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "rattery")
            .map(|dirs| dirs.data_local_dir().join("cookies.json"))
    }

    pub fn request_header(&self, url: &Url) -> Option<HeaderValue> {
        let store = self.store.lock().unwrap();
        let value = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if value.is_empty() {
            None
        } else {
            HeaderValue::from_str(&value).ok()
        }
    }

    /// Record `Set-Cookie` headers, within quota: oversized cookies and
    /// cookies beyond the per-domain limit are ignored. With a persistent
    /// jar this is one locked load-modify-save transaction, so concurrent
    /// processes never overwrite each other's cookies.
    pub fn store_response(&self, url: &Url, headers: &http::HeaderMap) {
        let values: Vec<&str> = headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|text| text.len() <= self.max_cookie_bytes)
            .collect();
        if values.is_empty() {
            return;
        }
        match &self.path {
            None => {
                let mut store = self.store.lock().unwrap();
                self.merge(&mut store, url, &values);
            }
            Some(path) => {
                let result = with_jar_lock(path, || {
                    let mut store = load_store(path)?;
                    self.merge(&mut store, url, &values);
                    save_private(path, |file| {
                        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, file)
                            .map_err(|e| std::io::Error::other(e.to_string()))
                    })?;
                    Ok(store)
                });
                match result {
                    Ok(store) => *self.store.lock().unwrap() = store,
                    Err(err) => {
                        eprintln!(
                            "rattery: could not save cookies to {}: {err}",
                            path.display()
                        )
                    }
                }
            }
        }
    }

    /// Apply `Set-Cookie` values within the per-domain quota, counting every
    /// cookie stored for the domain whatever its path.
    fn merge(&self, store: &mut CookieStore, url: &Url, values: &[&str]) {
        for text in values {
            let Ok(candidate) = cookie_store::Cookie::parse(*text, url) else {
                continue;
            };
            let mut for_domain = 0usize;
            let mut replaces = false;
            for existing in store.iter_any().filter(|c| c.domain.matches(url)) {
                for_domain += 1;
                if existing.name() == candidate.name()
                    && existing.domain == candidate.domain
                    && existing.path == candidate.path
                {
                    replaces = true;
                }
            }
            if for_domain >= self.max_per_host && !replaces {
                continue;
            }
            let _ = store.insert(candidate.into_owned(), url);
        }
    }
}

fn load_store(path: &Path) -> Result<CookieStore> {
    match open_no_follow(path) {
        Some(file) => {
            Ok(cookie_store::serde::json::load_all(BufReader::new(file)).unwrap_or_default())
        }
        None => Ok(CookieStore::default()),
    }
}

/// Run `f` holding the jar's lock file exclusively: one lock for readers and
/// writers, so a load-modify-save is a transaction.
pub(crate) fn with_jar_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = path
        .parent()
        .context("cookie jar has no parent directory")?;
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    dir.create(parent)?;
    let lock = private_file(&path.with_extension("lock"), false)?;
    lock.lock()?;
    let result = f();
    let _ = lock.unlock();
    result
}

pub(crate) fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Open for reading without following a symbolic link at the final component.
pub(crate) fn open_no_follow(path: &Path) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc_o_nofollow());
    }
    options.open(path).ok()
}

#[cfg(unix)]
fn libc_o_nofollow() -> i32 {
    // O_NOFOLLOW; the value is stable on Linux and the BSDs, and std has no
    // constant for it without pulling in libc.
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x0100
    }
}

/// Write `path` privately and atomically: owner-only file, a temporary file
/// synced and renamed into place, and the directory synced afterwards. The
/// caller holds the jar lock.
pub(crate) fn save_private(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    if is_symlink(path) {
        return Err(std::io::Error::other("target is a symbolic link"));
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let result = (|| {
        let mut file = private_file(&tmp, true)?;
        write(&mut file)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn private_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(truncate)
        .read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc_o_nofollow());
    }
    options.open(path)
}

// ---------------------------------------------------------------------------
// The wasi-http hook that ties it together.

pub struct OriginHooks {
    policy: OriginPolicy,
    cookies: Option<CookieJar>,
    request_policy: Option<Arc<dyn RequestPolicy>>,
    concurrency: Arc<Semaphore>,
    request_body_bytes: usize,
    response_body_bytes: usize,
    on_phase: Option<crate::terminal::PhaseHook>,
}

impl OriginHooks {
    pub fn new(
        policy: OriginPolicy,
        cookies: Option<CookieJar>,
        request_policy: Option<Arc<dyn RequestPolicy>>,
        limits: &Limits,
        on_phase: Option<crate::terminal::PhaseHook>,
    ) -> Self {
        Self {
            policy,
            cookies,
            request_policy,
            concurrency: Arc::new(Semaphore::new(limits.http_concurrency.max(1))),
            request_body_bytes: limits.request_body_bytes,
            response_body_bytes: limits.response_body_bytes,
            on_phase,
        }
    }
}

fn url_of(uri: &http::Uri) -> Option<Url> {
    Url::parse(&uri.to_string()).ok()
}

/// Run the embedder's policy on a request head. Shared with websockets.
pub async fn apply_policy(
    request_policy: Option<&Arc<dyn RequestPolicy>>,
    on_phase: Option<&crate::terminal::PhaseHook>,
    parts: &mut http::request::Parts,
    info: &RequestInfo,
) -> Result<PolicyGuard, Error> {
    let Some(policy) = request_policy else {
        return Ok(None);
    };
    match policy.on_request(parts, info).await {
        Ok(guard) => Ok(guard),
        Err(err) => {
            if let Some(hook) = on_phase {
                hook(Phase::RequestDenied {
                    url: parts.uri.to_string(),
                    reason: err.reason,
                });
            }
            Err(Error::HttpRequestDenied)
        }
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
        if self.policy.decide(request.uri()) == Decision::Deny {
            if let Some(hook) = &self.on_phase {
                hook(Phase::RequestDenied {
                    url: request.uri().to_string(),
                    reason: "origin not allowed".into(),
                });
            }
            return Box::new(async { Err(Error::HttpRequestDenied) });
        }
        let info = RequestInfo {
            kind: RequestKind::Http,
            app_origin: self.policy.app_origin().map(str::to_owned),
            cross_origin: self.policy.is_cross_origin(request.uri()),
        };
        let (mut parts, body) = request.into_parts();
        if let Some(value) = info.app_origin.as_deref().and_then(origin_header_value) {
            parts.headers.insert(ORIGIN, value);
        }
        // Like a browser: the app cannot forge cookies, the jar supplies them.
        parts.headers.remove(COOKIE);
        let url = url_of(&parts.uri);
        let cookies = self.cookies.clone();
        if let (Some(jar), Some(url)) = (&cookies, &url)
            && let Some(value) = jar.request_header(url)
        {
            parts.headers.insert(COOKIE, value);
        }
        let request_policy = self.request_policy.clone();
        let on_phase = self.on_phase.clone();
        let concurrency = self.concurrency.clone();
        let request_body_bytes = self.request_body_bytes;
        let response_body_bytes = self.response_body_bytes;

        Box::new(async move {
            let permit = concurrency
                .acquire_owned()
                .await
                .map_err(|_| Error::HttpRequestDenied)?;
            let guard = apply_policy(
                request_policy.as_ref(),
                on_phase.as_ref(),
                &mut parts,
                &info,
            )
            .await?;
            let body = Limited::new(body, request_body_bytes)
                .map_err(|e| Error::HttpRequestBodySize(None).tap_boxed(e))
                .boxed_unsync();
            let request = http::Request::from_parts(parts, body);
            let (response, io) = default_send_request(request, options).await?;
            if let Some(policy) = &request_policy {
                let (head, body) = response.into_parts();
                policy.on_response(&head, &info).await;
                let response = http::Response::from_parts(head, body);
                return finish(
                    response,
                    io,
                    url,
                    cookies,
                    response_body_bytes,
                    permit,
                    guard,
                );
            }
            finish(
                response,
                io,
                url,
                cookies,
                response_body_bytes,
                permit,
                guard,
            )
        })
    }
}

trait TapBoxed {
    fn tap_boxed(self, _: Box<dyn std::error::Error + Send + Sync>) -> Self;
}

impl TapBoxed for Error {
    fn tap_boxed(self, _: Box<dyn std::error::Error + Send + Sync>) -> Self {
        self
    }
}

#[allow(clippy::type_complexity)]
fn finish<B>(
    mut response: http::Response<B>,
    io: impl Future<Output = Result<(), Error>> + Send + 'static,
    url: Option<Url>,
    cookies: Option<CookieJar>,
    response_body_bytes: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
    guard: PolicyGuard,
) -> Result<
    (
        http::Response<WasiBody>,
        Box<dyn Future<Output = Result<(), Error>> + Send>,
    ),
    Error,
>
where
    B: http_body::Body<Data = bytes::Bytes, Error = Error> + Send + 'static,
{
    if let (Some(jar), Some(url)) = (&cookies, &url) {
        jar.store_response(url, response.headers());
    }
    // The app never sees Set-Cookie, so HttpOnly means what it says.
    response.headers_mut().remove(SET_COOKIE);
    let response = response.map(|body| {
        Limited::new(body, response_body_bytes)
            .map_err(|e| match e.downcast::<Error>() {
                Ok(inner) => *inner,
                Err(_) => Error::HttpResponseBodySize(None),
            })
            .boxed_unsync()
    });
    // Permit and guard live as long as the request does.
    let io = async move {
        let result = io.await;
        drop(guard);
        drop(permit);
        result
    };
    Ok((response, Box::new(io)))
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
        assert!(!policy.is_cross_origin(&uri("http://localhost:3000/api/x")));
        assert!(policy.is_cross_origin(&uri("http://localhost:3001/")));

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
    fn cookie_jar_round_trips_persists_privately_and_refuses_symlinks() {
        let dir = std::env::temp_dir().join(format!("rattery-jar-{}", std::process::id()));
        let path = dir.join("cookies.json");
        let url = Url::parse("http://localhost:3000/api/x").unwrap();

        let jar = CookieJar::at(path.clone()).unwrap();
        assert!(jar.request_header(&url).is_none());
        let mut headers = http::HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("session=abc; Path=/; HttpOnly"),
        );
        headers.append(SET_COOKIE, HeaderValue::from_static("theme=dark; Path=/"));
        jar.store_response(&url, &headers);
        let sent = jar.request_header(&url).unwrap();
        let sent = sent.to_str().unwrap();
        assert!(
            sent.contains("session=abc") && sent.contains("theme=dark"),
            "{sent}"
        );
        assert!(
            jar.request_header(&Url::parse("http://other:3000/").unwrap())
                .is_none()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        // A new jar at the same path sees the saved cookies.
        let reloaded = CookieJar::at(path.clone()).unwrap();
        assert!(reloaded.request_header(&url).is_some());

        // A symlink in place of the jar is refused.
        #[cfg(unix)]
        {
            let link = dir.join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(CookieJar::at(link).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cookie_quotas() {
        let jar = CookieJar::ephemeral();
        let url = Url::parse("http://localhost:3000/").unwrap();
        let mut headers = http::HeaderMap::new();
        let big = format!("big={}; Path=/", "x".repeat(DEFAULT_COOKIE_BYTES));
        headers.append(SET_COOKIE, HeaderValue::from_str(&big).unwrap());
        jar.store_response(&url, &headers);
        assert!(
            jar.request_header(&url).is_none(),
            "oversized cookie must be ignored"
        );

        for i in 0..(DEFAULT_COOKIES_PER_HOST + 10) {
            let mut headers = http::HeaderMap::new();
            headers.append(
                SET_COOKIE,
                HeaderValue::from_str(&format!("c{i}=v; Path=/")).unwrap(),
            );
            jar.store_response(&url, &headers);
        }
        let sent = jar.request_header(&url).unwrap();
        assert_eq!(
            sent.to_str().unwrap().split("; ").count(),
            DEFAULT_COOKIES_PER_HOST
        );
        assert_eq!(
            jar.store.lock().unwrap().iter_any().count(),
            DEFAULT_COOKIES_PER_HOST
        );
    }

    #[test]
    fn cookie_quota_counts_every_path() {
        let url = Url::parse("http://localhost:3000/").unwrap();

        // Distinct names on distinct paths.
        let jar = CookieJar::ephemeral();
        for i in 0..(DEFAULT_COOKIES_PER_HOST + 10) {
            let mut headers = http::HeaderMap::new();
            headers.append(
                SET_COOKIE,
                HeaderValue::from_str(&format!("c{i}=v; Path=/p{i}")).unwrap(),
            );
            jar.store_response(&url, &headers);
        }
        assert_eq!(
            jar.store.lock().unwrap().iter_any().count(),
            DEFAULT_COOKIES_PER_HOST
        );

        // One name across many paths does not escape the quota either.
        let jar = CookieJar::ephemeral();
        for i in 0..(DEFAULT_COOKIES_PER_HOST + 10) {
            let mut headers = http::HeaderMap::new();
            headers.append(
                SET_COOKIE,
                HeaderValue::from_str(&format!("same=v{i}; Path=/p{i}")).unwrap(),
            );
            jar.store_response(&url, &headers);
        }
        assert_eq!(
            jar.store.lock().unwrap().iter_any().count(),
            DEFAULT_COOKIES_PER_HOST
        );

        // Replacing an existing cookie (same name, domain, and path) still
        // works at quota, and does not add one.
        let mut headers = http::HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("same=new; Path=/p0"));
        jar.store_response(&url, &headers);
        assert_eq!(
            jar.store.lock().unwrap().iter_any().count(),
            DEFAULT_COOKIES_PER_HOST
        );
        let sent = jar
            .request_header(&Url::parse("http://localhost:3000/p0/x").unwrap())
            .unwrap();
        assert!(sent.to_str().unwrap().contains("same=new"));
    }

    #[test]
    fn rejects_bad_origins() {
        assert!(OriginPolicy::new(Some("ftp://x"), &[], false).is_err());
        assert!(OriginPolicy::new(Some("not a url"), &[], false).is_err());
    }
}
