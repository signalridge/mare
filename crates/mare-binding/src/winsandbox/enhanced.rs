//! Windows process sandbox, enhanced tier: dedicated sandbox
//! accounts, deny-read ACLs and WFP network filters (Apache-2.0).
//!
//! `mare sandbox setup` (elevated once) creates:
//! - a local group `MareSandboxUsers` and two accounts in it: [`USER_OFFLINE`] (network limited to
//!   loopback ports in [`PROXY_PORT_RANGE`] by persistent WFP filters — mare's proxy binds there)
//!   and [`USER_ONLINE`] (for policies with an open network). Random passwords, stored DPAPI
//!   (machine scope) in a setup file under `%ProgramData%\mare\sandbox`;
//! - deny-read ACEs for the group on sensitive paths (SSH/cloud credentials, browser profiles,
//!   mare's own `auth.json`; configurable: `plugins.permissions.deny_read` / `--no-default-deny-read`);
//! - a copy of the `mare` executable in `%ProgramData%\mare\sandbox\bin` (the sandbox accounts
//!   can't read the user's profile, where mare is usually installed) and a deny-read canary file.
//!
//! At run time the default-tier runner (`mare __sandbox-exec --as offline|online …`, the real
//! user, inside mare's Job Object) starts the installed copy as the sandbox account with
//! `CreateProcessWithLogonW`, suspended until assigned to its kill-on-close job. Two authenticated
//! local named pipes carry the complete request and stdio; the logon command line carries only
//! their names. The account runner creates the write-restricted token with capability/account/
//! login/Everyone SIDs and the account integrity level. So reads are limited by the account
//! (plus deny-read ACEs), writes by
//! the capability SIDs, and the network by WFP.
//!
//! **Git Bash**: the inner runner also adds the sandbox account's own SID
//! to the restricting SIDs (`--account`), so the
//! MSYS2 runtime can open the signal pipe it creates with a {user, Administrators, SYSTEM} DACL.
//! That is only safe because the account owns nothing worth protecting: setup gives
//! `%ProgramData%\mare\sandbox`, its `bin` and the helper a protected DACL (SYSTEM and
//! Administrators write, everyone else read).
//! [`lock_violations`] / `win::check_lock` verify it for `mare doctor`, and the self-test proves
//! from inside the sandbox that the accounts can't write there.
//!
//! **Honesty gate**: setup files alone never make the tier "full". The first sandboxed spawn of a
//! process runs a self-test through that exact chain ([`SELFTEST_ARG`]): reading the canary must
//! be denied, writing outside the granted roots must be denied, a loopback port outside the proxy
//! range must be unreachable and one inside it reachable, and the sandbox directory and helper
//! must not be writable. Anything else falls back to the default tier, reported `partial` with
//! the failing check named.

#[cfg(any(windows, test))]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};

/// Local group holding both sandbox accounts (deny-read ACEs target it).
pub const GROUP: &str = "MareSandboxUsers";
/// Account for proxied / blocked network: WFP allows it loopback ports in [`PROXY_PORT_RANGE`] only.
pub const USER_OFFLINE: &str = "mare-sbx-off";
/// Account for policies with an open network (no WFP filters).
pub const USER_ONLINE: &str = "mare-sbx-on";
/// Loopback ports the offline account may reach; mare's network proxy binds inside this range
/// whenever the enhanced tier is active.
pub const PROXY_PORT_RANGE: (u16, u16) = (47_320, 47_419);
/// Setup file format version (a mismatch means "run `mare sandbox setup` again").
/// 2: the sandbox directory, `bin` and the helper carry a protected DACL (the account SID joins
/// the restricting SIDs).
pub const SETUP_VERSION: u64 = 2;
/// argv[1] of the self-test the gate runs inside the sandbox.
pub const SELFTEST_ARG: &str = "__sandbox-selftest";

/// `%ProgramData%\mare\sandbox` (machine-wide: readable without elevation, by every account).
pub fn sandbox_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MARE_SANDBOX_DIR") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("mare").join("sandbox")
}

pub fn setup_file(dir: &Path) -> PathBuf {
    dir.join("setup.json")
}

/// The self-test's scratch workspace, beside (not inside) the locked sandbox directory: the real
/// user creates and grants it, which the locked directory doesn't allow.
pub fn selftest_workspace(dir: &Path) -> PathBuf {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.with_file_name(format!("{name}-selftest"))
}

/// SIDs allowed write access on the sandbox directory, `bin` and the helper: SYSTEM,
/// Administrators, TrustedInstaller.
pub const LOCK_WRITERS: [&str; 3] =
    ["S-1-5-18", "S-1-5-32-544", "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"];

