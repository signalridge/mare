//! Host-level webserver route & static asset registry.
//!
//! Owned at the [`crate::HostManager`] level (shared across all [`crate::Host`] sessions) so
//! HTTP routes and `web = { dir }` static mounts outlive any single session while still attributing
//! ownership cleanly to each session/plugin/disposer.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use mare_kernel::{ErrorValue, Kernel, ServiceCallError};

const MAX_ROUTES: usize = 256;
const MAX_ASSETS: usize = 64;
const MAX_STATIC_FILE_BYTES: u64 = 32 * 1024 * 1024;
const RESERVED_PATHS: &[&str] = &["/readyz", "/healthz", "/rpc"];
const ALLOWED_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "ANY"];

/// Public listener status snapshot returned by `webserver.get`.
#[derive(Clone, Debug, Default)]
pub struct WebListenerInfo {
    pub running: bool,
    pub url: Option<String>,
    pub ws_url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: bool,
    pub trusted_hosts: Vec<String>,
}

/// Incoming HTTP request metadata forwarded to a registered route handler.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WebRouteRequest {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

fn deserialize_string_headers<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<BTreeMap<String, serde_json::Value>> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw.unwrap_or_default().into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect())
}

fn deserialize_body_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_string())
}

/// HTTP response returned by a registered route handler.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct WebRouteResponse {
    pub status: u16,
    #[serde(default, deserialize_with = "deserialize_string_headers")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_body_or_default")]
    pub body: String,
}

type BoxRouteFuture = Pin<Box<dyn Future<Output = Result<WebRouteResponse, String>>>>;
pub type RouteCallback = Rc<dyn Fn(WebRouteRequest) -> BoxRouteFuture>;

#[derive(Clone)]
pub struct RegisteredRoute {
    pub route_id: String,
    pub method: String,
    pub path: String,
    pub host_id: u64,
    pub owner_session_id: Rc<RefCell<Option<String>>>,
    pub plugin: Option<String>,
    pub handler: RouteCallback,
}

#[derive(Clone, Debug)]
pub struct RegisteredAsset {
    pub asset_id: String,
    pub prefix: String,
    pub canonical_dir: PathBuf,
    pub raw_dir: String,
    pub index: String,
    pub fallback: Option<String>,
    pub owners: BTreeSet<u64>,
    pub owner_session_id: Rc<RefCell<Option<String>>>,
    pub plugin: Option<String>,
    pub declarative: bool,
}

/// Outcome of resolving a static asset request against registered mounts.
#[derive(Debug)]
pub enum AssetResolve {
    NotMatched,
    MethodNotAllowed,
    BadRequest(&'static str),
    NotFound,
    TooLarge,
    File { content_type: &'static str, bytes: Vec<u8>, head_only: bool },
}

/// Parameters for [`WebRegistry::register_assets`].
pub struct RegisterAssetsArgs<'a> {
    pub prefix: &'a str,
    pub dir: &'a Path,
    pub root_guard: Option<&'a Path>,
    pub index: Option<&'a str>,
    pub fallback: Option<&'a str>,
    pub host_id: u64,
    pub owner_session_id: Rc<RefCell<Option<String>>>,
    pub plugin: Option<String>,
    pub declarative: bool,
}

/// Host-wide HTTP route & static asset registry shared across sessions in one [`crate::HostManager`].
pub struct WebRegistry {
    listener: RefCell<WebListenerInfo>,
    routes: RefCell<Vec<RegisteredRoute>>,
    assets: RefCell<Vec<RegisteredAsset>>,
    next_seq: Cell<u64>,
}

impl Default for WebRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WebRegistry {
    pub fn new() -> Self {
        Self {
            listener: RefCell::new(WebListenerInfo::default()),
            routes: RefCell::new(Vec::new()),
            assets: RefCell::new(Vec::new()),
            next_seq: Cell::new(1),
        }
    }

