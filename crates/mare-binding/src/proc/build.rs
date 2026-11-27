//! Command construction, sandbox wrapping, and non-Lua confined spawning for `mare.proc`.

use std::process::Stdio;

use mlua::{Lua, Table, Value};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use super::super::ossandbox::Prepared;
use super::super::platform::{self, ProcessBackend};
use super::attach_monitor;
use mare_shared::lua::rt_err;

/// Give normal protection cleanup a brief chance to finish without making a finished command
/// wait indefinitely for procfs visibility. The independent monitor retains the lease afterwards.
pub(super) const PROTECTION_CLEANUP_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

pub(crate) struct SpawnOpts {
    pub(crate) cmd: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) clear_env: bool,
    /// scrub adjustments: names kept / dropped beyond the default rule
    pub(crate) env_policy: super::super::subenv::EnvPolicy,
    pub(crate) stdin: bool,
    pub(crate) sandbox: Value,
    /// set by [`build`]: the sandbox policy + network proxy; the caller keeps it alive for as
    /// long as the process runs (`o.prepared.take()`)
    pub(crate) prepared: std::cell::RefCell<Option<Prepared>>,
}

pub(crate) fn parse_opts(t: &Table) -> mlua::Result<SpawnOpts> {
    let cmd: String = t.get("cmd")?;
    let args: Vec<String> = t.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
    let cwd: Option<String> = t.get("cwd")?;
    let mut env = vec![];
    if let Some(e) = t.get::<Option<Table>>("env")? {
        for p in e.pairs::<String, String>() {
            env.push(p?);
        }
    }
    Ok(SpawnOpts {
        cmd,
        args,
        cwd,
        env,
        clear_env: t.get::<Option<bool>>("clear_env")?.unwrap_or(false),
        env_policy: super::super::subenv::policy_from(t)?,
        stdin: t.get::<Option<bool>>("stdin")?.unwrap_or(false),
        // inside a tool call the core forces the context policy
        sandbox: crate::exec_ctx::spawn_sandbox(t, t.get("sandbox")?)?,
        prepared: Default::default(),
    })
}

