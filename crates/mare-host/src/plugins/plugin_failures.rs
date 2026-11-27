//! Startup failures of plugins and config files: which ones block
//! requests (essential services declared in contracts or `required = true`), the
//! one short line each is shown as, and what the frontends and Lua (`mare.plugins.failures()`)
//! get of them.

use std::path::PathBuf;

use mlua::Value;

use mare_kernel::Kernel;

/// What fixes a startup failure: the escape hatch (`nvim --clean`, `wezterm --skip-config`).
pub(crate) const STARTUP_FIX_HINT: &str = "run mare doctor, fix it and /reload (or start clean: mare --clean)";

/// A plugin whose definition or start failed at startup, or a config file (init.lua) that
/// failed: each is isolated (lazy.nvim `Util.try`), shown once as one short line naming the
/// plugin, the phase and the place ([`Self::summary`]), and kept with its full error and
/// traceback (`/plugins`, `mare doctor`, the log) until a full /reload completes.
#[derive(Clone, Debug)]
pub(crate) struct PluginFailure {
    /// the plugin's name, or the config file's name (`init.lua`, `.mare/init.lua`)
    pub(crate) plugin: String,
    /// "definition", "setup", "start" (a dependency is missing) or "config" (a config file)
    pub(crate) phase: &'static str,
    /// the whole error, traceback included
    pub(crate) error: String,
    /// the plugin's source (file or directory) or the config file
    pub(crate) path: Option<PathBuf>,
    /// blocks every request (see [`mare_kernel::Kernel::essential_services`]); otherwise a warning
    pub(crate) blocks: bool,
    /// the essential services it was responsible for, named in its summary
    pub(crate) essential: Vec<String>,
    /// Package operation, when this is a package rather than a plugin or config file.
    package_phase: Option<&'static str>,
}

impl PluginFailure {
    pub(crate) fn plugin(
        plugin: &str,
        phase: &'static str,
        error: String,
        src: Option<&mare_kernel::PluginSource>,
        services: &[String],
        essential_services: &[String],
        required: bool,
    ) -> Self {
        let essential: Vec<String> = services.iter().filter(|s| essential_services.contains(*s)).cloned().collect();
        let blocks = !essential.is_empty() || required;
        let path = src.and_then(|s| s.path.clone());
        Self { plugin: plugin.to_string(), phase, error, path, blocks, essential, package_phase: None }
    }

    /// A definition refused for colliding with an earlier one without `override = true`: the
    /// earlier definition stays active, so this only warns, whatever the plugin's name or origin.
    pub(crate) fn override_refused(r: &mare_kernel::OverrideRefusal) -> Self {
        Self {
            plugin: r.name.clone(),
            phase: "override",
            error: r.message(),
            path: r.source.path.clone(),
            blocks: false,
            essential: vec![],
            package_phase: None,
        }
    }

    /// A config file that failed at startup: mare still starts (WezTerm, Neovim) so slash
    /// commands and /reload can repair it, but every model request is refused until a reload
    /// succeeds.
    pub(crate) fn config_file(path: PathBuf, error: String) -> Self {
        let user = mare_config::user_init();
        let plugin = if path == user { "init.lua".to_string() } else { ".mare/init.lua".to_string() };
        Self { plugin, phase: "config", error, path: Some(path), blocks: true, essential: vec![], package_phase: None }
    }

    fn package(
        kernel: &Kernel,
        config: &serde_json::Value,
        spec: &mare_plugin_manager::PackageSpec,
        package_phase: &'static str,
        error: String,
    ) -> Self {
        let required = package_required(config, &spec.name);
        let specs = mare_plugin_manager::specs(config);
        let mut provided = vec![];
        let mut required_plugin = false;
        for def in kernel.def_names().into_iter().filter_map(|name| kernel.def(&name)) {
            if source_package(&specs, &def.source).is_some_and(|owner| owner.name == spec.name) {
                required_plugin |= matches!(def.table.get::<Value>("required"), Ok(Value::Boolean(true)));
                provided.extend(services(kernel, &def));
            }
        }
        let essential_services = kernel.essential_services();
        let mut essential: Vec<_> = provided.into_iter().filter(|s| essential_services.contains(s)).collect();
        essential.sort();
        essential.dedup();
        let blocks = required || required_plugin || !essential.is_empty();
        let phase = if package_phase == "load" { "start" } else { "setup" };
        Self {
            plugin: spec.name.clone(),
            phase,
            error,
            path: None,
            blocks,
            essential,
            package_phase: Some(package_phase),
        }
    }