    fn alloc_id(&self, prefix: &str) -> String {
        let n = self.next_seq.get();
        self.next_seq.set(n.wrapping_add(1));
        format!("{prefix}-{n}")
    }

    /// Update the active listener status reported by `webserver.get`.
    pub fn set_listener(&self, info: WebListenerInfo) {
        *self.listener.borrow_mut() = info;
    }

    /// Snapshot of current listener status.
    pub fn listener(&self) -> WebListenerInfo {
        self.listener.borrow().clone()
    }

    /// Remove all routes and decrement asset ownership for a shutting-down host instance.
    pub fn remove_host(&self, host_id: u64) {
        self.routes.borrow_mut().retain(|r| r.host_id != host_id);
        let mut assets = self.assets.borrow_mut();
        for a in assets.iter_mut() {
            a.owners.remove(&host_id);
        }
        assets.retain(|a| !a.owners.is_empty());
    }

    pub fn list_routes(&self) -> Vec<serde_json::Value> {
        self.routes
            .borrow()
            .iter()
            .map(|r| {
                let mut obj = serde_json::json!({
                    "route_id": r.route_id,
                    "method": r.method,
                    "path": r.path,
                });
                if let Some(sid) = r.owner_session_id.borrow().as_ref() {
                    obj["owner_session_id"] = sid.clone().into();
                }
                if let Some(p) = &r.plugin {
                    obj["plugin"] = p.clone().into();
                }
                obj
            })
            .collect()
    }

    pub fn list_assets(&self) -> Vec<serde_json::Value> {
        self.assets
            .borrow()
            .iter()
            .map(|a| {
                let mut obj = serde_json::json!({
                    "asset_id": a.asset_id,
                    "prefix": a.prefix,
                    "dir": a.raw_dir,
                    "index": a.index,
                });
                if let Some(fb) = &a.fallback {
                    obj["fallback"] = fb.clone().into();
                }
                if let Some(p) = &a.plugin {
                    obj["plugin"] = p.clone().into();
                }
                obj
            })
            .collect()
    }

    pub fn register_route(
        &self,
        method: &str,
        path: &str,
        host_id: u64,
        owner_session_id: Rc<RefCell<Option<String>>>,
        plugin: Option<String>,
        handler: RouteCallback,
    ) -> Result<String, ServiceCallError> {
        let method = normalize_route_method(method)?;
        let path = normalize_route_path(path)?;
        let mut routes = self.routes.borrow_mut();
        if routes.len() >= MAX_ROUTES {
            return Err(service_err("route_limit_reached", "maximum number of registered HTTP routes reached"));
        }
        if routes.iter().any(|r| r.path == path && (r.method == method || r.method == "ANY" || method == "ANY")) {
            return Err(service_err("route_conflict", format!("route already registered for {method} {path}")));
        }
        let route_id = self.alloc_id("route");
        routes.push(RegisteredRoute {
            route_id: route_id.clone(),
            method,
            path,
            host_id,
            owner_session_id,
            plugin,
            handler,
        });
        Ok(route_id)
    }

    pub fn unregister_route(&self, route_id: &str) -> bool {
        let mut routes = self.routes.borrow_mut();
        let before = routes.len();
        routes.retain(|r| r.route_id != route_id);
        routes.len() != before
    }

