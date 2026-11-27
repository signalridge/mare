//! Shared test support (after Codex `core/tests/common`): a per-test environment with its own
//! MARE_HOME and workspace, a Host builder, Lua helpers and a tiny scripted HTTP/SSE server.
//!//! MARE_HOME (and a few MARE_* / API-key variables) are process-wide. Nextest starts each test
//! in a private environment; [`TestEnv`] serializes temporary overrides within that process.
#![allow(dead_code)]

mod builtins;
mod pty;
pub use pty::{MareCmd, PtySession, render_screen, tail};
pub mod cassette;
pub mod rpc;
pub mod scripted;
mod scripted_render;

pub mod barrier;
mod git;
mod mock_server;
mod plugins;
mod process;

pub use barrier::*;
pub use git::*;
pub use mock_server::*;
pub use plugins::*;
pub use process::*;
pub use rpc::{RpcClient, RpcMessageBuffer};

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;
use serde_json::{Value, json};

/// Lua is !Send: everything touching the VM runs on a LocalSet (actor model).
pub async fn local<F: Future>(f: F) -> F::Output {
    tokio::task::LocalSet::new().run_until(f).await
}

fn env_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(Default::default).clone()
}

/// A temp MARE_HOME + workspace, holding the process-wide environment lock for its lifetime.
pub struct TestEnv {
    pub home: tempfile::TempDir,
    pub work: tempfile::TempDir,
    /// canonical workspace path (the host's cwd)
    pub cwd: PathBuf,
    saved_vars: Vec<(String, Option<std::ffi::OsString>)>,
    keep_home: bool,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

fn ensure_test_offline() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() != Ok("1") {
            mare_binding::http::enable_test_offline();
        }
    });
}

/// Build a child `mare` `Command` with `MARE_TEST_OFFLINE=1` forwarded in the child environment
/// (unless `MARE_TEST_ALLOW_LIVE=1`), avoiding process-wide `set_var`.
pub fn mare_cmd() -> std::process::Command {
    ensure_test_offline();
    let mut cmd = std::process::Command::new(mare_bin());
    cmd.env("BROWSER", "true");
    if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() != Ok("1") {
        cmd.env("MARE_TEST_OFFLINE", "1");
    }
    cmd
}

impl TestEnv {
    pub async fn new() -> Self {
        ensure_test_offline();
        mare_shared::ui::install_hooks(mare_tui::hooks());
        let lock = env_lock().lock_owned().await;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(work.path()).unwrap();
        let mut env = Self { home, work, cwd, saved_vars: vec![], keep_home: false, _lock: lock };
        let h = env.home.path().to_path_buf();
        env.set_var("MARE_HOME", h);
        env.set_var("BROWSER", "true");
        env
    }

    /// Keep diagnostic files in MARE_HOME while still closing its index and cleaning work.
    pub fn preserve_home(&mut self) {
        self.keep_home = true;
        self.home.disable_cleanup(true);
    }

    /// Set an environment variable for this test only, restoring its original value on drop.
    pub fn set_var(&mut self, k: &str, v: impl AsRef<std::ffi::OsStr>) {
        if !self.saved_vars.iter().any(|(name, _)| name == k) {
            self.saved_vars.push((k.to_string(), std::env::var_os(k)));
        }
        // SAFETY: tests touching the environment hold the global lock (or run in their own process)
        unsafe { std::env::set_var(k, v) };
    }