    /// The full problem shown by `/plugins`, replaced when the next full reload completes.
    pub(crate) fn package_problem(&self) -> Option<String> {
        self.package_phase.map(|phase| format!("package {}: {phase}: {}", self.plugin, self.error))
    }

    pub(crate) fn owns_package_problem(&self, problem: &str) -> bool {
        self.package_phase.is_some()
            && (self.package_problem().as_deref() == Some(problem)
                || problem == self.error
                || ((problem.starts_with(&format!("package {} ", self.plugin))
                    || problem.starts_with(&format!("package {}:", self.plugin)))
                    && problem.ends_with(&self.error)))
    }

    /// One line: what failed, in which phase and where, and the error without its traceback —
    /// "plugin `ap` setup failed (~/.config/mare/plugins/ap.lua:1): apply exploded".
    pub(crate) fn summary(&self) -> String {
        let (src, line, msg) = error_location(&self.error);
        // the place: the plugin's own file when the error is in it (Lua shortens chunk names
        // to "...tail"), else whatever source the error names
        let own = self
            .path
            .as_ref()
            .filter(|p| src.is_none_or(|s| p.to_string_lossy().ends_with(s.trim_start_matches("..."))));
        let place = match (own, src, line) {
            (Some(p), _, Some(l)) => Some(format!("{}:{l}", short_path(&file_of(p)))),
            (Some(p), _, None) => Some(short_path(&file_of(p))),
            (None, Some(s), Some(l)) => Some(format!("{s}:{l}")),
            _ => None,
        };
        let place = place.map(|p| format!(" ({p})")).unwrap_or_default();
        // what it leaves unprovided (its name is the slot it holds): "… — essential service
        // `permissions` has no provider"
        let essential = match self.essential.as_slice() {
            [] => String::new(),
            e => {
                let names: Vec<String> = e.iter().map(|s| format!("`{s}`")).collect();
                let (noun, verb) = if e.len() == 1 { ("service", "has") } else { ("services", "have") };
                if self.package_phase.is_some() {
                    format!(" — package is responsible for essential {noun} {}", names.join(", "))
                } else {
                    format!(" — essential {noun} {} {verb} no provider", names.join(", "))
                }
            }
        };
        if let Some(phase) = self.package_phase {
            return if phase == "load" {
                format!("package `{}` unavailable: {msg}{essential}", self.plugin)
            } else {
                format!("package `{}` {phase} failed{place}: {msg}{essential}", self.plugin)
            };
        }
        match self.phase {
            "config" => format!("{} has an error{place}: {msg}", self.plugin),
            "override" => self.error.clone(),
            "start" => format!("plugin `{}` not started: {msg}{essential}", self.plugin),
            phase => format!("plugin `{}` {phase} failed{place}: {msg}{essential}", self.plugin),
        }
    }

    /// The notice shown once (TUI, plain mode, headless stderr): the summary, what it means for
    /// requests, and how to fix it.
    pub(crate) fn notice(&self) -> String {
        let consequence = if self.blocks { "requests are blocked" } else { "requests continue without it" };
        format!("{} — {consequence}; {STARTUP_FIX_HINT}", self.summary())
    }
}

fn package_required(config: &serde_json::Value, name: &str) -> bool {
    config.get("packages").and_then(|p| p.as_array()).is_some_and(|packages| {
        packages.iter().any(|p| {
            p.get("name").and_then(|n| n.as_str()) == Some(name)
                && p.get("required").and_then(|v| v.as_bool()) == Some(true)
        })
    })
}

fn source_package<'a>(
    specs: &'a [mare_plugin_manager::PackageSpec],
    source: &mare_kernel::PluginSource,
) -> Option<&'a mare_plugin_manager::PackageSpec> {
    if source.origin != mare_kernel::Origin::Package {
        return None;
    }
    let path = source.path.as_ref()?;
    specs.iter().filter(|spec| path.starts_with(spec.root())).max_by_key(|spec| spec.root().components().count())
}

