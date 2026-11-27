//! Startup budget guard for all built-in plugins: verifies that each built-in's `apply` runs
//! synchronously and stays within its registered `require` module cap.

use std::collections::{BTreeMap, BTreeSet};

use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;
use serde_json::json;

use crate::common::*;

/// Maximum number of Lua modules each built-in plugin may `require` during `apply`.
///
/// Entries reflect current behavior and may only tighten over time; every built-in plugin must be
/// listed here so new eager imports cannot slip in unnoticed.
const BUILTIN_APPLY_REQUIRE_CAPS: &[(&str, u32)] = &[
    ("agent", 8),
    ("agent-approvals", 2),
    ("agents", 3),
    ("approval-review-model", 0),
    ("approvals", 0),
    ("cells", 2),
    ("changes", 0),
    ("commands", 4),
    ("compact", 1),
    ("context", 6),
    ("conversations", 2),
    ("copy", 0),
    ("file-ref", 0),
    ("fs", 0),
    ("images", 0),
    ("jobs", 5),
    ("keymap-default", 0),
    ("llm", 8),
    ("lua-console", 0),
    ("mcp", 7),
    ("message", 0),
    ("notify", 0),
    ("onboarding", 0),
    ("permissions", 9),
    ("plugins", 1),
    ("proc", 0),
    ("profile", 0),
    ("provider-anthropic", 0),
    ("provider-google", 0),
    ("provider-openai", 2),
    ("provider-openai-compat", 1),
    ("provider-opencode-go", 1),
    ("query", 0),
    ("query-disk", 0),
    ("repeat-guard", 0),
    ("review", 0),
    ("session", 6),
    ("shell-mode", 0),
    ("spill", 0),
    ("status", 0),
    ("statusline", 0),
    ("subagent", 2),
    ("terminal", 0),
    ("theme-default", 4),
    ("tools", 7),
    ("tools-apply-patch", 0),
    ("ui", 2),
    ("update-check", 0),
    ("usage", 0),
    ("user-config", 0),
    ("web", 2),
    ("web-fetch-http", 0),
    ("webserver", 0),
];

#[cfg(any(debug_assertions, feature = "faux"))]
const OPTIONAL_FAUX_CAP: Option<(&str, u32)> = Some(("provider-faux", 0));
#[cfg(not(any(debug_assertions, feature = "faux")))]
const OPTIONAL_FAUX_CAP: Option<(&str, u32)> = None;

/// Built-in plugins whose `apply` still yields at runtime through indirect disk I/O in submodules.
///
/// Relationship with the static lint (`scripts/lua_api_apply_sync.py`):
/// - `_BUILTIN_APPLY_YIELD_EXCEPTIONS` tracks direct or intra-file helper yields in plugin entry
///   files.
/// - `DEFERRED_IO_BUILTINS` tracks cross-module runtime yields observed during actual boot.
///
/// Both lists are strictly shrinking ratchet guards and are now empty (0 static exceptions and 0
/// runtime yields across all 54 built-in plugins).
const DEFERRED_IO_BUILTINS: &[&str] = &[];

fn parse_builtin_apply_entry(label: &str) -> Option<(String, bool, u32)> {
    let rest = label.strip_prefix("plugin ")?;
    let (name, tail) = rest.split_once(" (builtin) [")?;
    let inner = tail.strip_suffix(']')?;
    let (mode, req_part) = inner.split_once(", req=")?;
    let yielded = match mode {
        "sync" => false,
        "yielded" => true,
        _ => return None,
    };
    let reqs = req_part.parse::<u32>().ok()?;
    Some((name.to_string(), yielded, reqs))
}

#[tokio::test]
async fn all_builtin_plugins_apply_synchronously_within_require_caps() {
    local(async {
        let env = TestEnv::new().await;
        mare_shared::profile::enable_startup_profile();
        let before = mare_shared::profile::entries().len();
        let h = Host::new(HostOptions {
            cwd: env.cwd.clone(),
            mode: Mode::Print,
            config: json!({}),
            load_init: false,
            project_trusted: false,
            args: json!({ "no_session": true }),
            no_user_plugins: true,
        })
        .await
        .unwrap();

        let builtins: BTreeSet<String> = h
            .kernel
            .plugin_status()
            .into_iter()
            .filter(|(_, origin, _)| origin == "builtin")
            .map(|(name, _, _)| name)
            .collect();

        let mut observed: BTreeMap<String, (bool, u32)> = BTreeMap::new();
        for entry in mare_shared::profile::entries().into_iter().skip(before) {
            if let Some((name, yielded, reqs)) = parse_builtin_apply_entry(&entry.label) {
                observed.insert(name, (yielded, reqs));
            }
        }

        h.shutdown().await;

        let mut caps: BTreeMap<&str, u32> = BUILTIN_APPLY_REQUIRE_CAPS.iter().copied().collect();
        if let Some((name, cap)) = OPTIONAL_FAUX_CAP {
            caps.insert(name, cap);
        }
        let deferred: BTreeSet<&str> = DEFERRED_IO_BUILTINS.iter().copied().collect();

        let mut violations = Vec::new();
        for name in &builtins {
            if !observed.contains_key(name) {
                violations.push(format!("{name}: missing startup apply measurement"));
            }
            if !caps.contains_key(name.as_str()) {
                violations.push(format!("{name}: missing entry in BUILTIN_APPLY_REQUIRE_CAPS"));
            }
        }
        for &name in caps.keys() {
            if !builtins.contains(name) {
                violations.push(format!("{name}: stale entry in BUILTIN_APPLY_REQUIRE_CAPS (not a built-in)"));
            }
        }
        for (name, &(yielded, reqs)) in &observed {
            if let Some(&cap) = caps.get(name.as_str())
                && reqs > cap
            {
                violations.push(format!("{name}: required {reqs} module(s) during apply, exceeding cap {cap}"));
            }
            if yielded && !deferred.contains(name.as_str()) {
                violations.push(format!("{name}: apply yielded unexpectedly (must complete synchronously)"));
            }
            if !yielded && deferred.contains(name.as_str()) {
                violations.push(format!("{name}: apply is now synchronous; remove it from DEFERRED_IO_BUILTINS"));
            }
        }

        // Guard against measuring only `package.loaded` while missing owner-partitioned plugin modules.
        let total_reqs: u32 = observed.values().map(|(_, reqs)| *reqs).sum();
        assert!(
            total_reqs > 0,
            "expected built-in apply measurements to count loaded plugin modules (got total_reqs=0)"
        );
        let observed_summary: Vec<String> = observed
            .iter()
            .map(|(n, (y, r))| format!("    ({n:?}, {r}), // {}", if *y { "yielded" } else { "sync" }))
            .collect();
        assert!(
            violations.is_empty(),
            "built-in plugin apply budget violations:\n{}\n\nobserved ({} built-ins):\n{}",
            violations.join("\n"),
            observed.len(),
            observed_summary.join("\n")
        );
    })
    .await;
}
