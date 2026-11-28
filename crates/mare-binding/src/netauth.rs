//! Network authorization shared by every way a tool call reaches the network:
//! `mare.http`, the sandbox proxy ([`crate::netproxy`]) and `mare.net`. One decision per
//! hop — host rules (wildcards, deny list, approval of allow-list misses), then DNS, then a
//! private/loopback check on every resolved address — and the caller connects to the addresses
//! that passed (pinning; no second resolution an attacker could rebind). Proxy environment
//! variables are only a convenience on top of this, never the restriction itself.
//! Host rules and non-public ranges (Apache-2.0); special-range IPv6 checks (NAT64,
//! 6to4, Teredo, etc.) and fake-IP resolution / local-proxy routing (MIT).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};

use mlua::{Lua, Table};

/// Test/debug resolver override: `name -> addresses` (consulted before the system resolver).
static RESOLVER: RwLock<Option<HashMap<String, Vec<IpAddr>>>> = RwLock::new(None);
static RESOLVER_DELAY: RwLock<Option<std::time::Duration>> = RwLock::new(None);

/// Replace the resolver override (tests: map a fake name to a local server to observe pinning
/// and the private-address check without real DNS). `None` restores the system resolver.
#[doc(hidden)]
pub fn set_resolver_override(map: Option<HashMap<String, Vec<IpAddr>>>) {
    if let Ok(mut g) = RESOLVER.write() {
        *g = map.map(|m| m.into_iter().map(|(k, v)| (normalize_host(&k), v)).collect());
    }
}

/// Optional artificial delay in [`lookup`] (tests: verify fetch timeout covers DNS resolution).
#[doc(hidden)]
pub fn set_resolver_delay(delay: Option<std::time::Duration>) {
    if let Ok(mut g) = RESOLVER_DELAY.write() {
        *g = delay;
    }
}

async fn lookup(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Some(delay) = RESOLVER_DELAY.read().ok().and_then(|g| *g) {
        tokio::time::sleep(delay).await;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if !host.contains('%')
        && let Some(ip) = numeric_host_address(host)
    {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let over = RESOLVER.read().ok().and_then(|g| g.as_ref().and_then(|m| m.get(host).cloned()));
    if let Some(ips) = over {
        return Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect());
    }
    tokio::net::lookup_host((host, port)).await.map(|a| a.collect()).map_err(|e| format!("resolve {host}: {e}"))
}

/// Frozen snapshot of the originating session when a [`NetApprover`] handle was captured:
/// prevents cross-session approval/cache leakage when a session switches mid-flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetAskOrigin {
    pub session_id: Option<String>,
    pub session_epoch: String,
}

#[derive(Clone)]
struct NetOriginSource {
    session: Arc<Mutex<(Option<String>, String)>>,
}

impl NetOriginSource {
    fn snapshot(&self) -> NetAskOrigin {
        let (session_id, session_epoch) = self.session.lock().unwrap().clone();
        NetAskOrigin { session_id, session_epoch }
    }
}

/// A sandboxed process wants a host that the allow list doesn't cover: ask whoever approves
/// network access. The host installs
/// the receiving end ([`set_approver`]) and answers `true` (allowed) / `false`.
pub struct NetAsk {
    pub host: String,
    pub port: u16,
    pub origin: Option<NetAskOrigin>,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Per-session network approver handle, stored in the originating session's Lua `app_data`
/// and explicitly delegated to child agents or captured by tool invocations.
#[derive(Clone)]
pub struct NetApprover {
    tx: tokio::sync::mpsc::Sender<NetAsk>,
    frozen_origin: Option<NetAskOrigin>,
}

impl NetApprover {
    pub fn new(tx: tokio::sync::mpsc::Sender<NetAsk>) -> Self {
        Self { tx, frozen_origin: None }
    }

    pub fn with_origin(mut self, origin: NetAskOrigin) -> Self {
        self.frozen_origin = Some(origin);
        self
    }