pub(crate) fn source_package_required(config: &serde_json::Value, source: &mare_kernel::PluginSource) -> bool {
    source_package(&mare_plugin_manager::specs(config), source).is_some_and(|spec| package_required(config, &spec.name))
}

/// Whether an unattributed package operation can affect a required plugin or service.
pub(crate) fn package_operation_blocks(kernel: &Kernel, config: &serde_json::Value) -> bool {
    mare_plugin_manager::specs(config)
        .iter()
        .any(|spec| PluginFailure::package(kernel, config, spec, "load", String::new()).blocks)
}

/// Each unavailable package has its own diagnostic, retaining the install or manifest error.
pub(crate) fn unavailable_packages(
    kernel: &Kernel,
    config: &serde_json::Value,
    notices: &[String],
) -> Vec<PluginFailure> {
    let unavailable = mare_plugin_manager::unavailable(config);
    let specs = mare_plugin_manager::specs(config);
    specs
        .iter()
        .filter(|spec| unavailable.contains(&spec.name))
        .map(|spec| {
            let space = format!("package {} ", spec.name);
            let colon = format!("package {}: ", spec.name);
            let error = notices
                .iter()
                .find_map(|n| n.strip_prefix(&space).or_else(|| n.strip_prefix(&colon)))
                .unwrap_or("not installed or incompatible")
                .to_string();
            PluginFailure::package(kernel, config, spec, "load", error)
        })
        .collect()
}

/// Hook errors carry the package name in their source message; keep each failure separate.
pub(crate) fn package_hook_failures(
    kernel: &Kernel,
    config: &serde_json::Value,
    which: &str,
    problems: &[String],
) -> Vec<PluginFailure> {
    let specs = mare_plugin_manager::specs(config);
    let package_phase = if which == "init" { "init" } else { "config" };
    problems
        .iter()
        .map(|problem| {
            specs
                .iter()
                .find_map(|spec| {
                    let prefix = format!("package {}: {which} failed: ", spec.name);
                    problem
                        .strip_prefix(&prefix)
                        .map(|error| PluginFailure::package(kernel, config, spec, package_phase, error.to_string()))
                })
                .unwrap_or_else(|| PluginFailure {
                    plugin: "packages".into(),
                    phase: "setup",
                    error: problem.clone(),
                    path: None,
                    blocks: package_operation_blocks(kernel, config),
                    essential: vec![],
                    package_phase: Some(package_phase),
                })
        })
        .collect()
}

/// A plugin source's file (a directory plugin's `init.lua`).
fn file_of(p: &std::path::Path) -> PathBuf {
    if p.is_dir() { p.join("init.lua") } else { p.to_path_buf() }
}

/// A path for messages: under the home directory as `~/…`, else relative to the config
/// directory, else as it is.
fn short_path(p: &std::path::Path) -> String {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    let shown = mare_shared::style::display_path(&p.to_string_lossy(), &home, 0);
    if shown.starts_with('~') {
        return shown;
    }
    let user = mare_config::user_init();
    match user.parent().and_then(|dir| p.strip_prefix(dir).ok()) {
        Some(rel) => rel.to_string_lossy().into_owned(),
        None => shown,
    }
}

/// The first line of an error, taken apart: the source and line it names (`…/ap.lua:1: …`, if
/// any) and the message without mlua's and the loader's prefixes.
pub(crate) fn error_location(error: &str) -> (Option<&str>, Option<u32>, &str) {
    let mut msg = error.lines().next().unwrap_or("").trim();
    // the loader's "loading plugin <name> (<file>): …"
    if let Some(rest) = msg.strip_prefix("loading plugin ")
        && let Some(at) = rest.find("): ")
    {
        msg = &rest[at + 3..];
    }
    loop {
        let before = msg;
        for prefix in ["failed: ", "could not be re-enabled: ", "runtime error: ", "syntax error: "] {
            msg = msg.strip_prefix(prefix).unwrap_or(msg);
        }
        if before == msg {
            break;
        }
    }
    // "<source>:<line>: <message>"
    let mut from = 0;
    while let Some(i) = msg[from..].find(':').map(|i| i + from) {
        let digits: String = msg[i + 1..].chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && msg[i + 1 + digits.len()..].starts_with(": ") {
            let line = digits.parse().ok();
            return (Some(&msg[..i]), line, &msg[i + 1 + digits.len() + 2..]);
        }
        from = i + 1;
    }
    (None, None, msg)
}