    /// Register a static asset directory mount under `args.prefix`.
    pub fn register_assets(&self, args: RegisterAssetsArgs<'_>) -> Result<String, ServiceCallError> {
        let prefix = normalize_asset_prefix(args.prefix)?;
        let canonical_dir = canonicalize_asset_dir(args.dir, args.root_guard)?;
        let index = normalize_rel_file(args.index.unwrap_or("index.html"))
            .ok_or_else(|| service_err("invalid_asset_dir", "invalid index file name"))?;
        let fallback = match args.fallback {
            Some(fb) => Some(
                normalize_rel_file(fb).ok_or_else(|| service_err("invalid_asset_dir", "invalid fallback file name"))?,
            ),
            None => None,
        };
        let mut assets = self.assets.borrow_mut();
        if args.declarative
            && let Some(existing) = assets.iter_mut().find(|a| {
                a.declarative
                    && a.prefix == prefix
                    && a.canonical_dir == canonical_dir
                    && a.index == index
                    && a.fallback == fallback
                    && a.plugin == args.plugin
            })
        {
            existing.owners.insert(args.host_id);
            return Ok(existing.asset_id.clone());
        }
        if assets.len() >= MAX_ASSETS {
            return Err(service_err("invalid_asset_dir", "maximum number of asset mounts reached"));
        }
        if assets.iter().any(|a| a.prefix == prefix) {
            return Err(service_err("asset_conflict", format!("asset mount already exists for prefix `{prefix}`")));
        }
        let asset_id = self.alloc_id("asset");
        assets.push(RegisteredAsset {
            asset_id: asset_id.clone(),
            prefix,
            canonical_dir,
            raw_dir: args.dir.display().to_string(),
            index,
            fallback,
            owners: BTreeSet::from([args.host_id]),
            owner_session_id: args.owner_session_id,
            plugin: args.plugin,
            declarative: args.declarative,
        });
        Ok(asset_id)
    }

    pub fn unregister_asset(&self, asset_id: &str) -> bool {
        let mut assets = self.assets.borrow_mut();
        let before = assets.len();
        assets.retain(|a| a.asset_id != asset_id);
        assets.len() != before
    }

    /// Find a dynamic route handler matching `(method, path)` (exact match wins over `ANY`).
    pub fn match_route(&self, method: &str, path: &str) -> Option<RouteCallback> {
        let routes = self.routes.borrow();
        let upper = method.to_ascii_uppercase();
        routes
            .iter()
            .find(|r| r.path == path && r.method == upper)
            .or_else(|| routes.iter().find(|r| r.path == path && r.method == "ANY"))
            .map(|r| r.handler.clone())
    }

    /// Synchronize declarative `web = { dir }` mounts for one host from active plugin definitions and
    /// installed `mare-plugin.toml` manifests.
    pub fn sync_declarative_assets(
        &self,
        host_id: u64,
        session_id: Rc<RefCell<Option<String>>>,
        kernel: &Kernel,
        config: &serde_json::Value,
        include_packages: bool,
    ) {
        {
            let mut assets = self.assets.borrow_mut();
            for a in assets.iter_mut() {
                if a.declarative {
                    a.owners.remove(&host_id);
                }
            }
            assets.retain(|a| !(a.declarative && a.owners.is_empty()));
        }
        let active: BTreeSet<String> =
            kernel.plugin_tree().unwrap_or_default().into_iter().filter(|r| r.active()).map(|r| r.id).collect();

        for def in kernel.plugin_defs() {
            if !active.contains(&def.name) {
                continue;
            }
            let Some(web) = &def.web else { continue };
            let Some(src_path) = def.source.path.as_deref() else { continue };
            let base_dir = if src_path.is_dir() {
                src_path
            } else if let Some(p) = src_path.parent() {
                p
            } else {
                continue;
            };
            let prefix =
                web.prefix.clone().unwrap_or_else(|| format!("/plugins/{}", sanitize_prefix_segment(&def.name)));
            let target = base_dir.join(&web.dir);
            if let Err(err) = self.register_assets(RegisterAssetsArgs {
                prefix: &prefix,
                dir: &target,
                root_guard: Some(base_dir),
                index: web.index.as_deref(),
                fallback: web.fallback.as_deref(),
                host_id,
                owner_session_id: session_id.clone(),
                plugin: Some(def.name.clone()),
                declarative: true,
            }) {
                tracing::warn!(plugin = %def.name, error = %err.public_message(), "declarative plugin web mount skipped");
                kernel.add_problem(format!("plugin `{}` web mount skipped: {}", def.name, err.public_message()));
            }
        }

        if !include_packages {
            return;
        }
        for pkg in mare_plugin_manager::installed(config) {
            let Ok(Some(manifest)) = mare_plugin_manager::manifest(&pkg.dir) else { continue };
            let Some(web) = manifest.web else { continue };
            let prefix =
                web.prefix.clone().unwrap_or_else(|| format!("/packages/{}", sanitize_prefix_segment(&pkg.name)));
            let target = pkg.dir.join(&web.dir);
            if let Err(err) = self.register_assets(RegisterAssetsArgs {
                prefix: &prefix,
                dir: &target,
                root_guard: Some(&pkg.dir),
                index: web.index.as_deref(),
                fallback: web.fallback.as_deref(),
                host_id,
                owner_session_id: session_id.clone(),
                plugin: Some(pkg.name.clone()),
                declarative: true,
            }) {
                tracing::warn!(package = %pkg.name, error = %err.public_message(), "declarative package web mount skipped");
            }
        }
    }
}

/// Sendable static asset lookup descriptor captured from the registered mount on the LocalSet.
#[derive(Debug, Clone)]
pub struct PreparedAsset {
    asset_id: String,
    canonical_dir: PathBuf,
    index: String,
    fallback: Option<String>,
    rel_decoded: String,
    safe_rel: PathBuf,
    head_only: bool,
}

impl PreparedAsset {
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }
}