    pub fn origin(&self) -> Option<&NetAskOrigin> {
        self.frozen_origin.as_ref()
    }
}

/// Install (or remove) the network approver for `lua`'s session/runtime. Without one, hosts
/// outside the allow list are refused.
pub fn set_approver(lua: &Lua, approver: Option<NetApprover>) {
    match approver {
        Some(a) => {
            lua.set_app_data(a);
        }
        None => {
            let _ = lua.remove_app_data::<NetApprover>();
        }
    }
}

/// Update the live session origin source on `lua` so subsequent [`approver`] captures freeze the
/// originating session identity.
pub fn set_origin_source(lua: &Lua, session_id: Option<String>, session_epoch: String) {
    if let Some(src) = lua.app_data_ref::<NetOriginSource>() {
        *src.session.lock().unwrap() = (session_id, session_epoch);
        return;
    }
    lua.set_app_data(NetOriginSource { session: Arc::new(Mutex::new((session_id, session_epoch))) });
}

/// Read the network approver bound to `lua`'s session/runtime, freezing its originating session
/// snapshot if not already frozen (e.g. inherited by a child agent).
pub fn approver(lua: &Lua) -> Option<NetApprover> {
    let mut ap = lua.app_data_ref::<NetApprover>().map(|a| a.clone())?;
    if ap.frozen_origin.is_none()
        && let Some(src) = lua.app_data_ref::<NetOriginSource>()
    {
        ap.frozen_origin = Some(src.snapshot());
    }
    Some(ap)
}

/// Ask `approver` about `host:port`; `false` when there is none or it doesn't answer.
async fn ask_approver(approver: Option<&NetApprover>, host: &str, port: u16) -> bool {
    let Some(approver) = approver else { return false };
    let (reply, rx) = tokio::sync::oneshot::channel();
    if approver
        .tx
        .send(NetAsk { host: host.to_string(), port, origin: approver.frozen_origin.clone(), reply })
        .await
        .is_err()
    {
        return false;
    }
    rx.await.unwrap_or(false)
}

/// What the network rules guarantee, said plainly (shown by `mare doctor` and `/permission caps`):
/// no TLS termination, no SNI inspection.
pub const GUARANTEE: &str = "domain rules limit the connection target (proxy CONNECT/SOCKS host, its resolved \
addresses — never local/private unless allowed — and the address actually connected to); they do not \
limit the application-level name inside a pass-through HTTPS tunnel (no TLS termination, no SNI check)";

/// How `198.18.0.0/15` fake-IP answers are handled (configured via `net.fake_ip`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FakeIpMode {
    /// Disable fake-IP recognition; treat `198.18.0.0/15` as normal non-public addresses.
    #[default]
    Off,
    /// Recognize fake-IP resolution shape (all A records in `198.18.0.0/15`, ignoring paired
    /// ULA / `2001:2::/48` AAAA records).
    Auto,
}

/// How an authorized connection will reach the target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxyRoute {
    /// Direct connection (no upstream proxy).
    #[default]
    Direct,
    /// Connection goes through a proxy on this machine (loopback / `localhost`).
    LocalProxy,
    /// Connection goes through a non-local proxy.
    RemoteProxy,
}

/// Result of [`NetPolicy::authorize_target`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthTarget {
    /// Connect to these pinned socket addresses (never re-resolve).
    Pinned(Vec<SocketAddr>),
    /// Fake-IP answer routed via a local proxy: pass the hostname to the proxy, not the fake IP.
    ViaLocalProxyByName,
}

/// Read `net.fake_ip` from Lua `mare.o` / `mare.config` (`"auto"` -> `Auto`, else `Off`).
pub fn fake_ip_mode_from_lua(lua: &Lua) -> FakeIpMode {
    let Ok(mare) = lua.globals().get::<Table>("mare") else {
        return FakeIpMode::Off;
    };
    let cfg = mare.get::<Table>("o").or_else(|_| mare.get::<Table>("config"));
    let Ok(cfg) = cfg else {
        return FakeIpMode::Off;
    };
    let Ok(net) = cfg.get::<Table>("net") else {
        return FakeIpMode::Off;
    };
    match net.get::<Option<String>>("fake_ip") {
        Ok(Some(s)) if s.trim().eq_ignore_ascii_case("auto") => FakeIpMode::Auto,
        _ => FakeIpMode::Off,
    }
}