/// Access bits that let a holder change a file or directory: write data / add file, append /
/// add subdirectory, write EAs, delete child, write attributes, DELETE, WRITE_DAC, WRITE_OWNER,
/// GENERIC_WRITE, GENERIC_ALL.
pub const WRITE_BITS: u32 =
    0x0002 | 0x0004 | 0x0010 | 0x0040 | 0x0100 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x4000_0000 | 0x1000_0000;

/// One DACL entry: (ACE type, access mask, SID string).
pub type Ace = (u8, u32, String);

/// What breaks the lock on one path, from its DACL: `protected` = no inherited ACEs. Only allow
/// ACEs (type 0) granting [`WRITE_BITS`] to a SID outside [`LOCK_WRITERS`] count. Empty = locked.
pub fn lock_violations(protected: bool, aces: &[Ace]) -> Vec<String> {
    let mut v = vec![];
    if !protected {
        v.push("the DACL inherits from its parent".to_string());
    }
    for (t, mask, sid) in aces {
        if *t == 0 && mask & WRITE_BITS != 0 && !LOCK_WRITERS.contains(&sid.as_str()) {
            v.push(format!("{sid} may write (mask {mask:#x})"));
        }
    }
    v
}

/// One sandbox account as recorded by setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub sid: String,
    /// DPAPI (machine scope) blob of the password, base64
    pub password: String,
}

/// What `mare sandbox setup` installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupState {
    pub version: u64,
    pub offline: Account,
    pub online: Account,
    pub group_sid: String,
    /// installed copy of the mare executable the sandbox accounts run
    pub helper: PathBuf,
    /// sha256 of the executable at setup time (a newer mare must re-run setup)
    pub helper_sha256: String,
    /// paths that received a deny-read ACE
    pub deny_read: Vec<PathBuf>,
    /// those of them the user configured (`plugins.permissions.deny_read`, `--deny-read`), not the built-in
    /// defaults; `None` = a setup from before the split ([`SetupState::user_deny_read`])
    pub deny_read_user: Option<Vec<PathBuf>>,
    /// file the accounts must not be able to read (self-test)
    pub canary: PathBuf,
    pub proxy_range: (u16, u16),
    /// WFP filters installed for the offline account (0 = network not controlled)
    pub wfp_filters: u64,
}

impl SetupState {
    pub fn to_json(&self) -> Value {
        let acct = |a: &Account| json!({ "name": a.name, "sid": a.sid, "password": a.password });
        json!({
            "version": self.version,
            "offline": acct(&self.offline),
            "online": acct(&self.online),
            "group_sid": self.group_sid,
            "helper": self.helper.to_string_lossy(),
            "helper_sha256": self.helper_sha256,
            "deny_read": self.deny_read.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "deny_read_user": self.deny_read_user.as_ref().map(|v| v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>()),
            "canary": self.canary.to_string_lossy(),
            "proxy_range": [self.proxy_range.0, self.proxy_range.1],
            "wfp_filters": self.wfp_filters,
        })
    }

    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s =
            |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("setup: missing {k}"));
        let acct = |k: &str| -> Result<Account, String> {
            let a = v.get(k).ok_or_else(|| format!("setup: missing {k}"))?;
            let f =
                |n: &str| a.get(n).and_then(Value::as_str).map(str::to_string).ok_or(format!("setup: missing {k}.{n}"));
            Ok(Account { name: f("name")?, sid: f("sid")?, password: f("password")? })
        };
        let range = v
            .get("proxy_range")
            .and_then(Value::as_array)
            .and_then(|a| Some((a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16)))
            .ok_or("setup: missing proxy_range")?;
        Ok(Self {
            version: v.get("version").and_then(Value::as_u64).unwrap_or(0),
            offline: acct("offline")?,
            online: acct("online")?,
            group_sid: s("group_sid")?,
            helper: PathBuf::from(s("helper")?),
            helper_sha256: s("helper_sha256")?,
            deny_read: v
                .get("deny_read")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect())
                .unwrap_or_default(),
            deny_read_user: v
                .get("deny_read_user")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()),
            canary: PathBuf::from(s("canary")?),
            proxy_range: range,
            wfp_filters: v.get("wfp_filters").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        let p = setup_file(dir);
        match std::fs::read_to_string(&p) {
            Ok(t) => {
                let v: Value = serde_json::from_str(mare_shared::strip_utf8_bom(&t))
                    .map_err(|e| format!("{}: {e}", p.display()))?;
                Self::from_json(&v).map(Some)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let body = serde_json::to_string_pretty(&self.to_json()).map_err(|e| e.to_string())?;
        std::fs::write(setup_file(dir), body).map_err(|e| e.to_string())
    }

    /// The deny-read entries the user configured: only a policy's explicit deny entries reach the
    /// Windows ACLs and make escalations stay sandboxed. The built-in `defaults` still get their
    /// ACE but don't count. A setup from before the split recorded no origin: whatever isn't a
    /// default is taken as the user's.
    pub fn user_deny_read(&self, defaults: &[PathBuf]) -> Vec<PathBuf> {
        match &self.deny_read_user {
            Some(v) => v.clone(),
            None => {
                let d: std::collections::HashSet<String> = defaults.iter().map(|p| path_key(p)).collect();
                self.deny_read.iter().filter(|p| !d.contains(&path_key(p))).cloned().collect()
            }
        }
    }

    /// The account for a policy: an open network uses the unfiltered account.
    pub fn account(&self, network_open: bool) -> &Account {
        if network_open { &self.online } else { &self.offline }
    }

    /// Problems that make the installed setup unusable before any self-test.
    pub fn staleness(&self, current_exe_sha256: Option<&str>) -> Option<String> {
        if self.version != SETUP_VERSION {
            return Some(format!(
                "setup version {} (expected {SETUP_VERSION}): run `mare sandbox setup` again",
                self.version
            ));
        }
        if !self.helper.is_file() {
            return Some(format!(
                "sandbox helper {} is missing: run `mare sandbox setup` again",
                self.helper.display()
            ));
        }
        if let Some(h) = current_exe_sha256
            && h != self.helper_sha256
        {
            return Some("mare was updated since `mare sandbox setup`: run it again".into());
        }
        None
    }
}

