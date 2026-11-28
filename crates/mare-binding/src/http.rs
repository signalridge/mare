//! `mare.http`: requests with streaming bodies and SSE iteration; a one-shot local
//! callback listener for OAuth logins.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use mlua::{Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::task::opt_token;
use crate::sse::{self, Caps as SseCaps};
use mare_shared::lua::{json_to_lua, lua_to_json, rt_err};

#[path = "http_authorized.rs"]
mod authorized;
#[path = "http_listener.rs"]
mod listener;
#[path = "http_response.rs"]
mod response;

use authorized::{ResolvedProxy, ShimGuard, normalize_explicit_proxy, send_authorized};
use listener::listen_once;
#[cfg(test)]
use response::utf8_guarded;
use response::{ByteStream, SseStream, TRANSPORT, request};

pub struct Response {
    status: u16,
    url: String,
    headers: Vec<(String, String)>,
    body: AsyncMutex<Option<ByteStream>>,
    sse: AsyncMutex<Option<SseStream>>,
    close_token: CancellationToken,
    /// set by the UTF-8 guard when the SSE body can't be valid UTF-8
    bad_utf8: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// caps on one SSE line and on one event's accumulated bytes
    caps: SseCaps,
    /// idle watchdog: no bytes at all for this long
    /// fails the read. Measured on raw bytes, so SSE heartbeat comments count as activity.
    idle: Option<Duration>,
    deadline: Option<tokio::time::Instant>,
    last_activity: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
}

async fn cancellable<T>(tok: &Option<CancellationToken>, f: impl std::future::Future<Output = T>) -> mlua::Result<T> {
    match tok {
        Some(t) => tokio::select! {
            v = f => Ok(v),
            _ = t.cancelled() => Err(rt_err("aborted")),
        },
        None => Ok(f.await),
    }
}

#[cfg(any(debug_assertions, test))]
static TEST_OFFLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(cfg!(test));

/// Enable the test-only offline guard (blocks non-loopback HTTP/WS targets in test builds).
pub fn enable_test_offline() {
    #[cfg(any(debug_assertions, test))]
    TEST_OFFLINE.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(any(debug_assertions, test))]
pub(crate) fn offline_guard_active() -> bool {
    if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() == Ok("1") {
        return false;
    }
    TEST_OFFLINE.load(std::sync::atomic::Ordering::Relaxed) || std::env::var("MARE_TEST_OFFLINE").as_deref() == Ok("1")
}

#[cfg(not(any(debug_assertions, test)))]
pub(crate) fn offline_guard_active() -> bool {
    false
}

/// Whether `host` is a loopback name (`localhost`, `*.localhost`) or loopback IP (`127.0.0.0/8`, `::1`).
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        Err(_) => false,
    }
}

fn is_loopback_url(url: &url::Url) -> bool {
    url.host_str().is_none_or(is_loopback_host)
}

/// Refuse non-loopback URLs when the offline test guard is active.
pub(crate) fn check_offline_url(raw: &str) -> Result<(), String> {
    if !offline_guard_active() {
        return Ok(());
    }
    if let Ok(u) = url::Url::parse(raw)
        && let Some(host) = u.host_str()
        && !is_loopback_host(host)
    {
        return Err(format!("offline test guard: {host} is not a loopback address; tests stay off the network"));
    }
    Ok(())
}

/// Refuse non-loopback proxy URLs (including bare `host:port` forms) when the offline test guard is active.
pub(crate) fn check_offline_proxy(raw: &str) -> Result<(), String> {
    if !offline_guard_active() {
        return Ok(());
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let normalized = if trimmed.contains("://") { trimmed.to_string() } else { format!("http://{trimmed}") };
    check_offline_url(&normalized)
}

fn cidr_matches(ip: std::net::IpAddr, pattern: &str) -> bool {
    let Some((net_str, prefix_str)) = pattern.split_once('/') else { return false };
    let (Ok(net), Ok(prefix)) = (net_str.parse::<std::net::IpAddr>(), prefix_str.parse::<u32>()) else {
        return false;
    };
    match (ip, net) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(b)) if prefix <= 32 => {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(b)) if prefix <= 128 => {
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
            (u128::from(a) & mask) == (u128::from(b) & mask)
        }
        _ => false,
    }
}