fn unscoped_ip_literal(host: &str) -> Option<&str> {
    let (addr, zone) = host.split_once('%')?;
    (!zone.is_empty() && addr.parse::<Ipv6Addr>().is_ok()).then_some(addr)
}

/// Parse IPv4/IPv6 literals including scoped IPv6 (`fe80::1%eth0`) and alternate numeric IPv4
/// forms (`0x7f000001`, `0177.0.0.1`, `2130706433`, `127.1`) via `url::Host::parse`.
fn numeric_host_address(host: &str) -> Option<IpAddr> {
    let h = normalize_host(host);
    let unscoped = unscoped_ip_literal(&h).unwrap_or(h.as_str());
    if let Ok(ip) = unscoped.parse::<IpAddr>() {
        return Some(ip);
    }
    if !unscoped.as_bytes().first()?.is_ascii_digit()
        || !unscoped.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b'x' | b'X'))
    {
        return None;
    }
    match url::Host::parse(unscoped).ok()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        _ => None,
    }
}

fn is_localhost_name(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost")
}

/// Whether `host` refers to this machine (`localhost`, `*.localhost`, or a loopback IP,
/// including `::ffff:127.x` and alternate numeric IPv4 forms).
pub fn is_local_proxy_host(host: &str) -> bool {
    let h = normalize_host(host);
    if is_localhost_name(&h) {
        return true;
    }
    numeric_host_address(&h).is_some_and(|ip| match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    })
}

#[derive(Clone, Debug, Default)]
pub struct NetPolicy {
    allow: Vec<String>,
    deny: Vec<String>,
    default_allow: bool,
    allow_local: bool,
    pub fake_ip: FakeIpMode,
}

impl NetPolicy {
    /// Open network with private/local IP blocking enforced (`web_fetch` SSRF guard).
    pub fn block_private_open(fake_ip: FakeIpMode) -> Self {
        Self { allow: Vec::new(), deny: Vec::new(), default_allow: true, allow_local: false, fake_ip }
    }
}

pub(crate) fn normalize_host(h: &str) -> String {
    let h = h.trim();
    let h = h.strip_prefix('[').and_then(|x| x.split(']').next()).unwrap_or(h);
    h.trim_end_matches('.').to_ascii_lowercase()
}

/// `example.com` exact; `*.example.com` subdomains only; `**.example.com` apex + subdomains.
fn matches(pattern: &str, host: &str) -> bool {
    if let Some(d) = pattern.strip_prefix("**.") {
        host == d || host.ends_with(&format!(".{d}"))
    } else if let Some(d) = pattern.strip_prefix("*.") {
        host.ends_with(&format!(".{d}"))
    } else {
        host == pattern
    }
}

fn validate(pattern: &str) -> Result<String, String> {
    let p = normalize_host(pattern);
    let core = p.trim_start_matches("**.").trim_start_matches("*.");
    if p.is_empty() || core.is_empty() || core.contains('*') || p == "*" || p == "**" {
        return Err(format!("invalid domain pattern `{pattern}` (a bare `*` is not allowed; use `*.example.com`)"));
    }
    Ok(p)
}

pub fn is_non_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => non_public_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped (`::ffff:a.b.c.d`) reaches the embedded IPv4 host
            if let Some(v4) = v6.to_ipv4_mapped() {
                return non_public_v4(v4);
            }
            let o = v6.octets();
            // `::/96` (deprecated IPv4-compatible)
            if o[..12] == [0; 12] {
                return true;
            }
            // `64:ff9b::/96` (NAT64 well-known prefix): check embedded IPv4
            if o[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                return is_non_public(IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15])));
            }
            // `2002::/16` (6to4): check embedded IPv4
            if o[0] == 0x20 && o[1] == 0x02 {
                return is_non_public(IpAddr::V4(Ipv4Addr::new(o[2], o[3], o[4], o[5])));
            }
            // `64:ff9b:1::/48` (local-use NAT64), `2001::/32` (Teredo), `2001:2::/48`
            // (benchmarking), `2001:db8::/32` (documentation), `100::/64` (discard),
            // `fec0::/10` (deprecated site-local)
            let special = o[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01]
                || o[..4] == [0x20, 0x01, 0x00, 0x00]
                || is_benchmark_v6(v6)
                || o[..4] == [0x20, 0x01, 0x0d, 0xb8]
                || o[..8] == [0x01, 0x00, 0, 0, 0, 0, 0, 0]
                || (o[0] == 0xfe && (o[1] & 0xc0) == 0xc0);
            special
                || v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