/// Sensitive paths under `home` that get a deny-read ACE by default (only those that exist are
/// applied: the sandbox accounts can't create files in the user's profile anyway).
pub fn default_deny_read(home: &Path, local_appdata: Option<&Path>, appdata: Option<&Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        ".ssh",
        ".aws",
        ".azure",
        ".gnupg",
        ".kube",
        ".docker",
        ".netrc",
        ".git-credentials",
        ".config/gcloud",
        ".config/gh",
        ".config/mare/auth.json",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect();
    if let Some(l) = local_appdata {
        for p in [r"Google\Chrome\User Data", r"Microsoft\Edge\User Data", r"BraveSoftware\Brave-Browser\User Data"] {
            v.push(l.join(p));
        }
    }
    if let Some(r) = appdata {
        for p in [r"Mozilla\Firefox\Profiles", r"Microsoft\Credentials", r"gnupg"] {
            v.push(r.join(p));
        }
    }
    v
}

/// [`default_deny_read`] for this process's profile (`%USERPROFILE%`, `%LOCALAPPDATA%`,
/// `%APPDATA%`).
pub fn env_default_deny_read() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let home = env("USERPROFILE").or_else(|| env("HOME")).unwrap_or_default();
    default_deny_read(&home, env("LOCALAPPDATA").as_deref(), env("APPDATA").as_deref())
}

/// Case-insensitive comparison key of a Windows path.
pub fn path_key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase()
}

/// The deny list: defaults (unless disabled) plus configured extras, de-duplicated
/// case-insensitively (Windows paths).
pub fn deny_read_list(defaults: Vec<PathBuf>, extra: &[PathBuf], use_defaults: bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    let mut seen = std::collections::HashSet::new();
    let base = if use_defaults { defaults } else { vec![] };
    for p in base.into_iter().chain(extra.iter().cloned()) {
        if seen.insert(path_key(&p)) {
            out.push(p);
        }
    }
    out
}

/// A port inside the proxy range (for the proxy to bind).
pub fn in_proxy_range(port: u16) -> bool {
    (PROXY_PORT_RANGE.0..=PROXY_PORT_RANGE.1).contains(&port)
}

/// The enhanced helper inherits the parent's window station. There is no station transfer
/// implementation, so reject unsupported stations before creating a run-as-account process.
#[cfg(any(windows, test))]
pub(crate) fn check_enhanced_station(station: &str) -> Result<(), String> {
    if station.eq_ignore_ascii_case("WinSta0") {
        Ok(())
    } else {
        Err(format!(
            "enhanced sandbox requires WinSta0; parent window station is {station}; starting the enhanced helper in this station is unsupported"
        ))
    }
}

// ---- honesty gate ----------------------------------------------------------------------------

/// One self-test check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Pass,
    Fail(String),
}

/// Result of the self-test run inside the sandbox (as the offline account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfTest {
    pub canary_read_denied: Check,
    pub outside_write_denied: Check,
    pub outside_port_blocked: Check,
    pub proxy_port_reachable: Check,
    /// the sandbox directory, its `bin` and the helper are not writable (the account
    /// SID is a restricting SID, so nothing the account may write can matter)
    pub sandbox_write_denied: Check,
}