fn start_failure_entry(
    kernel: &Kernel,
    name: &str,
    tree: &[mare_kernel::PluginTreeRow],
    problems: &[String],
    essential_services: &[String],
) -> PluginFailure {
    let prefix = format!("plugin {name} ");
    let error = match problems.iter().rev().find_map(|p| p.strip_prefix(&prefix)) {
        Some(recorded) => recorded.to_string(),
        // a PENDING plugin waits for a service it injects (#8: not started, not failed) — also
        // after a reload, which records no problem for it: name what it waits for, as startup does
        None => match Kernel::missing_in(tree, name).as_slice() {
            [] => "failed: unknown error".to_string(),
            missing => format!(
                "not started: required service {} is not available",
                missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
            ),
        },
    };
    // "plugin <name> not started: …" (a missing dependency) or "… failed: …" (`apply`)
    let (phase, error) = match error.strip_prefix("not started: ") {
        Some(rest) => ("start", rest.to_string()),
        None => ("setup", error),
    };
    match kernel.def(name) {
        Some(d) => {
            let required = matches!(d.table.get::<Value>("required"), Ok(Value::Boolean(true)))
                || source_package_required(&kernel.config(), &d.source);
            PluginFailure::plugin(
                name,
                phase,
                error,
                Some(&d.source),
                &services(kernel, &d),
                essential_services,
                required,
            )
        }
        None => PluginFailure::plugin(name, phase, error, None, &kernel.provided(name), essential_services, false),
    }
}

/// Start failure for a single defined plugin that is not active in `tree`, if any.
pub(crate) fn start_failure_for(
    kernel: &Kernel,
    name: &str,
    tree: &[mare_kernel::PluginTreeRow],
) -> Option<PluginFailure> {
    if kernel.def(name).is_none() || Kernel::active_in(tree, name) {
        return None;
    }
    let problems = kernel.problems();
    let essential_services = kernel.essential_services();
    Some(start_failure_entry(kernel, name, tree, &problems, &essential_services))
}

/// The plugins that are defined but not active — after startup or after any reload — with the
/// phase that failed ("start": a missing dependency, "setup": `apply`) and their latest error
/// from the kernel's problems. Computed when asked, so it always matches the running plugins.
pub(crate) fn start_failures(kernel: &Kernel, tree: &[mare_kernel::PluginTreeRow]) -> Vec<PluginFailure> {
    let problems = kernel.problems();
    let essential_services = kernel.essential_services();
    kernel
        .plugin_status_in(tree)
        .into_iter()
        .filter(|(_, _, active)| !active)
        .map(|(name, _, _)| start_failure_entry(kernel, &name, tree, &problems, &essential_services))
        .collect()
}

/// The services a plugin is responsible for: the ones its definition declares or inherited from
/// the definition it replaced ([`mare_kernel::PluginDef::services`]) and the ones it called
/// `ctx:provide` for at runtime, refused or not — so a plugin replacing a core service
/// without declaring it blocks requests when it fails, like one that declares it.
fn services(kernel: &Kernel, def: &mare_kernel::PluginDef) -> Vec<String> {
    let mut out = def.services();
    let runtime: Vec<String> = kernel.provided(&def.name).into_iter().filter(|s| !out.contains(s)).collect();
    out.extend(runtime);
    out
}