fn no_proxy_matches(url: &url::Url, env: &dyn Fn(&str) -> Option<String>) -> bool {
    let Some(raw) = env("NO_PROXY").or_else(|| env("no_proxy")) else { return false };
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let host_ip = host.parse::<std::net::IpAddr>().ok();
    let port = url.port_or_known_default();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if entry == "*" {
            return true;
        }
        let (pat_host, pat_port) = if let Some(rest) = entry.strip_prefix('[') {
            let Some((h, after)) = rest.split_once(']') else { continue };
            let p = after.strip_prefix(':').and_then(|s| s.parse::<u16>().ok());
            if !after.is_empty() && p.is_none() {
                continue;
            }
            (h, p)
        } else if entry.matches(':').count() == 1 {
            let Some((h, p_str)) = entry.split_once(':') else { continue };
            let Ok(p) = p_str.parse::<u16>() else { continue };
            (h, Some(p))
        } else {
            (entry, None)
        };
        if let Some(pp) = pat_port
            && Some(pp) != port
        {
            continue;
        }
        let pat = pat_host.to_ascii_lowercase();
        if let Some(ip) = host_ip
            && cidr_matches(ip, &pat)
        {
            return true;
        }
        if let Some(suffix) = pat.strip_prefix('.') {
            if host == suffix || host.ends_with(&pat) {
                return true;
            }
        } else if host == pat || host.ends_with(&format!(".{pat}")) {
            return true;
        }
    }
    false
}