impl SelfTest {
    /// The checks by name, in report order.
    pub fn checks(&self) -> [(&'static str, &Check); 5] {
        [
            ("deny-read canary", &self.canary_read_denied),
            ("write outside the writable roots", &self.outside_write_denied),
            ("loopback port outside the proxy range", &self.outside_port_blocked),
            ("proxy port reachable", &self.proxy_port_reachable),
            ("write to the sandbox directory or helper", &self.sandbox_write_denied),
        ]
    }

    /// Parse the JSON line the in-sandbox self-test prints.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let c = |k: &str| -> Result<Check, String> {
            let x = v.get(k).ok_or_else(|| format!("self-test output lacks {k}"))?;
            Ok(if x.get("ok").and_then(Value::as_bool) == Some(true) {
                Check::Pass
            } else {
                Check::Fail(x.get("detail").and_then(Value::as_str).unwrap_or("failed").to_string())
            })
        };
        Ok(Self {
            canary_read_denied: c("canary_read_denied")?,
            outside_write_denied: c("outside_write_denied")?,
            outside_port_blocked: c("outside_port_blocked")?,
            proxy_port_reachable: c("proxy_port_reachable")?,
            sandbox_write_denied: c("sandbox_write_denied")?,
        })
    }
}

/// Runs the self-test through the real sandbox chain (Windows) — or a mock in tests.
pub trait SelfTester {
    /// Check host capabilities before attempting to launch the enhanced helper.
    fn check_environment(&self) -> Result<(), String> {
        Ok(())
    }
    /// The installed setup, if any (`Err` = present but unreadable).
    fn setup(&self) -> Result<Option<SetupState>, String>;
    /// sha256 of the running executable (to detect a mare newer than the setup)
    fn current_exe_sha256(&self) -> Option<String>;
    fn run(&self, setup: &SetupState) -> Result<SelfTest, String>;
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// no enhanced setup installed: the default tier, nothing to report
    NotConfigured,
    /// self-test passed: report `full` for reads and network
    Full(Box<SetupState>),
    /// installed but not trustworthy: the default tier, with the reason (failing checks named)
    Fallback(String),
}

pub fn evaluate(t: &dyn SelfTester) -> Gate {
    let setup = match t.setup() {
        Ok(Some(s)) => s,
        Ok(None) => return Gate::NotConfigured,
        Err(e) => return Gate::Fallback(format!("enhanced tier unusable: {e}")),
    };
    if let Err(why) = t.check_environment() {
        return Gate::Fallback(format!("enhanced tier unusable: {why}"));
    }
    if let Some(why) = setup.staleness(t.current_exe_sha256().as_deref()) {
        return Gate::Fallback(format!("enhanced tier unusable: {why}"));
    }
    if setup.wfp_filters == 0 {
        return Gate::Fallback("enhanced tier self-test not run: no WFP filters were installed".into());
    }
    match t.run(&setup) {
        Err(e) => Gate::Fallback(format!("enhanced tier self-test could not run: {e}")),
        Ok(r) => {
            let failed: Vec<String> = r
                .checks()
                .iter()
                .filter_map(|(name, c)| match c {
                    Check::Pass => None,
                    Check::Fail(d) => Some(format!("{name} ({d})")),
                })
                .collect();
            if failed.is_empty() {
                Gate::Full(Box::new(setup))
            } else {
                Gate::Fallback(format!("enhanced tier self-test failed: {}", failed.join("; ")))
            }
        }
    }
}

/// The gate for this process: evaluated once, on first use (it spawns the self-test).
pub fn gate() -> &'static Gate {
    static G: OnceLock<Gate> = OnceLock::new();
    G.get_or_init(|| {
        #[cfg(windows)]
        let g = evaluate(&win::RealTester);
        #[cfg(not(windows))]
        let g = Gate::NotConfigured;
        if let Gate::Fallback(why) = &g {
            tracing::warn!("windows sandbox: {why}; using the default tier");
        }
        g
    })
}

/// The account to run as when the gate passed (`None` = default tier).
pub fn active_account(network_open: bool) -> Option<&'static Account> {
    match gate() {
        Gate::Full(s) => Some(s.account(network_open)),
        _ => None,
    }
}

/// The active enhanced tier denies reads the user configured (not only the built-in defaults):
/// an approved escalation must then stay in the sandbox ([`crate::deny_read::keep`]).
pub fn user_deny_read_active() -> bool {
    match gate() {
        Gate::Full(s) => !s.user_deny_read(&env_default_deny_read()).is_empty(),
        _ => false,
    }
}

/// Whether the active enhanced tier's setup ACE list covers every `Dir` entry with `Refuse`
/// gap action in [`crate::ossandbox::protected::host_protected`].
pub fn host_protect_covered() -> bool {
    match gate() {
        Gate::Full(s) => {
            let covered: std::collections::HashSet<String> = s.deny_read.iter().map(|p| path_key(p)).collect();
            crate::ossandbox::protected::host_protected()
                .into_iter()
                .filter(|e| {
                    e.shape == crate::ossandbox::protected::HostShape::Dir
                        && e.on_gap == crate::ossandbox::protected::GapAction::Refuse
                })
                .all(|e| covered.contains(&path_key(&e.path)))
        }
        _ => false,
    }
}