fn is_benchmark_v6(v6: Ipv6Addr) -> bool {
    v6.octets()[..6] == [0x20, 0x01, 0x00, 0x02, 0x00, 0x00]
}

fn is_fake_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && (o[1] & 0xfe) == 18
}

fn is_fake_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_fake_v4(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(is_fake_v4),
    }
}

fn is_fake_paired_v6(ip: IpAddr, addrs: &[SocketAddr]) -> bool {
    let IpAddr::V6(v6) = ip else { return false };
    if v6.to_ipv4_mapped().is_some() {
        return false;
    }
    if !(v6.is_unique_local() || is_benchmark_v6(v6)) {
        return false;
    }
    addrs.iter().any(|a| is_fake_ip(a.ip()))
}

fn non_public_v4(ip: Ipv4Addr) -> bool {
    let cidr = |base: [u8; 4], prefix: u32| {
        let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(base)) & mask)
    };
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || cidr([0, 0, 0, 0], 8)
        || cidr([100, 64, 0, 0], 10)
        || cidr([192, 0, 0, 0], 24)
        || cidr([192, 0, 2, 0], 24)
        || cidr([198, 18, 0, 0], 15)
        || cidr([198, 51, 100, 0], 24)
        || cidr([203, 0, 113, 0], 24)
        || cidr([240, 0, 0, 0], 4)
}

impl NetPolicy {
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Intersection: a host passes the
    /// result only if it passes both. Deny lists are unioned, `allow_local` / `default = allow`
    /// need both; an allow pattern is kept when the other side's allow list covers it — patterns
    /// with no counterpart are dropped (an empty result blocks the network).
    pub fn intersect(&self, o: &Self) -> Self {
        let covers = |q: &str, p: &str| -> bool {
            // pattern q admits every host pattern p admits
            if q == p {
                return true;
            }
            let core = |x: &str| x.trim_start_matches("**.").trim_start_matches("*.").to_string();
            match (q.strip_prefix("**."), q.strip_prefix("*.")) {
                (Some(d), _) => {
                    let c = core(p);
                    c == d || c.ends_with(&format!(".{d}"))
                }
                (None, Some(d)) => {
                    let c = core(p);
                    // `*.d` admits strict subdomains only
                    (!p.starts_with("**.") || c != d)
                        && (c.ends_with(&format!(".{d}")) || (p.starts_with("*.") && c == d))
                }
                _ => false,
            }
        };
        let side = |a: &Self, b: &Self| -> Vec<String> {
            if b.default_allow {
                return a.allow.clone();
            }
            a.allow.iter().filter(|p| b.allow.iter().any(|q| covers(q, p))).cloned().collect()
        };
        let default_allow = self.default_allow && o.default_allow;
        let mut allow = if default_allow { vec![] } else { side(self, o) };
        if !default_allow {
            allow.extend(side(o, self));
            allow.sort();
            allow.dedup();
        }
        let mut deny = self.deny.clone();
        deny.extend(o.deny.iter().cloned());
        deny.sort();
        deny.dedup();
        let fake_ip = if self.fake_ip == FakeIpMode::Off || o.fake_ip == FakeIpMode::Off {
            FakeIpMode::Off
        } else {
            FakeIpMode::Auto
        };
        Self { allow, deny, default_allow, allow_local: self.allow_local && o.allow_local, fake_ip }
    }

    pub fn is_deny_all(&self) -> bool {
        !self.default_allow && self.allow.is_empty()
    }