/// Resolve the filesystem path, permissions, file type, size limit, and (for `GET`) file bytes
/// for a [`PreparedAsset`] on a blocking thread.
pub fn resolve_prepared_asset(prepared: PreparedAsset) -> AssetResolve {
    use std::io::Read;

    let mut target = prepared.canonical_dir.join(&prepared.safe_rel);
    if target.is_dir() {
        target = target.join(&prepared.index);
    }
    let resolved = match dunce::canonicalize(&target) {
        Ok(canon) if canon.starts_with(&prepared.canonical_dir) && canon.is_file() => Some(canon),
        _ => None,
    };
    let final_path = match resolved {
        Some(p) => p,
        None => {
            let last_seg = prepared.rel_decoded.rsplit('/').next().unwrap_or("");
            let allow_fallback = prepared.fallback.as_ref().filter(|_| !last_seg.contains('.'));
            if let Some(fb) = allow_fallback
                && let Some(fb_rel) = safe_relative_subpath(fb)
                && let Ok(canon) = dunce::canonicalize(prepared.canonical_dir.join(fb_rel))
                && canon.starts_with(&prepared.canonical_dir)
                && canon.is_file()
            {
                canon
            } else {
                return AssetResolve::NotFound;
            }
        }
    };
    let Ok(file) = std::fs::File::open(&final_path) else {
        return AssetResolve::NotFound;
    };
    let Ok(meta) = file.metadata() else {
        return AssetResolve::NotFound;
    };
    if !meta.is_file() {
        return AssetResolve::NotFound;
    }
    if meta.len() > MAX_STATIC_FILE_BYTES {
        return AssetResolve::TooLarge;
    }
    let content_type = content_type_for(&final_path);
    if prepared.head_only {
        return AssetResolve::File { content_type, bytes: Vec::new(), head_only: true };
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    if file.take(MAX_STATIC_FILE_BYTES + 1).read_to_end(&mut bytes).is_err() {
        return AssetResolve::NotFound;
    }
    if bytes.len() as u64 > MAX_STATIC_FILE_BYTES {
        return AssetResolve::TooLarge;
    }
    AssetResolve::File { content_type, bytes, head_only: false }
}

impl WebRegistry {
    /// Validate mount prefix, HTTP method, and decoded relative path without performing disk I/O.
    pub fn prepare_asset_lookup(&self, method: &str, req_path: &str) -> Result<PreparedAsset, AssetResolve> {
        let mount = {
            let assets = self.assets.borrow();
            assets.iter().filter(|a| prefix_matches(&a.prefix, req_path)).max_by_key(|a| a.prefix.len()).cloned()
        };
        let Some(mount) = mount else { return Err(AssetResolve::NotMatched) };
        let head_only = method.eq_ignore_ascii_case("HEAD");
        if !head_only && !method.eq_ignore_ascii_case("GET") {
            return Err(AssetResolve::MethodNotAllowed);
        }
        let rel_raw = strip_mount_prefix(&mount.prefix, req_path);
        let rel_decoded = match percent_decode_path(rel_raw) {
            Ok(s) => s,
            Err(msg) => return Err(AssetResolve::BadRequest(msg)),
        };
        let candidate_rel = if rel_decoded.is_empty() || rel_decoded.ends_with('/') {
            format!("{rel_decoded}{}", mount.index)
        } else {
            rel_decoded.clone()
        };
        let safe_rel = match safe_relative_subpath(&candidate_rel) {
            Some(p) => p,
            None => return Err(AssetResolve::BadRequest("invalid asset path")),
        };
        Ok(PreparedAsset {
            asset_id: mount.asset_id,
            canonical_dir: mount.canonical_dir,
            index: mount.index,
            fallback: mount.fallback,
            rel_decoded,
            safe_rel,
            head_only,
        })
    }

    /// Verify on the LocalSet that `expected_asset_id` is still the winning mount for `req_path`.
    pub fn asset_still_active(&self, req_path: &str, expected_asset_id: &str) -> bool {
        let assets = self.assets.borrow();
        assets
            .iter()
            .filter(|a| prefix_matches(&a.prefix, req_path))
            .max_by_key(|a| a.prefix.len())
            .is_some_and(|a| a.asset_id == expected_asset_id)
    }

    /// Resolve a static asset HTTP request against registered mounts (longest prefix wins).
    pub fn resolve_asset(&self, method: &str, req_path: &str) -> AssetResolve {
        match self.prepare_asset_lookup(method, req_path) {
            Ok(prepared) => resolve_prepared_asset(prepared),
            Err(early) => early,
        }
    }
}

fn service_err(code: &str, message: impl Into<String>) -> ServiceCallError {
    ServiceCallError::Coded(ErrorValue { code: format!("webserver.{code}"), message: message.into(), data: None })
}

fn normalize_route_method(raw: &str) -> Result<String, ServiceCallError> {
    let up = raw.trim().to_ascii_uppercase();
    if ALLOWED_METHODS.contains(&up.as_str()) {
        Ok(up)
    } else {
        Err(service_err("invalid_route", format!("unsupported HTTP method `{raw}`")))
    }
}

fn normalize_route_path(raw: &str) -> Result<String, ServiceCallError> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('/') || trimmed.contains(['?', '#', '\\']) || trimmed.contains('\0') {
        return Err(service_err("invalid_route", format!("invalid route path `{raw}`")));
    }
    if RESERVED_PATHS.contains(&trimmed) {
        return Err(service_err("invalid_route", format!("route path `{trimmed}` is reserved")));
    }
    Ok(trimmed.to_string())
}