    /// Fix terminal capabilities for a test and restore the caller's environment on drop.
    pub fn color_terminal(&mut self) {
        for (name, value) in
            [("NO_COLOR", ""), ("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("MARE_COLOR", "truecolor")]
        {
            self.set_var(name, value);
        }
    }

    pub fn host(&self) -> HostBuilder {
        HostBuilder {
            opts: HostOptions {
                cwd: self.cwd.clone(),
                mode: Mode::Print,
                // Tests opt in to non-minimal plugins; permissions remains off unless explicitly enabled.
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "preset": "minimal", "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            },
        }
    }

    /// `~/.config/mare` of this test (init.lua, plugins/)
    pub fn config_dir(&self) -> PathBuf {
        self.home.path().join("config")
    }

    /// Write a fixture and establish its durable prefix through the production writer.
    pub fn write_durable_session(&self, name: &str, entries: &[Value]) -> PathBuf {
        let path = self.write_session(name, entries);
        drop(mare_store::SessionFile::open_existing(&path, None).unwrap());
        path
    }

    /// Prepare a replacement with a real fsynced watermark in the source lock generation.
    pub fn durable_replacement(&self, source: &Path, contents: &str) -> PathBuf {
        let replacement = source.with_extension("replacement");
        std::fs::write(&replacement, contents).unwrap();
        std::fs::hard_link(mare_store::lock_path(source), mare_store::lock_path(&replacement)).unwrap();
        drop(mare_store::SessionFile::open_existing(&replacement, None).unwrap());
        replacement
    }

    /// Write a raw session fixture for this workspace, without a durable watermark.
    pub fn write_session(&self, name: &str, entries: &[Value]) -> PathBuf {
        let dir = self.home.path().join("state/sessions").join(mare_store::cwd_slug(&self.cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let mut out =
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "s1", "cwd": self.cwd })
                .to_string();
        out.push('\n');
        for e in entries {
            out.push_str(&e.to_string());
            out.push('\n');
        }
        let p = dir.join(name);
        std::fs::write(&p, out).unwrap();
        p
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let mut cleanup_errors = Vec::new();
        if let Err(error) = mare_store::index::close_idle(&self.home.path().join("state/sessions")) {
            cleanup_errors.push(format!("TestEnv index cleanup: {error}"));
        }
        for (dir, keep) in [(&self.home, self.keep_home), (&self.work, false)] {
            if keep {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(dir.path()) {
                let entries = std::fs::read_dir(dir.path())
                    .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect::<Vec<_>>());
                let message =
                    format!("TestEnv cleanup {}: {error}; remaining entries: {entries:?}", dir.path().display());
                cleanup_errors.push(message);
            }
        }
        for (name, value) in self.saved_vars.iter().rev() {
            // SAFETY: TestEnv still holds the environment lock while restoring the caller's values.
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
        if !cleanup_errors.is_empty() {
            let message = cleanup_errors.join("\n");
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

pub struct HostBuilder {
    pub opts: HostOptions,
}

impl HostBuilder {
    /// Merge into the config (objects merge recursively).
    pub fn config(mut self, v: Value) -> Self {
        merge(&mut self.opts.config, v);
        self
    }
    /// Scripted faux model: each entry is one assistant turn (`text`, `thinking`, `tool_calls`,
    /// `error`/`retryable`/`kind`, `delay_ms`).
    pub fn faux(self, script: Value) -> Self {
        self.config(
            json!({ "model": "faux/echo", "plugins": { "provider-faux": { "script_json": script.to_string() } } }),
        )
    }
    pub fn args(mut self, v: Value) -> Self {
        self.opts.args = v;
        self
    }
    pub fn no_session(self) -> Self {
        self.args(json!({ "no_session": true }))
    }
    pub fn resume(self) -> Self {
        self.args(json!({ "continue": true }))
    }
    /// Load ~/.config/mare/init.lua (+ user plugins); the config passed in is the CLI layer.
    pub fn user_config(mut self, project_trusted: bool) -> Self {
        self.opts.load_init = true;
        self.opts.no_user_plugins = false;
        self.opts.project_trusted = project_trusted;
        self.opts.config = json!({});
        self
    }
    pub fn user_plugins(mut self) -> Self {
        self.opts.no_user_plugins = false;
        self
    }
    pub fn project_trusted(mut self, trusted: bool) -> Self {
        self.opts.project_trusted = trusted;
        self
    }
    pub async fn build(self) -> Arc<Host> {
        let h = Host::new(self.opts).await.unwrap();
        assert!(h.kernel.problems().is_empty(), "plugin problems: {:?}", h.kernel.problems());
        h
    }
    /// Build a host with `S`, `A`, `T`, `C` globals exposed to Lua snippets.
    pub async fn build_exposed(self) -> Arc<Host> {
        let h = self.build().await;
        expose(&h);
        h
    }
    /// Build a host with `S`, `A`, `T`, `C` globals exposed without failing on plugin problems.
    pub async fn build_unchecked_exposed(self) -> Arc<Host> {
        let h = Host::new(self.opts).await.unwrap();
        expose(&h);
        h
    }
}

pub fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

pub async fn eval<T: mlua::FromLuaMulti>(h: &Host, code: &str) -> T {
    h.lua().load(code).set_name("=test").eval_async::<T>().await.unwrap()
}

pub async fn exec(h: &Host, code: &str) {
    h.lua().load(code).set_name("=test").exec_async().await.unwrap()
}

pub fn global(h: &Host, name: &str, v: impl mlua::IntoLua) {
    h.lua().globals().set(name, v).unwrap();
}

/// Expose `S`ession, `A`gent, `T`ools and a root `C`tx to Lua snippets.
pub fn expose(h: &Host) {
    for (name, svc) in [("S", "session"), ("A", "agent"), ("T", "tools")] {
        global(h, name, h.service_table(svc).unwrap());
    }
    global(h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
    h.kernel.expose_test_boot();
}

/// Ceiling, in seconds, for an "eventually" wait: it exists only to catch a hang, so it is
/// generous enough for a loaded CI runner. A wait whose bound *is* the behaviour under test
/// (e.g. "the abort stops promptly") keeps this ceiling and asserts latency separately with an
/// `Instant` measured from the triggering action.
pub const WAIT_SECS: u64 = 60;

/// Poll `f` every 20ms until it returns true (panics with `what` after `secs`).
/// Uses tokio::time::Instant so callers can run under real time or `start_paused = true`.
pub async fn wait_until<F, Fut>(secs: u64, what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let t0 = tokio::time::Instant::now();
    while !f().await {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// All text blocks of all tool results in the agent's context, joined.
pub async fn tool_results(h: &Host) -> String {
    eval(
        h,
        r#"local out = {}
           for _, m in ipairs(A:list_messages().data) do
             if m.role == "tool_result" then
               for _, b in ipairs(m.content) do out[#out + 1] = b.text or "" end
             end
           end
           return table.concat(out, "\n---\n")"#,
    )
    .await
}

/// A model shell command that creates the file `p`, the same on every platform: `echo x > 'p'`
/// runs in bash, in Git Bash and in PowerShell (the Windows model shell inside the sandbox), and
/// `/` separators keep the path intact where `\` would be an escape.
pub fn create_file_cmd(p: &Path) -> String {
    format!("echo x > '{}'", p.to_string_lossy().replace('\\', "/"))
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    json!({ "type": "message", "entry_id": id, "parent_entry_id": parent,
            "message": { "role": role, "content": [{ "type": "text", "text": text }] } })
}

/// A durable completion marker for a fixture whose session header uses the default `s1` id.
pub fn run_end(leaf: &str) -> Value {
    json!({
        "type": "custom",
        "kind": "agent/run-end",
        "data": { "session_id": "s1", "run_id": "fixture-run", "leaf_entry_id": leaf },
    })
}

/// scenario closure helper: assert every file under `home/{state,cache}` matches a registered
/// artifact row and expected mode bits.
pub fn assert_registered(home: &Path) {
    mare_host::artifacts::assert_registered(home).unwrap_or_else(|e| panic!("{e}"));
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn nextest_process_without_testenv_uses_an_isolated_home() {
        let mare_home = PathBuf::from(std::env::var_os("MARE_HOME").expect("MARE_HOME"));
        assert_eq!(std::env::var("MARE_NEXTEST_TEST_HOME").as_deref(), Ok("1"), "nextest wrapper did not run");
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"), "wrapper must run exactly once");
        let root = mare_home.parent().unwrap();
        assert_eq!(mare_home, root.join("mare"));
        let root_real = dunce::canonicalize(root).unwrap();
        assert!(root.file_name().unwrap().to_string_lossy().starts_with("mn-"));
        for (name, dir) in [
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("XDG_STATE_HOME", "xdg-state"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
            ("APPDATA", "appdata"),
            ("LOCALAPPDATA", "local-appdata"),
        ] {
            assert_eq!(std::env::var_os(name).map(PathBuf::from), Some(root.join(dir)), "{name}");
        }
        let home = dunce::canonicalize(root.join("home")).unwrap();
        let temp = dunce::canonicalize(std::env::var_os("TMPDIR").expect("isolated TMPDIR")).unwrap();
        for name in ["TEMP", "TMP"] {
            assert_eq!(
                std::env::var_os(name).map(PathBuf::from).map(dunce::canonicalize).transpose().unwrap(),
                Some(temp.clone())
            );
        }
        assert!(!home.starts_with(&temp) && !temp.starts_with(&home));
        if let Some(parent) = std::env::var_os("MARE_TMPDIR") {
            let parent = dunce::canonicalize(parent).unwrap();
            assert!(root_real.parent() == Some(parent.as_path()) || temp.parent() == Some(parent.as_path()));
        }
        #[cfg(unix)]
        {
            assert!(!home.starts_with(dunce::canonicalize("/tmp").unwrap()));
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
            assert!(!home.starts_with(dunce::canonicalize(workspace).unwrap()));
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut bytes = vec![0u8; 4096];
            // SAFETY: confstr writes at most bytes.len bytes into the valid buffer.
            let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, bytes.as_mut_ptr().cast(), bytes.len()) };
            assert!(n > 0 && n <= bytes.len(), "cannot determine Darwin user temp directory");
            let user_temp = Path::new(std::ffi::OsStr::from_bytes(&bytes[..n - 1]));
            assert!(!home.starts_with(dunce::canonicalize(user_temp).unwrap()));
        }
        for path in [
            mare_shared::paths::state_file(),
            mare_shared::paths::trust_file(),
            mare_shared::paths::auth_file(),
            mare_shared::paths::host_token_file(),
        ] {
            assert!(path.starts_with(&mare_home), "{} escaped the test home", path.display());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "private test state").unwrap();
        }
    }

    #[tokio::test]
    async fn testenv_restores_existing_values_after_repeated_sets_and_cleanup_failure() {
        let original_home = std::env::var_os("MARE_HOME").expect("test runner supplies MARE_HOME");
        // SAFETY: nextest gives this test its own process; TestEnv holds the env lock below.
        unsafe {
            std::env::set_var("MARE_TEST_RESTORE_PROBE", "outside");
            std::env::set_var("TERM", "outside-term");
            std::env::remove_var("NO_COLOR");
        }
        for fail_cleanup in [false, true] {
            let mut env = TestEnv::new().await;
            env.set_var("MARE_TEST_RESTORE_PROBE", "first");
            env.set_var("MARE_TEST_RESTORE_PROBE", "second");
            env.color_terminal();
            env.color_terminal();
            if fail_cleanup {
                std::fs::remove_dir_all(env.home.path()).unwrap();
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(env)));
            assert_eq!(result.is_err(), fail_cleanup);
            assert_eq!(std::env::var_os("MARE_HOME"), Some(original_home.clone()));
            assert_eq!(std::env::var("MARE_TEST_RESTORE_PROBE").as_deref(), Ok("outside"));
            assert_eq!(std::env::var("TERM").as_deref(), Ok("outside-term"));
            assert_eq!(std::env::var_os("NO_COLOR"), None);
        }
    }

    #[tokio::test]
    async fn testenv_restores_previous_home_after_task_cancellation() {
        let original = std::env::var_os("MARE_HOME").expect("test runner supplies MARE_HOME");
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _env = TestEnv::new().await;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        assert_ne!(std::env::var_os("MARE_HOME"), Some(original.clone()));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(std::env::var_os("MARE_HOME"), Some(original));
    }

    #[tokio::test]
    async fn home_preservation_closes_index_and_only_keeps_requested_directory() {
        for preserve in [false, true] {
            let mut env = TestEnv::new().await;
            let home = env.home.path().to_path_buf();
            let work = env.work.path().to_path_buf();
            let index = mare_store::index::shared(&home.join("state/sessions")).unwrap();
            let owner = Arc::downgrade(&index);
            drop(index);
            if preserve {
                env.preserve_home();
            }
            drop(env);
            assert!(owner.upgrade().is_none(), "preservation must still close the index");
            assert!(!work.exists(), "workspace cleanup remains unconditional");
            assert_eq!(home.exists(), preserve);
            if preserve {
                std::fs::remove_dir_all(home).unwrap();
            }
        }
    }
}