/// Why the enhanced tier isn't in use, for reports (`None` when not configured or active).
pub fn fallback_reason() -> Option<&'static str> {
    match gate() {
        Gate::Fallback(why) => Some(why.as_str()),
        _ => None,
    }
}

pub fn sha256_file(p: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(p).ok()?;
    Some(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

fn is_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5)
}

/// Try every write the sandbox accounts must not manage on the sandbox directory `dir` and the
/// helper; `Ok` = all denied. A probe that gets through is undone at once. The helper is probed
/// for its DACL and attributes rather than its data: it is the running image, so a data open
/// fails with a sharing violation whatever the ACL says.
pub fn probe_sandbox_writes(dir: &Path, helper: &Path) -> Result<(), String> {
    let mut got_through = vec![];
    for d in [dir.to_path_buf(), dir.join("bin")] {
        let f = d.join(format!(".mare-selftest-{}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&f) {
            Ok(_) => {
                let _ = std::fs::remove_file(&f);
                got_through.push(format!("created {}", f.display()));
            }
            Err(e) if is_denied(&e) => {}
            Err(e) => got_through.push(format!("{}: unexpected error: {e}", f.display())),
        }
    }
    let mut o = std::fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const WRITE_DAC: u32 = 0x0004_0000;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        o.access_mode(WRITE_DAC | FILE_WRITE_ATTRIBUTES);
    }
    #[cfg(not(windows))]
    o.append(true);
    match o.open(helper) {
        Ok(_) => got_through.push(format!("opened {} for writing", helper.display())),
        Err(e) if is_denied(&e) => {}
        Err(e) => got_through.push(format!("{}: unexpected error: {e}", helper.display())),
    }
    if got_through.is_empty() { Ok(()) } else { Err(got_through.join("; ")) }
}

/// `mare __sandbox-selftest <canary> <outside-file> <outside-port> <proxy-port> <sandbox-dir>
/// <helper>`: runs inside the sandbox, prints one JSON line with each check's outcome. Every
/// check expects the sandbox to do its job, so "ok" means "confined as promised".
pub fn selftest_main(args: &[String]) -> i32 {
    match selftest_checks(args) {
        Some(v) => {
            println!("{v}");
            0
        }
        None => {
            eprintln!(
                "usage: mare {SELFTEST_ARG} <canary> <outside-file> <outside-port> <proxy-port> <sandbox-dir> <helper>"
            );
            2
        }
    }
}

fn selftest_checks(args: &[String]) -> Option<Value> {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let [canary, outside, out_port, proxy_port, sandbox, helper] = args else {
        return None;
    };
    let denied = is_denied;
    let check = |ok: bool, detail: String| json!({ "ok": ok, "detail": detail });
    let canary_r = match std::fs::read(canary) {
        Ok(_) => check(false, "the canary was readable".into()),
        Err(e) if denied(&e) => check(true, "access denied".into()),
        Err(e) => check(false, format!("unexpected error: {e}")),
    };
    let outside_r = match std::fs::write(outside, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(outside);
            check(false, "wrote outside the writable roots".into())
        }
        Err(e) if denied(&e) => check(true, "access denied".into()),
        Err(e) => check(false, format!("unexpected error: {e}")),
    };
    let connect = |port: &str| -> Result<(), String> {
        let port: u16 = port.parse().map_err(|_| format!("bad port {port}"))?;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map(|_| ()).map_err(|e| e.to_string())
    };
    let out_r = match connect(out_port) {
        Ok(()) => check(false, "connected to a loopback port outside the proxy range".into()),
        Err(e) => check(true, e),
    };
    let proxy_r = match connect(proxy_port) {
        Ok(()) => check(true, "connected".into()),
        Err(e) => check(false, e),
    };
    let sandbox_r = match probe_sandbox_writes(Path::new(sandbox), Path::new(helper)) {
        Ok(()) => check(true, "access denied".into()),
        Err(e) => check(false, e),
    };
    Some(json!({
        "canary_read_denied": canary_r,
        "outside_write_denied": outside_r,
        "outside_port_blocked": out_r,
        "proxy_port_reachable": proxy_r,
        "sandbox_write_denied": sandbox_r,
    }))
}

#[cfg(windows)]
#[path = "enhanced_win.rs"]
pub mod win; // used by `mare sandbox` (mare-cli)

/// Resulting DACL operation when the effective union mask for `(path, group_sid)` changes.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrantTransition {
    SetMask(u32),
    Revoke,
}