pub(crate) fn check_offline_proxy_for_url_with(
    raw_url: &str,
    explicit_proxy: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    if !offline_guard_active() {
        return Ok(());
    }
    let parsed = url::Url::parse(raw_url).ok();
    if parsed.as_ref().is_some_and(|u| no_proxy_matches(u, env)) {
        return Ok(());
    }
    if let Some(proxy) = explicit_proxy {
        check_offline_proxy(proxy)?;
    }
    let vars: &[&str] = match parsed.as_ref().map(|u| u.scheme()) {
        Some("http") => &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"],
        Some("https") => &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"],
        _ => crate::proxyenv::PROXY_VARS,
    };
    for k in vars {
        if let Some(v) = env(k) {
            check_offline_proxy(&v)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn check_offline_proxy_env_with(env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    if !offline_guard_active() {
        return Ok(());
    }
    for k in crate::proxyenv::PROXY_VARS {
        if let Some(v) = env(k) {
            check_offline_proxy(&v)?;
        }
    }
    Ok(())
}

struct OfflineDnsResolver;

impl reqwest::dns::Resolve for OfflineDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let offline = offline_guard_active();
            if offline && !is_loopback_host(&host) {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("offline test guard: {host} is not a loopback address; tests stay off the network"),
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .filter(|sa| !offline || sa.ip().is_loopback())
                .collect();
            if offline && addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("offline test guard: {host} resolved to no loopback address; tests stay off the network"),
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Every `mare.http` client starts here: user agent, connect timeout, the proxy variables
/// reqwest reads itself (`HTTP(S)_PROXY`, `ALL_PROXY`, `NO_PROXY` via reqwest's NoProxy) and a
/// custom CA bundle.
pub(crate) fn base_builder_with(env: &dyn Fn(&str) -> Option<String>) -> Result<reqwest::ClientBuilder, String> {
    crate::proxyenv::check_supported(env)?;
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("mare/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .dns_resolver(std::sync::Arc::new(OfflineDnsResolver))
        .redirect(redirect_policy(Redirect::Follow, MAX_REDIRECTS));
    for cert in custom_ca()? {
        b = b.add_root_certificate(cert);
    }
    Ok(b)
}

pub(crate) fn base_builder() -> Result<reqwest::ClientBuilder, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    base_builder_with(&env)
}

/// The custom CA certificates (`crate::cacerts`: `MARE_CA_CERTIFICATE`, else `SSL_CERT_FILE` +
/// `SSL_CERT_DIR`) as reqwest roots.
fn custom_ca() -> Result<Vec<reqwest::Certificate>, String> {
    crate::cacerts::load_from_env()?
        .iter()
        .map(|der| reqwest::Certificate::from_der(der.as_ref()).map_err(|e| format!("custom CA certificate: {e}")))
        .collect()
}

/// Build a client with an error a user can act on: reqwest only says "builder error"; the cause
/// (e.g. a Linux image without any CA certificates, found by the Harbor evals) is in the chain.
pub(crate) fn build_client(b: reqwest::ClientBuilder) -> Result<reqwest::Client, String> {
    b.build().map_err(|e| {
        let mut msg = String::from("could not set up the HTTP client");
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
        while let Some(c) = src {
            msg.push_str(": ");
            msg.push_str(&c.to_string());
            src = c.source();
        }
        if !crate::cacerts::custom_ca_configured() {
            msg.push_str(crate::cacerts::no_roots_hint());
        }
        msg
    })
}

fn client() -> mlua::Result<&'static reqwest::Client> {
    static C: std::sync::OnceLock<Result<reqwest::Client, String>> = std::sync::OnceLock::new();
    C.get_or_init(|| base_builder().and_then(build_client)).as_ref().map_err(|e| rt_err(e.clone()))
}

/// Shared offline-guarded HTTP client (user agent, custom CAs, proxy/DNS policy) for internal callers.
pub(crate) fn shared_client() -> anyhow::Result<&'static reqwest::Client> {
    client().map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestClient(reqwest::Client);

#[cfg(test)]
pub(crate) struct TestRequestBuilder {
    inner: reqwest::RequestBuilder,
    offline_err: Option<String>,
}

#[cfg(test)]
impl TestClient {
    pub(crate) fn request(&self, method: reqwest::Method, url: impl AsRef<str>) -> TestRequestBuilder {
        let raw = url.as_ref();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let offline_err = check_offline_url(raw).and_then(|()| check_offline_proxy_for_url_with(raw, None, &env)).err();
        TestRequestBuilder { inner: self.0.request(method, raw), offline_err }
    }

    pub(crate) fn get(&self, url: impl AsRef<str>) -> TestRequestBuilder {
        self.request(reqwest::Method::GET, url)
    }

    pub(crate) fn post(&self, url: impl AsRef<str>) -> TestRequestBuilder {
        self.request(reqwest::Method::POST, url)
    }
}

#[cfg(test)]
impl TestRequestBuilder {
    pub(crate) fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        reqwest::header::HeaderName: TryFrom<K>,
        <reqwest::header::HeaderName as TryFrom<K>>::Error: Into<::http::Error>,
        reqwest::header::HeaderValue: TryFrom<V>,
        <reqwest::header::HeaderValue as TryFrom<V>>::Error: Into<::http::Error>,
    {
        self.inner = self.inner.header(key, value);
        self
    }

    pub(crate) fn body<T: Into<reqwest::Body>>(mut self, body: T) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    pub(crate) async fn send(self) -> Result<reqwest::Response, String> {
        if let Some(err) = self.offline_err {
            return Err(err);
        }
        self.inner.send().await.map_err(|e| {
            let mut msg = e.to_string();
            let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
            while let Some(c) = src {
                msg.push_str(": ");
                msg.push_str(&c.to_string());
                src = c.source();
            }
            msg
        })
    }
}

#[cfg(test)]
pub(crate) fn test_client() -> TestClient {
    TestClient(build_client(base_builder().unwrap()).unwrap())
}

/// Like [`test_client`] but redirects are returned, not followed (the offline guard still applies).
#[cfg(test)]
pub(crate) fn test_client_no_redirect() -> TestClient {
    TestClient(
        build_client(base_builder().unwrap().redirect(redirect_policy(Redirect::Manual, MAX_REDIRECTS))).unwrap(),
    )
}

#[cfg(test)]
pub(crate) async fn test_get(url: impl AsRef<str>) -> Result<reqwest::Response, String> {
    test_client().get(url).send().await
}

/// A client that never reuses pooled connections: retries after a transport failure must not get
/// the broken (HTTP/2) connection back from the pool.
fn fresh_client() -> mlua::Result<reqwest::Client> {
    build_client(base_builder().map_err(rt_err)?.pool_max_idle_per_host(0)).map_err(rt_err)
}

/// `Retry-After` (seconds or an HTTP date) and `retry-after-ms`, in milliseconds. Past dates give
/// 0; unparseable values give None.
pub fn retry_after_ms(headers: &[(String, String)]) -> Option<u64> {
    let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.trim());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(ms as u64);
    }
    let v = get("retry-after")?;
    if let Ok(secs) = v.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then_some((secs * 1000.0) as u64);
    }
    let when = jiff::fmt::rfc2822::parse(v).ok()?;
    let ms = when.timestamp().as_millisecond() - jiff::Timestamp::now().as_millisecond();
    Some(ms.max(0) as u64)
}