/// Why requests are refused because of failures, if they are: a config file's
/// error alone reads "init.lua has an error (…) — fix it and /reload …"; otherwise the parts —
/// config errors, `other` (unavailable packages, package hooks), blocking definition and start
/// failures — follow "startup is incomplete: " (or "plugins are not ready: " after a full
/// /reload).
pub(crate) fn blocked_reason(other: &[String], failures: &[PluginFailure], after_reload: bool) -> Option<String> {
    let blocking = |phases: &[&str]| -> Vec<String> {
        failures.iter().filter(|f| f.blocks && phases.contains(&f.phase)).map(PluginFailure::summary).collect()
    };
    let config = blocking(&["config"]);
    let definitions = blocking(&["definition"]);
    let started: Vec<_> = failures
        .iter()
        .filter(|f| f.blocks && f.package_phase.is_none() && ["setup", "start"].contains(&f.phase))
        .map(PluginFailure::summary)
        .collect();
    let packages: Vec<_> =
        failures.iter().filter(|f| f.blocks && f.package_phase.is_some()).map(PluginFailure::summary).collect();
    if !config.is_empty() && other.is_empty() && definitions.is_empty() && started.is_empty() && packages.is_empty() {
        return Some(format!("{} — {STARTUP_FIX_HINT}", config.join("; ")));
    }
    let mut parts = config;
    parts.extend(other.iter().cloned());
    parts.extend(packages);
    if !definitions.is_empty() {
        parts.push(format!("plugin definitions failed: {}", definitions.join("; ")));
    }
    if !started.is_empty() {
        parts.push(format!("plugins failed to start: {}", started.join("; ")));
    }
    if parts.is_empty() {
        return None;
    }
    let what = if after_reload { "plugins are not ready" } else { "startup is incomplete" };
    Some(format!("{what}: {}; {STARTUP_FIX_HINT}", parts.join("; ")))
}

/// What the frontend shows once at startup, each with its level: one short line per failure that
/// doesn't block, the reason requests are blocked (if they are), then the other problems.
/// Tracebacks stay in `/plugins`, `mare doctor` and the log.
pub(crate) fn startup_notices(
    failures: &[PluginFailure],
    blocked: Option<&str>,
    problems: Vec<String>,
) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> =
        failures.iter().filter(|f| !f.blocks).map(|f| (f.notice(), "warning")).collect();
    if let Some(b) = blocked {
        out.push((b.to_string(), "error"));
    }
    for p in problems {
        let covered = blocked.is_some_and(|b| b == p || b.contains(p.as_str()))
            || failures.iter().any(|f| p.starts_with(&format!("plugin {} ", f.plugin)) || p.ends_with(&f.error));
        if !covered {
            out.push((format!("{p} — run mare doctor"), "warning"));
        }
    }
    out
}

/// `mare.plugins.failures()`: {{plugin, error, summary, phase, blocks, kind}}.
pub(crate) fn to_lua(lua: &mlua::Lua, failures: &[PluginFailure]) -> mlua::Result<mlua::Table> {
    let out = lua.create_table()?;
    for f in failures {
        let t = lua.create_table()?;
        t.set("plugin", f.plugin.as_str())?;
        t.set("error", f.error.as_str())?;
        t.set("summary", f.summary())?;
        t.set("phase", f.phase)?;
        t.set("blocks", f.blocks)?;
        t.set("kind", if f.phase == "config" { "config" } else { "plugin" })?;
        out.push(t)?;
    }
    Ok(out)
}

/// The running plugin whose source is `src` (a full /reload keeps it when the file no longer
/// loads: the last good definition stays, as WezTerm keeps the last good config).
pub(crate) fn running_def(kernel: &Kernel, src: &mare_kernel::PluginSource) -> Option<mare_kernel::PluginDef> {
    kernel
        .def_names()
        .into_iter()
        .filter_map(|n| kernel.def(&n))
        .find(|d| d.source.path == src.path && d.source.origin == src.origin && kernel.plugin_active(&d.name))
}