    pub fn allows_local(&self) -> bool {
        self.allow_local
    }

    pub fn tighten_fake_ip(&mut self, mode: FakeIpMode) {
        if mode == FakeIpMode::Off {
            self.fake_ip = FakeIpMode::Off;
        }
    }

    pub fn from_lua(t: &Table) -> Result<Self, String> {
        let list = |k: &str| -> Result<Vec<String>, String> {
            t.get::<Option<Vec<String>>>(k)
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .iter()
                .map(|p| validate(p))
                .collect()
        };
        let default = t.get::<Option<String>>("default").map_err(|e| e.to_string())?;
        let default_allow = match default.as_deref() {
            None | Some("deny") => false,
            Some("allow") => true,
            Some(o) => return Err(format!("default must be \"allow\" or \"deny\", got `{o}`")),
        };
        Ok(Self {
            allow: list("allow")?,
            deny: list("deny")?,
            default_allow,
            allow_local: t.get::<Option<bool>>("allow_local").map_err(|e| e.to_string())?.unwrap_or(false),
            fake_ip: FakeIpMode::default(),
        })
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("allow", self.allow.clone())?;
        t.set("deny", self.deny.clone())?;
        t.set("default", if self.default_allow { "allow" } else { "deny" })?;
        t.set("allow_local", self.allow_local)?;
        Ok(t)
    }

    /// Name-only decision (no DNS): `Err(reason)` when refused.
    pub fn check_host(&self, host: &str) -> Result<(), String> {
        let h = normalize_host(host);
        if self.deny.iter().any(|p| matches(p, &h)) {
            return Err(format!("{h} is in the network deny list"));
        }
        if let Some(ip) = numeric_host_address(&h)
            && is_non_public(ip)
            && !self.allow_local
        {
            return Err(format!("{h} is a local/private address (set allow_local = true)"));
        }
        if is_localhost_name(&h) && !self.allow_local {
            return Err("localhost is blocked (set allow_local = true)".into());
        }
        if self.default_allow || self.allow.iter().any(|p| matches(p, &h)) {
            Ok(())
        } else {
            Err(format!("{h} is not in the network allow list"))
        }
    }

    /// Refused only because the allow list doesn't cover it (approvable).
    fn not_listed(&self, host: &str) -> bool {
        let h = normalize_host(host);
        let local = is_localhost_name(&h) || numeric_host_address(&h).is_some_and(is_non_public);
        !self.default_allow
            && !local
            && !self.deny.iter().any(|p| matches(p, &h))
            && !self.allow.iter().any(|p| matches(p, &h))
    }

    /// Full decision for one connection (one hop): name rules (an allow-list miss may be
    /// approved), then the resolved addresses — a public name that resolves to a private address
    /// is still local (DNS rebinding / SSRF). Returns the addresses that passed: callers connect
    /// to exactly these (pinning), never re-resolve.
    pub async fn authorize(
        &self,
        host: &str,
        port: u16,
        approver: Option<&NetApprover>,
    ) -> Result<Vec<SocketAddr>, String> {
        match self.authorize_target(host, port, approver, ProxyRoute::Direct, self.fake_ip).await? {
            AuthTarget::Pinned(addrs) => Ok(addrs),
            AuthTarget::ViaLocalProxyByName => unreachable!("direct route never returns ViaLocalProxyByName"),
        }
    }