fn normalize_asset_prefix(raw: &str) -> Result<String, ServiceCallError> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('/')
        || trimmed.contains(['?', '#', '\\'])
        || trimmed.contains('\0')
        || trimmed.contains("..")
    {
        return Err(service_err("invalid_asset_dir", format!("invalid asset prefix `{raw}`")));
    }
    let normalized = if trimmed.len() > 1 { trimmed.trim_end_matches('/') } else { "/" };
    if RESERVED_PATHS.contains(&normalized) {
        return Err(service_err("invalid_asset_dir", format!("asset prefix `{normalized}` is reserved")));
    }
    Ok(normalized.to_string())
}

fn sanitize_prefix_segment(name: &str) -> String {
    name.trim_matches('/').replace('/', "-")
}

fn canonicalize_asset_dir(dir: &Path, root_guard: Option<&Path>) -> Result<PathBuf, ServiceCallError> {
    let canon_dir = dunce::canonicalize(dir)
        .map_err(|e| service_err("invalid_asset_dir", format!("cannot resolve {}: {e}", dir.display())))?;
    if !canon_dir.is_dir() {
        return Err(service_err("invalid_asset_dir", format!("{} is not a directory", dir.display())));
    }
    if let Some(root) = root_guard {
        let canon_root = dunce::canonicalize(root)
            .map_err(|e| service_err("invalid_asset_dir", format!("cannot resolve root {}: {e}", root.display())))?;
        if !canon_dir.starts_with(&canon_root) {
            return Err(service_err("invalid_asset_dir", format!("{} escapes its plugin root", dir.display())));
        }
    }
    Ok(canon_dir)
}