/// The parts of a request that every hop re-sends (redirects are followed by hand when the
/// network is restricted, so each hop can be re-authorized).
struct Parts {
    method: reqwest::Method,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    timeout: Option<Duration>,
    /// waiting for the response headers
    header_timeout: Option<Duration>,
    /// a new connection, not one from the pool
    fresh: bool,
    /// a provider's own proxy and connect timeout (`mare.provider{ proxy, connect_timeout_ms }`,
    /// ); unset = the environment's proxy settings and the default timeout
    route: Route,
    redirect: Redirect,
    max_redirects: usize,
    block_private: bool,
}

/// Explicit redirect controls only narrow the core network authorization policy.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
enum Redirect {
    #[default]
    Follow,
    Manual,
    SameOrigin,
}

pub(crate) fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    redirect_policy(Redirect::SameOrigin, MAX_REDIRECTS)
}

fn redirect_policy(mode: Redirect, max: usize) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if offline_guard_active() && !is_loopback_url(attempt.url()) {
            let msg = format!(
                "offline test guard: redirect to {} is not a loopback address; tests stay off the network",
                attempt.url().host_str().unwrap_or("<unknown>")
            );
            return attempt.error(msg);
        }
        if mode == Redirect::Manual {
            return attempt.stop();
        }
        if attempt.previous().len() > max {
            return attempt.error("too many redirects");
        }
        if mode == Redirect::SameOrigin
            && attempt.previous().first().is_some_and(|u| u.origin() != attempt.url().origin())
        {
            return attempt.error("cross-origin redirect blocked");
        }
        attempt.follow()
    })
}

fn strip_userinfo(u: &mut url::Url) {
    let _ = u.set_username("");
    let _ = u.set_password(None);
}

#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Route {
    proxy: Option<String>,
    connect_timeout_ms: Option<u64>,
}

fn apply_route(mut b: reqwest::ClientBuilder, r: &Route, fresh: bool) -> mlua::Result<reqwest::ClientBuilder> {
    if let Some(p) = &r.proxy {
        let lower = p.trim().to_ascii_lowercase();
        if lower.starts_with("socks") {
            let safe = mare_shared::redact::redact_str(p);
            return Err(rt_err(format!("proxy {safe}: SOCKS proxies are not supported — use an HTTP or HTTPS proxy")));
        }
        let proxy = reqwest::Proxy::all(p.as_str())
            .map_err(|e| {
                let safe_p = mare_shared::redact::redact_str(p);
                let safe_e = mare_shared::redact::redact_str(&e.to_string());
                rt_err(format!("invalid proxy {safe_p}: {safe_e}"))
            })?
            .no_proxy(reqwest::NoProxy::from_env());
        b = b.proxy(proxy);
    }
    if let Some(ms) = r.connect_timeout_ms {
        b = b.connect_timeout(Duration::from_millis(ms));
    }
    if fresh {
        b = b.pool_max_idle_per_host(0);
    }
    Ok(b)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    route: Route,
    redirect: Redirect,
    max_redirects: usize,
}