    /// Full decision for one connection (one hop), aware of whether the connection goes direct or
    /// through a local/remote proxy and whether fake-IP recognition is active.
    pub async fn authorize_target(
        &self,
        host: &str,
        port: u16,
        approver: Option<&NetApprover>,
        route: ProxyRoute,
        fake_ip_mode: FakeIpMode,
    ) -> Result<AuthTarget, String> {
        if let Err(e) = self.check_host(host) {
            // only an allow-list miss can be approved; deny-listed and local hosts stay refused
            if !(self.not_listed(host) && ask_approver(approver, &normalize_host(host), port).await) {
                return Err(e);
            }
        }
        let h = normalize_host(host);
        let is_literal = numeric_host_address(&h).is_some();
        let addrs = lookup(&h, port).await?;
        if addrs.is_empty() {
            return Err(format!("resolve {h}: no addresses"));
        }
        if fake_ip_mode == FakeIpMode::Auto && !is_literal {
            let non_paired: Vec<SocketAddr> =
                addrs.iter().copied().filter(|a| !is_fake_paired_v6(a.ip(), &addrs)).collect();
            if !non_paired.is_empty() && non_paired.iter().all(|a| is_fake_ip(a.ip())) {
                return match route {
                    ProxyRoute::Direct => Ok(AuthTarget::Pinned(non_paired)),
                    ProxyRoute::LocalProxy if self.allow_local => Ok(AuthTarget::ViaLocalProxyByName),
                    ProxyRoute::LocalProxy => Err(format!(
                        "{h} resolves to a fake-ip address; by-name local proxy routing requires allow_local = true"
                    )),
                    ProxyRoute::RemoteProxy => Err(format!(
                        "{h} resolves to a fake-ip address, which cannot be routed through a non-local proxy"
                    )),
                };
            }
        }
        if !self.allow_local && addrs.iter().any(|a| is_non_public(a.ip())) {
            return Err(format!("{h} resolves to a local/private address (set allow_local = true)"));
        }
        Ok(AuthTarget::Pinned(addrs))
    }
}

#[cfg(test)]
pub(crate) static RESOLVER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(allow: &[&str], local: bool) -> NetPolicy {
        NetPolicy {
            allow: allow.iter().map(|p| validate(p).unwrap()).collect(),
            deny: vec![validate("bad.example.com").unwrap()],
            default_allow: false,
            allow_local: local,
            fake_ip: FakeIpMode::Auto,
        }
    }

    #[test]
    fn host_rules() {
        let p = pol(&["github.com", "*.crates.io", "**.npmjs.org"], false);
        assert!(p.check_host("GitHub.com.").is_ok());
        assert!(p.check_host("api.github.com").is_err());
        assert!(p.check_host("static.crates.io").is_ok());
        assert!(p.check_host("crates.io").is_err());
        assert!(p.check_host("npmjs.org").is_ok() && p.check_host("registry.npmjs.org").is_ok());
        assert!(p.check_host("127.0.0.1").is_err() && p.check_host("localhost").is_err());
        assert!(p.check_host("10.1.2.3").is_err());
        assert!(pol(&["*.example.com"], false).check_host("bad.example.com").is_err());
        assert!(validate("*").is_err() && validate("**").is_err() && validate("*.").is_err());
        let open = NetPolicy { default_allow: true, allow_local: true, ..Default::default() };
        assert!(open.check_host("127.0.0.1").is_ok());
    }

    #[tokio::test]
    async fn resolved_private_addresses_are_refused_and_addresses_pinned() {
        let _guard = RESOLVER_TEST_LOCK.lock().await;
        set_resolver_override(Some(HashMap::from([
            ("rebind.test".to_string(), vec!["127.0.0.1".parse().unwrap()]),
            ("public.test".to_string(), vec!["93.184.216.34".parse().unwrap()]),
        ])));
        let p = pol(&["rebind.test", "public.test"], false);
        // a listed public name that resolves to loopback is still local (DNS rebinding)
        let e = p.authorize("rebind.test", 80, None).await.unwrap_err();
        assert!(e.contains("resolves to a local/private address"), "{e}");
        // the addresses returned are the ones checked: the caller connects to exactly these
        assert_eq!(p.authorize("public.test", 443, None).await.unwrap(), vec!["93.184.216.34:443".parse().unwrap()]);
        // allow_local opts in
        assert_eq!(pol(&["rebind.test"], true).authorize("rebind.test", 80, None).await.unwrap().len(), 1);
        set_resolver_override(None);
    }
}

#[cfg(test)]
mod v6_tests {
    use super::*;