/// A finished reload's notices, one short line each: a plugin left inactive as its failure
/// notice (blocking or not), one that kept or got back its previous definition as the first
/// line of the kernel's report.
pub(crate) fn reload_notices(kernel: &Kernel, report_failures: &[String]) -> Vec<String> {
    let inactive = start_failures(kernel, &kernel.plugin_tree().unwrap_or_default());
    let mut out: Vec<String> = report_failures
        .iter()
        .filter(|f| !inactive.iter().any(|i| f.starts_with(&format!("plugin {} ", i.plugin))))
        .map(|f| format!("{} — run mare doctor", f.lines().next().unwrap_or_default()))
        .collect();
    out.extend(inactive.iter().map(PluginFailure::notice));
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mlua::{Lua, ObjectLike, Table};

    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[tokio::test]
    async fn essential_and_poisoned_allowlists_come_from_contracts() {
        let kernel = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let contracts: Table = kernel.lua().load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        kernel.install_service_contracts(contracts.clone()).unwrap();

        let essential: BTreeSet<String> = kernel.essential_services().into_iter().collect();
        assert_eq!(essential, set(&["agent", "approvals", "llm", "permissions", "session", "tools"]));

        assert_eq!(
            kernel.poisoned_allowed_methods("agent"),
            Some(set(&[
                "activity",
                "baseline_cut",
                "cancel",
                "capabilities",
                "get",
                "interrupted",
                "list_messages",
                "list_queue",
                "messages",
                "pending_request",
                "persist_error",
                "wait",
            ]))
        );
        assert_eq!(
            kernel.poisoned_allowed_methods("session"),
            Some(set(&[
                "switching",
                "info",
                "available",
                "entries",
                "list_items",
                "path_to",
                "leaf",
                "entry_id",
                "list_records",
                "list",
                "get",
                "children",
                "file_changes",
                "short_id",
                "live_snapshot",
                "operation_receipt",
                "release_operation",
                "baseline_cut",
            ]))
        );
        assert_eq!(
            kernel.poisoned_allowed_methods("tools"),
            Some(set(&[
                "snippets",
                "capabilities",
                "register",
                "shadow",
                "get",
                "list",
                "definitions",
                "bind_declarations",
                "edit_mode",
                "observe",
                "observation",
                "fs_guard",
                "with_file_lock",
                "readonly",
                "side_effect",
                "disable",
                "enable",
                "shell",
                "process_env",
                "register_executor",
                "touch",
                "env_policy",
                "executor",
                "register_search_source",
                "concurrency_safe",
                "cwd",
                "revision",
                "has_dynamic_admission",
                "invalidate",
                "is_disabled",
                "output_channel",
                "send_output",
            ]))
        );
        for blocked in ["execute", "command", "shell_command", "fs_commit"] {
            assert!(!kernel.allows_when_poisoned("tools", blocked), "tools.{blocked} must be blocked");
        }
        // Guard against silently default-denying new `tools` methods: every declared method
        // on `tools` must carry an explicit `"allow"` or `"deny"` policy.
        let tools_contract = contracts.get::<Table>("tools").unwrap();
        let tools_methods = tools_contract.get::<Table>("methods").unwrap();
        for pair in tools_methods.pairs::<String, Table>() {
            let (method, spec) = pair.unwrap();
            let policy: Option<String> = spec.get("poisoned").unwrap();
            assert!(
                matches!(policy.as_deref(), Some("allow" | "deny")),
                "tools.{method} must explicitly set poisoned = \"allow\" or \"deny\""
            );
        }
        assert!(kernel.allows_when_poisoned("llm", "get_model"));
        assert_eq!(kernel.poisoned_allowed_methods("llm"), None);

        // Third-party plugins cannot declare an essential service.
        let third_party = mare_kernel::PluginSource {
            name_hint: "third".into(),
            origin: mare_kernel::Origin::User,
            path: None,
            code: Some(br#"return { name = "third", contracts = { extra = { essential = true, methods = {} } } }"#),
        };
        let err = kernel.load_def(&third_party).err().expect("non-builtin essential contract must be refused");
        assert!(err.to_string().contains("essential"), "{err}");

        // A built-in contract marking an extra service essential makes a failing plugin that
        // provides it block requests.
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = mare_plugin::new_lua().unwrap();
                let env = mare_plugin::Env {
                    cwd: std::env::temp_dir(),
                    mode: mare_plugin::Mode::Print,
                    ui: mare_shared::ui::UiShared::new(),
                    config: serde_json::json!({}),
                    args: serde_json::json!({}),
                };
                mare_plugin::install(&lua, &env).unwrap();
                let k2 = Kernel::new(lua, serde_json::json!({})).unwrap();
                let extra_contracts: Table = k2
                    .lua()
                    .load(
                        r#"return {
                            extra_svc = { essential = true, methods = { ping = { doc = "ping", result = { type = "string" } } } },
                            full_guarded = {
                                doc = "full guarded test service",
                                methods = {
                                    diag = { doc = "read", poisoned = "allow", result = { type = "string" } },
                                    mutate = { doc = "write", poisoned = "deny", result = { type = "string" } },
                                },
                            },
                        }"#,
                    )
                    .eval()
                    .unwrap();
                k2.install_service_contracts(extra_contracts).unwrap();
                assert_eq!(k2.essential_services(), vec!["extra_svc"]);
                assert!(k2.allows_when_poisoned("full_guarded", "diag"));
                assert!(!k2.allows_when_poisoned("full_guarded", "mutate"));

                let def = k2
                    .load_def(&mare_kernel::PluginSource {
                        name_hint: "extra-provider".into(),
                        origin: mare_kernel::Origin::User,
                        path: None,
                        code: Some(
                            br#"return {
                                name = "extra-provider",
                                provide = { "extra_svc" },
                                apply = function() error("extra boom") end,
                            }"#,
                        ),
                    })
                    .unwrap();
                let working = k2
                    .load_def(&mare_kernel::PluginSource {
                        name_hint: "guarded-provider".into(),
                        origin: mare_kernel::Origin::Builtin,
                        path: None,
                        code: Some(
                            br#"return {
                                name = "guarded-provider",
                                provide = { "full_guarded" },
                                apply = function(ctx)
                                    local S = require("cordis").class("GuardedSvc", mare.Service)
                                    function S:constructor(c) S.super.constructor(self, c, "full_guarded") end
                                    function S:diag() return "ok" end
                                    function S:mutate() return "changed" end
                                    S.new(ctx)
                                end,
                            }"#,
                        ),
                    })
                    .unwrap();
                k2.set_defs(vec![def, working]);
                k2.start().await.unwrap();

                let failures = start_failures(&k2, &k2.plugin_tree().unwrap_or_default());
                assert_eq!(failures.len(), 1);
                assert!(failures[0].blocks, "failing provider of an essential contract must block");
                assert_eq!(failures[0].essential, vec!["extra_svc"]);
                let reason = blocked_reason(&[], &failures, false).expect("requests must be blocked");
                assert!(reason.contains("extra_svc") && reason.contains("extra-provider"), "{reason}");

                let svc: Table = match k2.service("full_guarded") {
                    Some(Value::Table(t)) => t,
                    other => panic!("expected full_guarded service table, got {other:?}"),
                };
                assert_eq!(svc.call_method::<String>("diag", ()).unwrap(), "ok");
                assert_eq!(svc.call_method::<String>("mutate", ()).unwrap(), "changed");

                mare_shared::session_barrier::barrier(k2.lua()).poison("disk full");
                assert_eq!(svc.call_method::<String>("diag", ()).unwrap(), "ok");
                let blocked = svc.call_method::<String>("mutate", ()).unwrap_err().to_string();
                assert!(blocked.contains("disk full"), "{blocked}");

                k2.shutdown().await;
            })
            .await;
    }

    /// Paths inside the user's home directory are formatted with `~/…`.
    #[test]
    fn short_path_formats_home_paths_with_tilde() {
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let target = PathBuf::from(&home).join(".config").join("mare").join("plugins").join("bad.lua");
        let shown = short_path(&target);
        assert!(shown.starts_with("~/") || shown.starts_with("~\\"), "expected tilde prefix, got: {shown}");
        assert!(shown.ends_with("bad.lua"), "expected file name preserved, got: {shown}");
    }

    #[test]
    fn unattributed_package_hook_failure_uses_package_operation_blocks() {
        let kernel = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
        let config = serde_json::json!({});
        let failures = package_hook_failures(&kernel, &config, "init", &["unexpected loader error".to_string()]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].plugin, "packages");
        assert_eq!(failures[0].package_phase, Some("init"));
        assert!(!failures[0].blocks);
    }
}