/// A client for a non-default route or redirect policy: an explicit proxy (with the environment's
/// `NO_PROXY` still applied through reqwest's own `NoProxy`, as for the default client), connect
/// timeout, and redirect policy. Kept per `(route, redirect, max_redirects)` so pooled connections
/// and warmup are reused.
fn route_client(r: &Route, redirect: Redirect, max_redirects: usize, fresh: bool) -> mlua::Result<reqwest::Client> {
    static CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<ClientKey, reqwest::Client>>> =
        std::sync::LazyLock::new(Default::default);
    let key = ClientKey { route: r.clone(), redirect, max_redirects };
    if !fresh && let Some(c) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(c.clone());
    }
    let mut b = base_builder().map_err(rt_err)?;
    if redirect != Redirect::Follow || max_redirects != MAX_REDIRECTS {
        b = b.redirect(redirect_policy(redirect, max_redirects));
    }
    let b = apply_route(b, r, fresh)?;
    let c = build_client(b).map_err(rt_err)?;
    if !fresh {
        CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, c.clone());
    }
    Ok(c)
}

/// Send under the cancel token and the header timeout; failures are `transport:` errors.
async fn send(
    rb: reqwest::RequestBuilder,
    p: &Parts,
    tok: &Option<CancellationToken>,
) -> mlua::Result<reqwest::Response> {
    let fut = rb.send();
    let res = match p.header_timeout {
        Some(t) => match tokio::time::timeout(t, cancellable(tok, fut)).await {
            Ok(r) => r?,
            Err(_) => return Err(rt_err(format!("{TRANSPORT}no response headers within {}s", t.as_secs()))),
        },
        None => cancellable(tok, fut).await?,
    };
    res.map_err(|e| {
        let mut detail = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(c) = src {
            detail.push_str(": ");
            detail.push_str(&c.to_string());
            src = c.source();
        }
        let safe = mare_shared::redact::redact_str(&detail);
        rt_err(if e.is_timeout() {
            format!("{TRANSPORT}request timed out: {safe}")
        } else {
            format!("{TRANSPORT}request failed: {safe}")
        })
    })
}

fn parts(lua: &Lua, opts: &Table) -> mlua::Result<Parts> {
    let method: String = opts.get::<Option<String>>("method")?.unwrap_or_else(|| "GET".into());
    let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).map_err(rt_err)?;
    let mut headers = vec![];
    if let Some(h) = opts.get::<Option<Table>>("headers")? {
        for p in h.pairs::<String, String>() {
            headers.push(p?);
        }
    }
    let mut body = None;
    if let Some(v) = opts.get::<Option<Value>>("json")? {
        let j = lua_to_json(lua, v)?;
        headers.push(("content-type".into(), "application/json".into()));
        body = Some(Bytes::from(serde_json::to_vec(&j).map_err(rt_err)?));
    } else if let Some(f) = opts.get::<Option<Table>>("form")? {
        let mut pairs = vec![];
        for p in f.pairs::<String, String>() {
            pairs.push(p?);
        }
        headers.push(("content-type".into(), "application/x-www-form-urlencoded".into()));
        body = Some(Bytes::from(
            url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish().into_bytes(),
        ));
    } else if let Some(b) = opts.get::<Option<mlua::LuaString>>("body")? {
        body = Some(Bytes::copy_from_slice(&b.as_bytes()));
    }
    let timeout = opts.get::<Option<f64>>("timeout_ms")?.map(|ms| Duration::from_millis(ms as u64));
    let header_timeout = opts.get::<Option<f64>>("header_timeout_ms")?.map(|ms| Duration::from_millis(ms as u64));
    let fresh = opts.get::<Option<bool>>("fresh")?.unwrap_or(false);
    let proxy = match opts.get::<Option<String>>("proxy")? {
        Some(p) if !p.trim().is_empty() => Some(normalize_explicit_proxy(&p)?),
        _ => None,
    };
    let route = Route {
        proxy,
        connect_timeout_ms: opts.get::<Option<f64>>("connect_timeout_ms")?.filter(|ms| *ms > 0.0).map(|ms| ms as u64),
    };
    let redirect = match opts.get::<Option<String>>("redirect")?.as_deref().unwrap_or("follow") {
        "follow" => Redirect::Follow,
        "manual" => Redirect::Manual,
        "same-origin" => Redirect::SameOrigin,
        _ => return Err(rt_err("redirect must be follow, manual, or same-origin")),
    };
    let max_redirects = opts.get::<Option<usize>>("max_redirects")?.unwrap_or(MAX_REDIRECTS);
    let block_private = opts.get::<Option<bool>>("block_private")?.unwrap_or(false);
    Ok(Parts { method, headers, body, timeout, header_timeout, fresh, route, redirect, max_redirects, block_private })
}