    #[test]
    fn ipv4_compatible_and_mapped_v6_loopback_are_non_public() {
        for a in ["::127.0.0.1", "::ffff:127.0.0.1", "::10.0.0.1", "::8.8.8.8", "::1", "::"] {
            assert!(is_non_public(a.parse().unwrap()), "{a}");
        }
        assert!(!is_non_public("::ffff:8.8.8.8".parse().unwrap()));
        assert!(!is_non_public("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn nat64_6to4_and_special_v6_ranges() {
        // NAT64 and 6to4 pointing to private/loopback are rejected
        for a in [
            "64:ff9b::10.0.0.1",
            "64:ff9b::127.0.0.1",
            "64:ff9b::192.168.1.1",
            "2002:0a00:0001::1", // 10.0.0.1
            "2002:7f00:0001::1", // 127.0.0.1
        ] {
            assert!(is_non_public(a.parse().unwrap()), "expected non-public: {a}");
        }
        // Public NAT64 and public 6to4 are allowed
        for a in ["64:ff9b::8.8.8.8", "64:ff9b::93.184.216.34", "2002:0808:0808::1"] {
            assert!(!is_non_public(a.parse().unwrap()), "expected public: {a}");
        }
        // Special ranges directly classified as non-public
        for a in ["64:ff9b:1::1", "2001::1", "2001:2::1", "100::1", "fec0::1", "2001:db8::1"] {
            assert!(is_non_public(a.parse().unwrap()), "expected non-public: {a}");
        }
    }

    #[tokio::test]
    async fn fake_ip_recognition_pinning_paired_aaaa_and_proxy_modes() {
        let _guard = RESOLVER_TEST_LOCK.lock().await;
        set_resolver_override(Some(HashMap::from([
            ("fake.test".to_string(), vec!["198.18.0.5".parse().unwrap()]),
            (
                "fake-paired.test".to_string(),
                vec!["198.18.0.6".parse().unwrap(), "fdfe:dcba:9876::1".parse().unwrap(), "2001:2::1".parse().unwrap()],
            ),
            ("mixed-priv.test".to_string(), vec!["198.18.0.7".parse().unwrap(), "10.0.0.1".parse().unwrap()]),
            ("mixed-pub.test".to_string(), vec!["198.18.0.8".parse().unwrap(), "93.184.216.34".parse().unwrap()]),
            ("ula-only.test".to_string(), vec!["fdfe:dcba:9876::1".parse().unwrap()]),
        ])));
        let p = NetPolicy {
            allow: vec![
                validate("fake.test").unwrap(),
                validate("fake-paired.test").unwrap(),
                validate("mixed-priv.test").unwrap(),
                validate("mixed-pub.test").unwrap(),
                validate("ula-only.test").unwrap(),
                validate("198.18.0.5").unwrap(),
            ],
            deny: vec![],
            default_allow: false,
            allow_local: false,
            fake_ip: FakeIpMode::Auto,
        };
        // 1. fake-ip hostname allowed & pinned
        assert_eq!(
            p.authorize_target("fake.test", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.unwrap(),
            AuthTarget::Pinned(vec!["198.18.0.5:443".parse().unwrap()])
        );
        // 2. fake-ip literal IP rejected
        assert!(p.authorize_target("198.18.0.5", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.is_err());
        // 3. mixed resolution follows normal rules (rejected)
        assert!(p.authorize_target("mixed-priv.test", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.is_err());
        assert!(p.authorize_target("mixed-pub.test", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.is_err());
        // 4. paired AAAA (ULA / 2001:2::/48) not dialed (stripped from pinned addresses)
        assert_eq!(
            p.authorize_target("fake-paired.test", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.unwrap(),
            AuthTarget::Pinned(vec!["198.18.0.6:443".parse().unwrap()])
        );
        // unpaired ULA is still rejected
        assert!(p.authorize_target("ula-only.test", 443, None, ProxyRoute::Direct, FakeIpMode::Auto).await.is_err());
        // 5. via local proxy, allow_local = false rejects by-name routing; allow_local = true uses hostname
        assert!(p.authorize_target("fake.test", 443, None, ProxyRoute::LocalProxy, FakeIpMode::Auto).await.is_err());
        let p_local = NetPolicy { allow_local: true, ..p.clone() };
        assert_eq!(
            p_local.authorize_target("fake.test", 443, None, ProxyRoute::LocalProxy, FakeIpMode::Auto).await.unwrap(),
            AuthTarget::ViaLocalProxyByName
        );
        // 6. via non-local proxy, rejected (both allow_local = false and allow_local = true)
        assert!(p.authorize_target("fake.test", 443, None, ProxyRoute::RemoteProxy, FakeIpMode::Auto).await.is_err());
        let remote_err = p_local
            .authorize_target("fake.test", 443, None, ProxyRoute::RemoteProxy, FakeIpMode::Auto)
            .await
            .unwrap_err();
        assert!(remote_err.contains("non-local proxy"), "unexpected error: {remote_err}");
        // 7. net.fake_ip = "off" rejects fake-ip hostname
        assert!(p.authorize_target("fake.test", 443, None, ProxyRoute::Direct, FakeIpMode::Off).await.is_err());
        set_resolver_override(None);
    }

    #[tokio::test]
    async fn default_fake_ip_mode_is_off_and_local_proxy_requires_allow_local() {
        let _guard = RESOLVER_TEST_LOCK.lock().await;
        set_resolver_override(Some(HashMap::from([
            ("fake.test".to_string(), vec!["198.18.0.5".parse().unwrap()]),
            ("fake-ula.test".to_string(), vec!["198.18.0.6".parse().unwrap(), "fd00::1".parse().unwrap()]),
        ])));
        assert_eq!(FakeIpMode::default(), FakeIpMode::Off);
        let lua = mlua::Lua::new();
        let t: mlua::Table = lua.load(r#"return { allow = { "fake.test", "fake-ula.test" } }"#).eval().unwrap();
        let p = NetPolicy::from_lua(&t).unwrap();
        assert_eq!(p.fake_ip, FakeIpMode::Off);
        // Default mode rejects 198.18.0.0/15 direct connection
        assert!(p.authorize_target("fake.test", 443, None, ProxyRoute::Direct, p.fake_ip).await.is_err());
        // Even with FakeIpMode::Auto, by-name LocalProxy is rejected when allow_local = false
        assert!(p.authorize_target("fake.test", 443, None, ProxyRoute::LocalProxy, FakeIpMode::Auto).await.is_err());
        assert!(
            p.authorize_target("fake-ula.test", 443, None, ProxyRoute::LocalProxy, FakeIpMode::Auto).await.is_err()
        );
        // With allow_local = true and FakeIpMode::Auto, LocalProxy returns ViaLocalProxyByName
        let t_local: mlua::Table =
            lua.load(r#"return { allow = { "fake.test", "fake-ula.test" }, allow_local = true }"#).eval().unwrap();
        let p_local = NetPolicy::from_lua(&t_local).unwrap();
        assert_eq!(
            p_local
                .authorize_target("fake-ula.test", 443, None, ProxyRoute::LocalProxy, FakeIpMode::Auto)
                .await
                .unwrap(),
            AuthTarget::ViaLocalProxyByName
        );
        set_resolver_override(None);
    }

    #[tokio::test]
    async fn alternate_numeric_ips_scoped_v6_and_sub_localhost_are_blocked() {
        let open_no_local =
            NetPolicy { default_allow: true, allow_local: false, fake_ip: FakeIpMode::Auto, ..Default::default() };
        for local_host in [
            "0x7f000001",
            "0177.0.0.1",
            "2130706433",
            "127.1",
            "0xc6120001",
            "[fe80::1%eth0]",
            "fe80::1%25eth0",
            "sub.localhost",
            "api.localhost.",
        ] {
            assert!(open_no_local.check_host(local_host).is_err(), "{local_host} must be blocked");
            assert!(!open_no_local.not_listed(local_host), "{local_host} must not be approvable");
            assert!(
                open_no_local
                    .authorize_target(local_host, 80, None, ProxyRoute::Direct, FakeIpMode::Auto)
                    .await
                    .is_err(),
                "{local_host} must not bypass FakeIpMode::Auto"
            );
        }
        assert!(is_local_proxy_host("sub.localhost"));
        assert!(is_local_proxy_host("0x7f000001"));
    }
}