/// Reference-counted per-`(path_key, group_sid)` mask tracker. Computes the exact effective bitmask
/// union so releasing a broader grant while a narrower grant remains downgrades the ACE via
/// `SET_ACCESS` instead of leaving the broader mask in place.
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct GroupGrantTracker {
    active: HashMap<(String, String), HashMap<u32, usize>>,
}

#[cfg(any(windows, test))]
impl GroupGrantTracker {
    fn union(counts: &HashMap<u32, usize>) -> u32 {
        counts.keys().fold(0u32, |acc, &m| acc | m)
    }

    /// Record a grant of `mask` on `(path_key, group_sid)`. Returns `(was_empty, new_effective)`
    /// when the effective union mask increased and the DACL must be rewritten with `SET_ACCESS`.
    pub(crate) fn acquire(&mut self, path_key: String, group_sid: String, mask: u32) -> (bool, Option<u32>) {
        let entry = self.active.entry((path_key, group_sid)).or_default();
        let before = Self::union(entry);
        *entry.entry(mask).or_insert(0) += 1;
        let after = Self::union(entry);
        (before == 0, (after != before).then_some(after))
    }

    /// Release one reference to `mask` on `(path_key, group_sid)`. Returns the DACL transition
    /// needed if the effective union mask shrank or dropped to zero.
    pub(crate) fn release(&mut self, path_key: &str, group_sid: &str, mask: u32) -> Option<GrantTransition> {
        let key = (path_key.to_string(), group_sid.to_string());
        let entry = self.active.get_mut(&key)?;
        let before = Self::union(entry);
        if let Some(count) = entry.get_mut(&mask) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.remove(&mask);
            }
        }
        let after = Self::union(entry);
        if entry.is_empty() {
            self.active.remove(&key);
        }
        if after == before {
            None
        } else if after == 0 {
            Some(GrantTransition::Revoke)
        } else {
            Some(GrantTransition::SetMask(after))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> SetupState {
        let helper = dir.join("mare-sandbox.exe");
        std::fs::write(&helper, b"helper").unwrap();
        let a =
            |n: &str| Account { name: n.into(), sid: format!("S-1-5-21-1-2-3-{}", n.len()), password: "AAA=".into() };
        SetupState {
            version: SETUP_VERSION,
            offline: a(USER_OFFLINE),
            online: a(USER_ONLINE),
            group_sid: "S-1-5-21-9".into(),
            helper: helper.clone(),
            helper_sha256: sha256_file(&helper).unwrap(),
            deny_read: vec![dir.join(".ssh")],
            deny_read_user: Some(vec![]),
            canary: dir.join("canary.txt"),
            proxy_range: PROXY_PORT_RANGE,
            wfp_filters: 4,
        }
    }

    struct Mock {
        setup: Result<Option<SetupState>, String>,
        exe: Option<String>,
        run: Result<SelfTest, String>,
    }
    impl SelfTester for Mock {
        fn setup(&self) -> Result<Option<SetupState>, String> {
            self.setup.clone()
        }
        fn current_exe_sha256(&self) -> Option<String> {
            self.exe.clone()
        }
        fn run(&self, _: &SetupState) -> Result<SelfTest, String> {
            self.run.clone()
        }
    }

    fn all_pass() -> SelfTest {
        SelfTest {
            canary_read_denied: Check::Pass,
            outside_write_denied: Check::Pass,
            outside_port_blocked: Check::Pass,
            proxy_port_reachable: Check::Pass,
            sandbox_write_denied: Check::Pass,
        }
    }

    #[test]
    fn setup_state_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        s.save(d.path()).unwrap();
        assert_eq!(SetupState::load(d.path()).unwrap(), Some(s.clone()));
        assert_eq!(s.account(true).name, USER_ONLINE);
        assert_eq!(s.account(false).name, USER_OFFLINE);
        assert_eq!(SetupState::load(&d.path().join("none")).unwrap(), None);
    }

    /// DENYREAD2 option B: only the user's own deny-read entries count, recorded apart from the
    /// built-in defaults; a setup from before the split counts whatever isn't a default.
    #[test]
    fn only_user_deny_read_entries_count() {
        let d = tempfile::tempdir().unwrap();
        let home = Path::new("/home/u");
        let defaults = default_deny_read(home, None, None);
        let mut s = state(d.path());
        s.deny_read = vec![home.join(".ssh"), home.join(".aws")];
        s.deny_read_user = Some(vec![]);
        assert!(s.user_deny_read(&defaults).is_empty(), "defaults only");
        // a user entry, even one that is also a default, counts once recorded as the user's
        s.deny_read.push(PathBuf::from("/data/secret"));
        s.deny_read_user = Some(vec![home.join(".ssh"), PathBuf::from("/data/secret")]);
        assert_eq!(s.user_deny_read(&defaults).len(), 2);
        s.save(d.path()).unwrap();
        assert_eq!(SetupState::load(d.path()).unwrap().unwrap().deny_read_user, s.deny_read_user);
        // legacy file (no `deny_read_user`): non-defaults are the user's, compared case-insensitively
        let mut v = s.to_json();
        v.as_object_mut().unwrap().remove("deny_read_user");
        let mut legacy = SetupState::from_json(&v).unwrap();
        assert_eq!(legacy.deny_read_user, None);
        assert_eq!(legacy.user_deny_read(&defaults), vec![PathBuf::from("/data/secret")]);
        legacy.deny_read = vec![PathBuf::from("/HOME/u/.SSH")];
        assert!(legacy.user_deny_read(&defaults).is_empty());
    }

    #[test]
    fn gate_requires_a_passing_self_test() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let sha = s.helper_sha256.clone();
        let mk = |setup, exe: Option<&str>, run| Mock { setup, exe: exe.map(str::to_string), run };

        // nothing installed: default tier, no complaint
        assert_eq!(evaluate(&mk(Ok(None), None, Ok(all_pass()))), Gate::NotConfigured);
        // everything passes: full
        assert!(matches!(evaluate(&mk(Ok(Some(s.clone())), Some(&sha), Ok(all_pass()))), Gate::Full(_)));
        // one failing check: fallback naming it — never full on setup files alone
        let mut r = all_pass();
        r.outside_port_blocked = Check::Fail("connected".into());
        match evaluate(&mk(Ok(Some(s.clone())), Some(&sha), Ok(r))) {
            Gate::Fallback(why) => assert!(why.contains("loopback port outside the proxy range"), "{why}"),
            g => panic!("{g:?}"),
        }
        // the account could write the sandbox directory or the helper: never full
        let mut r = all_pass();
        r.sandbox_write_denied = Check::Fail(r"created C:\ProgramData\mare\sandbox\x".into());
        match evaluate(&mk(Ok(Some(s.clone())), Some(&sha), Ok(r))) {
            Gate::Fallback(why) => assert!(why.contains("sandbox directory or helper"), "{why}"),
            g => panic!("{g:?}"),
        }
        // the self-test can't run (logon failure…): fallback
        assert!(matches!(evaluate(&mk(Ok(Some(s.clone())), Some(&sha), Err("logon".into()))), Gate::Fallback(_)));
        // mare updated since setup: fallback before any self-test
        match evaluate(&mk(Ok(Some(s.clone())), Some("other"), Ok(all_pass()))) {
            Gate::Fallback(why) => assert!(why.contains("updated"), "{why}"),
            g => panic!("{g:?}"),
        }
        // no WFP filters installed: network uncontrolled, so never full
        let mut nw = s.clone();
        nw.wfp_filters = 0;
        assert!(matches!(evaluate(&mk(Ok(Some(nw)), Some(&sha), Ok(all_pass()))), Gate::Fallback(_)));
        // unreadable setup / old version: fallback
        assert!(matches!(evaluate(&mk(Err("bad json".into()), None, Ok(all_pass()))), Gate::Fallback(_)));
        let mut old = s;
        old.version = 0;
        assert!(matches!(evaluate(&mk(Ok(Some(old)), Some(&sha), Ok(all_pass()))), Gate::Fallback(_)));
    }

    #[test]
    fn selftest_output_parses() {
        let v = json!({
            "canary_read_denied": { "ok": true, "detail": "access denied" },
            "outside_write_denied": { "ok": false, "detail": "wrote outside" },
            "outside_port_blocked": { "ok": true },
            "proxy_port_reachable": { "ok": true },
            "sandbox_write_denied": { "ok": true },
        });
        let t = SelfTest::from_json(&v).unwrap();
        assert_eq!(t.outside_write_denied, Check::Fail("wrote outside".into()));
        assert!(SelfTest::from_json(&json!({})).is_err());
        // output without the sandbox-write check (an older helper) is not a pass
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("sandbox_write_denied");
        assert!(SelfTest::from_json(&old).is_err());
    }

    #[test]
    fn selftest_detects_an_unconfined_run() {
        // run unconfined (as here): every confinement check must fail, the reachable one pass
        let d = tempfile::tempdir().unwrap();
        let canary = d.path().join("canary");
        std::fs::write(&canary, b"secret").unwrap();
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = open.local_addr().unwrap().port().to_string();
        let sandbox = d.path().join("sandbox");
        std::fs::create_dir_all(sandbox.join("bin")).unwrap();
        let helper = sandbox.join("bin").join("mare-sandbox.exe");
        std::fs::write(&helper, b"helper").unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let args = [s(&canary), s(&d.path().join("out")), port.clone(), port, s(&sandbox), s(&helper)];
        let t = SelfTest::from_json(&selftest_checks(&args).unwrap()).unwrap();
        assert!(matches!(t.canary_read_denied, Check::Fail(_)));
        assert!(matches!(t.outside_write_denied, Check::Fail(_)));
        assert!(matches!(t.outside_port_blocked, Check::Fail(_)));
        assert_eq!(t.proxy_port_reachable, Check::Pass);
        match &t.sandbox_write_denied {
            Check::Fail(d) => assert!(d.contains("created") && d.contains("opened"), "{d}"),
            c => panic!("{c:?}"),
        }
        assert_eq!(std::fs::read(&helper).unwrap(), b"helper", "the probe never changes the helper");
        assert_eq!(std::fs::read_dir(&sandbox).unwrap().count(), 1, "probe files are removed");
        assert_eq!(std::fs::read_dir(sandbox.join("bin")).unwrap().count(), 1, "probe files are removed");
        assert_eq!(selftest_main(&args[..4]), 2, "the old four-argument form is refused");
    }

    #[test]
    fn lock_allows_only_system_and_administrators_to_write() {
        let ace = |t: u8, m: u32, s: &str| (t, m, s.to_string());
        let read = 0x0012_0089 | 0x0012_00A0;
        let locked =
            [ace(0, 0x001F_01FF, "S-1-5-18"), ace(0, 0x001F_01FF, "S-1-5-32-544"), ace(0, read, "S-1-5-32-545")];
        assert!(lock_violations(true, &locked).is_empty());
        // ProgramData's inherited "Users may create files" ACE, or any write bit for another SID
        let mut open = locked.to_vec();
        open.push(ace(0, 0x0002 | 0x0004, "S-1-5-32-545"));
        open.push(ace(0, 0x0004_0000, "S-1-5-21-1-2-3-1001"));
        let v = lock_violations(true, &open);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].starts_with("S-1-5-32-545") && v[1].starts_with("S-1-5-21-1-2-3-1001"), "{v:?}");
        // deny ACEs never open anything; an inheriting DACL is a violation of its own
        assert!(lock_violations(true, &[ace(1, 0x001F_01FF, "S-1-1-0")]).is_empty());
        assert_eq!(lock_violations(false, &locked).len(), 1);
    }

    #[test]
    fn selftest_workspace_sits_beside_the_sandbox_directory() {
        let d = Path::new("/ProgramData/mare/sandbox");
        assert_eq!(selftest_workspace(d), Path::new("/ProgramData/mare/sandbox-selftest"));
    }

    #[test]
    fn deny_list_merges_and_dedups() {
        let home = Path::new("/home/u");
        let d = default_deny_read(home, Some(Path::new("/l")), Some(Path::new("/r")));
        assert!(d.contains(&home.join(".ssh")));
        let extra = vec![PathBuf::from("/home/u/.SSH"), PathBuf::from("/data/secret")];
        let l = deny_read_list(d.clone(), &extra, true);
        assert_eq!(l.len(), d.len() + 1, "case-insensitive duplicate dropped");
        assert_eq!(deny_read_list(d, &extra, false), extra);
        assert!(in_proxy_range(PROXY_PORT_RANGE.0) && !in_proxy_range(PROXY_PORT_RANGE.1 + 1));
    }

    #[test]
    fn group_grant_tracker_downgrades_effective_mask_when_broader_grant_drops() {
        const READ: u32 = 0x0012_00A9;
        const WRITE_ONLY: u32 = 0x0011_0156;
        const MODIFY: u32 = READ | WRITE_ONLY;

        let mut tracker = GroupGrantTracker::default();
        assert_eq!(tracker.acquire("p".into(), "sid".into(), MODIFY), (true, Some(MODIFY)));
        // Overlapping narrower read grant does not change the effective union.
        assert_eq!(tracker.acquire("p".into(), "sid".into(), READ), (false, None));
        // Dropping MODIFY while READ remains must downgrade the ACE to READ.
        assert_eq!(tracker.release("p", "sid", MODIFY), Some(GrantTransition::SetMask(READ)));
        // Overlapping write-only grant widens union to READ | WRITE_ONLY.
        assert_eq!(tracker.acquire("p".into(), "sid".into(), WRITE_ONLY), (false, Some(MODIFY)));
        // Dropping READ while WRITE_ONLY remains must strip read bits back down to WRITE_ONLY.
        assert_eq!(tracker.release("p", "sid", READ), Some(GrantTransition::SetMask(WRITE_ONLY)));
        // Dropping the last grant revokes the ACE entirely.
        assert_eq!(tracker.release("p", "sid", WRITE_ONLY), Some(GrantTransition::Revoke));
        assert_eq!(tracker.release("p", "sid", WRITE_ONLY), None);
    }
}

#[cfg(test)]
#[path = "enhanced_station_tests.rs"]
mod station_tests;