fn normalize_rel_file(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\\') || trimmed.contains('\0') {
        return None;
    }
    safe_relative_subpath(trimmed)?;
    Some(trimmed.to_string())
}

fn prefix_matches(prefix: &str, req_path: &str) -> bool {
    if prefix == "/" {
        return req_path.starts_with('/');
    }
    req_path == prefix || req_path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

fn strip_mount_prefix<'a>(prefix: &str, req_path: &'a str) -> &'a str {
    if prefix == "/" {
        req_path.strip_prefix('/').unwrap_or(req_path)
    } else {
        req_path.strip_prefix(prefix).unwrap_or("").trim_start_matches('/')
    }
}

fn percent_decode_path(raw: &str) -> Result<String, &'static str> {
    if raw.contains('\\') || raw.contains('\0') {
        return Err("illegal path character");
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("malformed percent-encoding");
            }
            let hi = (bytes[i + 1] as char).to_digit(16).ok_or("malformed percent-encoding")?;
            let lo = (bytes[i + 2] as char).to_digit(16).ok_or("malformed percent-encoding")?;
            let byte = ((hi << 4) | lo) as u8;
            if byte == 0 || byte == b'\\' {
                return Err("illegal decoded byte");
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "invalid utf-8 in path")
}

fn safe_relative_subpath(rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Install `mare.webserver` primitives onto `mare` for the built-in `webserver` service plugin.
pub(crate) fn install_lua(
    lua: &Lua,
    mare: &Table,
    registry: Rc<WebRegistry>,
    host_id: u64,
    session_id_cell: Rc<RefCell<Option<String>>>,
    cwd: PathBuf,
) -> anyhow::Result<()> {
    let tbl = lua.create_table()?;

    let reg_get = registry.clone();
    tbl.set(
        "get",
        lua.create_function(move |lua, ()| {
            let info = reg_get.listener();
            let mut val = serde_json::json!({ "running": info.running });
            if info.running {
                if let Some(u) = &info.url {
                    val["url"] = u.clone().into();
                }
                if let Some(u) = &info.ws_url {
                    val["ws_url"] = u.clone().into();
                }
                if let Some(h) = &info.host {
                    val["host"] = h.clone().into();
                }
                if let Some(p) = info.port {
                    val["port"] = p.into();
                }
                val["tls"] = info.tls.into();
                val["trusted_hosts"] = serde_json::json!(info.trusted_hosts);
            }
            mare_shared::lua::json_to_lua(lua, &val)
        })?,
    )?;

    let reg_routes = registry.clone();
    tbl.set(
        "list_routes",
        lua.create_function(move |lua, ()| {
            mare_shared::lua::json_to_lua(lua, &serde_json::Value::Array(reg_routes.list_routes()))
        })?,
    )?;

    let reg_assets = registry.clone();
    tbl.set(
        "list_assets",
        lua.create_function(move |lua, ()| {
            mare_shared::lua::json_to_lua(lua, &serde_json::Value::Array(reg_assets.list_assets()))
        })?,
    )?;

    let reg_r = registry.clone();
    let sid_r = session_id_cell.clone();
    tbl.set(
        "register_route",
        lua.create_function(move |lua, (owner_name, path, opts): (Option<String>, String, Option<Table>)| {
            let method = opts
                .as_ref()
                .and_then(|o| o.get::<Option<String>>("method").ok().flatten())
                .unwrap_or_else(|| "GET".to_string());
            let handle: Option<Function> = opts.as_ref().and_then(|o| o.get("handle").ok());
            let handle = handle.ok_or_else(|| {
                mlua::Error::external(service_err("invalid_route", "register_route requires a `handle` callback"))
            })?;
            let reg_key = Rc::new(RefCell::new(Some(lua.create_registry_value(handle)?)));
            let reg_key_cb = reg_key.clone();
            let weak_lua = lua.weak();
            let cb: RouteCallback = Rc::new(move |req: WebRouteRequest| {
                let weak_lua = weak_lua.clone();
                let func = weak_lua
                    .try_upgrade()
                    .and_then(|l| reg_key_cb.borrow().as_ref().and_then(|k| l.registry_value::<Function>(k).ok()));
                Box::pin(async move {
                    let Some(func) = func else {
                        return Err("route handler lua VM unavailable".to_string());
                    };
                    let Some(l) = weak_lua.try_upgrade() else {
                        return Err("route handler lua VM gone".to_string());
                    };
                    let req_json = serde_json::to_value(&req).map_err(|e| e.to_string())?;
                    let req_val = mare_shared::lua::json_to_lua(&l, &req_json).map_err(|e| e.to_string())?;
                    let res = func.call_async::<Value>(req_val).await.map_err(|e| e.to_string())?;
                    let res_json = mare_shared::lua::strict_lua_to_json(&l, res).map_err(|e| e.to_string())?;
                    serde_json::from_value::<WebRouteResponse>(res_json)
                        .map_err(|e| format!("route handler response invalid: {e}"))
                })
            });
            let route_id = reg_r
                .register_route(&method, &path, host_id, sid_r.clone(), owner_name, cb)
                .map_err(mlua::Error::external)?;
            let reg_drop = Rc::downgrade(&reg_r);
            let weak_lua_drop = lua.weak();
            lua.create_function(move |_, ()| {
                if let Some(key) = reg_key.borrow_mut().take()
                    && let Some(l) = weak_lua_drop.try_upgrade()
                {
                    let _ = l.remove_registry_value(key);
                }
                let removed = reg_drop.upgrade().is_some_and(|r| r.unregister_route(&route_id));
                Ok(removed)
            })
        })?,
    )?;

    let reg_a = registry;
    let sid_a = session_id_cell;
    tbl.set(
        "register_assets",
        lua.create_function(move |lua, (owner_name, dir, opts): (Option<String>, String, Option<Table>)| {
            let prefix = opts
                .as_ref()
                .and_then(|o| o.get::<Option<String>>("prefix").ok().flatten())
                .unwrap_or_else(|| "/".to_string());
            let index = opts.as_ref().and_then(|o| o.get::<Option<String>>("index").ok().flatten());
            let fallback = opts.as_ref().and_then(|o| o.get::<Option<String>>("fallback").ok().flatten());
            let raw_path = Path::new(&dir);
            let resolved_dir = if raw_path.is_absolute() { raw_path.to_path_buf() } else { cwd.join(raw_path) };
            let asset_id = reg_a
                .register_assets(RegisterAssetsArgs {
                    prefix: &prefix,
                    dir: &resolved_dir,
                    root_guard: None,
                    index: index.as_deref(),
                    fallback: fallback.as_deref(),
                    host_id,
                    owner_session_id: sid_a.clone(),
                    plugin: owner_name,
                    declarative: false,
                })
                .map_err(mlua::Error::external)?;
            let reg_drop = Rc::downgrade(&reg_a);
            lua.create_function(move |_, ()| {
                let removed = reg_drop.upgrade().is_some_and(|r| r.unregister_asset(&asset_id));
                Ok(removed)
            })
        })?,
    )?;

    mare.set(mare_plugin::WEBSERVER_GLOBAL_NAME, tbl)?;
    Ok(())
}