pub(crate) fn build(lua: &Lua, o: &SpawnOpts, default_cwd: &str) -> mlua::Result<Command> {
    let cwd = o.cwd.clone().unwrap_or_else(|| default_cwd.to_string());
    let prepared = Prepared::new(lua, &o.sandbox, std::path::Path::new(&cwd))?;
    let c = build_with_prepared(&o.cmd, &o.args, &cwd, &o.env, o.clear_env, &o.env_policy, o.stdin, prepared.as_ref())?;
    *o.prepared.borrow_mut() = prepared;
    Ok(c)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_with_prepared(
    prog: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
    clear_env: bool,
    env_policy: &super::super::subenv::EnvPolicy,
    stdin: bool,
    prepared: Option<&Prepared>,
) -> mlua::Result<Command> {
    let (mut c, extra_env) = match prepared {
        Some(sb) => build_sandboxed_command(prog, args, sb)?,
        None => {
            let mut c = Command::new(prog);
            c.args(args);
            (c, vec![])
        }
    };
    c.current_dir(cwd);
    // every child starts from the scrubbed parent environment; explicit
    // variables merge after it, so deliberately forwarded credentials still work
    c.env_clear();
    if !clear_env {
        let mut base = super::super::subenv::scrubbed_parent_env(env_policy);
        if prepared.is_some() {
            super::super::subenv::strip_askpass_env(&mut base);
        }
        c.envs(base);
    }
    for (k, v) in env.iter().chain(extra_env.iter()) {
        c.env(k, v);
    }
    c.stdin(if stdin { Stdio::piped() } else { Stdio::null() });
    c.stdout(Stdio::piped());
    c.stderr(Stdio::piped());
    c.kill_on_drop(true);
    <platform::Process as ProcessBackend>::isolate(&mut c);
    Ok(c)
}

fn build_sandboxed_command(
    prog: &str,
    args: &[String],
    sb: &Prepared,
) -> mlua::Result<(Command, Vec<(String, String)>)> {
    let w = sb.wrap_monitored(prog, args);
    let mut c = Command::new(w.prog);
    c.args(w.args);
    #[cfg(target_os = "linux")]
    {
        // Install Landlock (when applicable) and baseline seccomp unconditionally on Linux
        // right before exec.
        let rw = w.landlock;
        let lifecycle_fds = w.lifecycle_fds;
        let filter = Some(super::super::ossandbox::seccomp_filter(w.socket_block).map_err(rt_err)?);
        unsafe {
            c.pre_exec(move || {
                if let Some(fds) = lifecycle_fds {
                    super::super::ossandbox::inherit_lifecycle(fds)?;
                }
                super::super::ossandbox::child_restrict(rw.as_ref(), filter.as_deref())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (w.landlock, w.socket_block);
    Ok((c, w.env))
}

/// Start any protection monitor, spawn `cmd`, and clean up the monitor if spawning fails.
pub(super) fn spawn_monitored(
    cmd: &mut Command,
    sandbox: Option<&Prepared>,
    err_prefix: &str,
) -> mlua::Result<(Child, Option<super::super::ossandbox::ProtectionMonitor>)> {
    let monitor = sandbox.map(|sb| sb.start_monitor()).transpose()?.flatten();
    match platform::spawn(cmd) {
        Ok(child) => Ok((child, monitor)),
        Err(e) => {
            if let Some(monitor) = &monitor {
                monitor.after_kill();
            }
            Err(rt_err(format!("{err_prefix}: {e}")))
        }
    }
}

/// Spawned sandbox-aware process components for non-Lua callers (e.g. `proc.spawn`).
pub struct RawProcess {
    pub pid: Option<u32>,
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub sandbox: Option<Prepared>,
    monitor: Option<super::super::ossandbox::ProtectionMonitor>,
    pub tree: super::super::trees::Tree,
}

/// Surviving process tree and its associated sandbox/network-proxy guard for a process
/// whose root exited while background descendants continue running.
pub struct LingeringProcess {
    tree: super::super::trees::Tree,
    _sandbox: Option<Prepared>,
}

impl LingeringProcess {
    /// Returns whether any process in this lingering tree is still alive.
    pub fn is_alive(&self) -> bool {
        self.tree.is_alive()
    }
}

impl RawProcess {
    /// Wait briefly for protection-monitor cleanup after the child exits, returning
    /// the process-tree handle and sandbox/network-proxy guard so the caller can keep
    /// lingering descendants confined and networked until they exit or disconnect.
    pub async fn finish_monitor_into_lingering(self) -> LingeringProcess {
        let Self { monitor, tree, sandbox, .. } = self;
        if let Some(monitor) = monitor {
            let _ = tokio::time::timeout(PROTECTION_CLEANUP_GRACE, monitor.finished()).await;
        }
        LingeringProcess { tree, _sandbox: sandbox }
    }
}

/// Spawn a piped process confined by `policy` (when `Some`), inheriting process-tree ownership.
#[allow(clippy::too_many_arguments)]
pub fn spawn_confined(
    prog: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
    stdin: bool,
    policy: Option<super::super::ossandbox::Policy>,
    owner: Option<super::super::trees::Owner>,
    approver: Option<super::super::netauth::NetApprover>,
) -> mlua::Result<RawProcess> {
    let sandbox =
        policy.map(|p| Prepared::from_policy_with_approver(p, std::path::Path::new(cwd), approver)).transpose()?;
    let mut cmd = build_with_prepared(
        prog,
        args,
        cwd,
        env,
        false,
        &super::super::subenv::EnvPolicy::default(),
        stdin,
        sandbox.as_ref(),
    )?;
    let (mut child, monitor) = spawn_monitored(&mut cmd, sandbox.as_ref(), &format!("spawn {prog}"))?;
    let monitor = attach_monitor(monitor, &mut child)?;
    let pid = child.id();
    let tree = super::super::trees::Tree::new(pid, owner);
    let stdin_h = child.stdin.take();
    let stdout_h = child.stdout.take();
    let stderr_h = child.stderr.take();
    Ok(RawProcess { pid, child, stdin: stdin_h, stdout: stdout_h, stderr: stderr_h, sandbox, monitor, tree })
}