fn builder(
    c: &reqwest::Client,
    p: &Parts,
    url: &str,
    method: reqwest::Method,
    body: bool,
    strip_credentials: bool,
) -> reqwest::RequestBuilder {
    let mut rb = c.request(method, url);
    for (k, v) in &p.headers {
        if strip_credentials
            && matches!(k.to_ascii_lowercase().as_str(), "authorization" | "cookie" | "proxy-authorization")
        {
            continue;
        }
        rb = rb.header(k, v);
    }
    if body && let Some(b) = &p.body {
        rb = rb.body(b.clone());
    }
    if let Some(t) = p.timeout {
        rb = rb.timeout(t);
    }
    rb
}

const MAX_REDIRECTS: usize = 10;

/// Resolve the proxy for `url` once so authorization and the actual connection share the exact same
/// normalized routing decision. Fails closed if routing cannot be determined.
fn resolve_proxy_for_url(url: &url::Url, explicit_proxy: Option<&str>) -> Result<Option<ResolvedProxy>, String> {
    let uri = url.as_str().parse::<http::Uri>().map_err(|e| format!("invalid request URI for proxy routing: {e}"))?;
    let intercept = if let Some(p) = explicit_proxy {
        let canonical = normalize_explicit_proxy(p).map_err(|e| e.to_string())?;
        let no_proxy = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).unwrap_or_default();
        let matcher_no_bypass =
            hyper_util::client::proxy::matcher::Matcher::builder().http(&canonical).https(&canonical).build();
        if matcher_no_bypass.intercept(&uri).is_none() {
            return Err(format!(
                "unable to determine proxy route for explicit proxy {}",
                mare_shared::redact::redact_str(&canonical)
            ));
        }
        let matcher = hyper_util::client::proxy::matcher::Matcher::builder()
            .http(&canonical)
            .https(&canonical)
            .no(no_proxy)
            .build();
        matcher.intercept(&uri)
    } else {
        hyper_util::client::proxy::matcher::Matcher::from_env().intercept(&uri)
    };
    intercept.map(ResolvedProxy::from_intercept).transpose()
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // inside a tool call with a restricted network, every hop is authorized and pinned by the
    // shared network layer; outside tool calls / with an open network the
    // request goes out as asked. The context is captured before the first await.
    t.set(
        "request",
        lua.create_async_function(|lua, opts: Table| {
            let barrier = crate::exec_gate::check_unpoisoned(&lua);
            let authz = crate::exec_ctx::active(&lua);
            async move {
                barrier?;
                request(lua, opts, authz?).await
            }
        })?,
    )?;
    t.set(
        "listen_once",
        lua.create_async_function(|lua, opts: Table| {
            let pre = crate::exec_ctx::check(&lua, "network", "localhost");
            async move {
                pre?;
                listen_once(lua, opts).await
            }
        })?,
    )?;
    // retry_after(headers) -> ms | nil: `retry-after-ms`, `Retry-After` seconds or HTTP date
    t.set(
        "retry_after",
        lua.create_function(|_, h: Table| {
            let pairs: Vec<(String, String)> = h.pairs::<String, String>().collect::<mlua::Result<_>>()?;
            Ok(retry_after_ms(&pairs))
        })?,
    )?;
    t.set(
        "url_encode",
        lua.create_function(|_, s: String| Ok(url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>()))?,
    )?;
    // build_url(base, {k=v}) -> string
    t.set(
        "build_url",
        lua.create_function(|_, (base, q): (String, Option<Table>)| {
            let mut u = url::Url::parse(&base).map_err(rt_err)?;
            if let Some(q) = q {
                let mut pairs: Vec<(String, String)> = q.pairs::<String, String>().collect::<mlua::Result<_>>()?;
                pairs.sort();
                u.query_pairs_mut().extend_pairs(pairs);
            }
            Ok(u.to_string())
        })?,
    )?;
    crate::ws::install(lua, &t)?;
    crate::http_serve::install(lua, &t)?;
    mare.set("http", t)?;
    let _ = lua.null();
    let _ = LuaSerdeExt::null(lua);
    Ok(())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
