//! Lua primitive surface invariants: `mare.api_version`, consistent `spill_id` / `start` /
//! `delete` naming across runtime and LuaLS types, and type-definition alignment.

use std::path::PathBuf;

use crate::common::*;

fn preset_types_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/types")
}

fn read_handwritten_type_files() -> Vec<(String, String)> {
    let root = preset_types_dir();
    let mut out = Vec::new();
    let mare_lua = root.join("mare.lua");
    out.push(("mare.lua".to_string(), std::fs::read_to_string(&mare_lua).unwrap()));
    for dir_rel in ["mare", "mare/host"] {
        let dir = root.join(dir_rel);
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if dir_rel == "mare" && (name == "events.lua" || name.ends_with(".gen.lua")) {
                continue;
            }
            let rel = format!("{dir_rel}/{name}");
            out.push((rel, std::fs::read_to_string(&path).unwrap()));
        }
    }
    out
}

/// User plugins can read `mare.api_version` and it equals `mare_protocol::PROTOCOL_VERSION`
/// across Print, TUI, RPC, and `mare test` hosts.
#[tokio::test]
async fn api_version_is_readable_and_equals_protocol() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("api_ver_probe.lua"),
            r#"
            return {
                name = "api_ver_probe",
                apply = function()
                    mare._seen_api_version = mare.api_version
                end,
            }
            "#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let seen: Option<String> = eval(&h, "return mare._seen_api_version").await;
        assert_eq!(
            seen.as_deref(),
            Some(mare_protocol::PROTOCOL_VERSION),
            "user plugin must read mare.api_version == PROTOCOL_VERSION"
        );
        let direct: Option<String> = eval(&h, "return mare.api_version").await;
        assert_eq!(direct.as_deref(), Some(mare_protocol::PROTOCOL_VERSION));
        h.shutdown().await;

        // Also check TUI host mode exposes the same `mare.api_version`.
        let mut tui_builder = env.host().user_plugins().no_session();
        tui_builder.opts.mode = mare_shared::ui::Mode::Tui;
        let tui_host = tui_builder.build().await;
        let tui_ver: Option<String> = eval(&tui_host, "return mare.api_version").await;
        assert_eq!(tui_ver.as_deref(), Some(mare_protocol::PROTOCOL_VERSION));
        tui_host.shutdown().await;

        // Also check RPC mode via `mare --rpc` + plugin probe.
        let mut rpc_builder = env.host().user_plugins().no_session();
        rpc_builder.opts.mode = mare_shared::ui::Mode::Rpc;
        let rpc_host = rpc_builder.build().await;
        let rpc_ver: Option<String> = eval(&rpc_host, "return mare.api_version").await;
        assert_eq!(rpc_ver.as_deref(), Some(mare_protocol::PROTOCOL_VERSION));
        rpc_host.shutdown().await;

        // Also check `mare test` runner exposes `mare.api_version`.
        let spec = env.cwd.join("api_version_spec.lua");
        std::fs::write(
            &spec,
            format!(
                r#"
                mare.test.describe("api_version", function()
                    mare.test.it("matches protocol version", function()
                        assert(mare.api_version == {:?}, "got " .. tostring(mare.api_version))
                    end)
                end)
                "#,
                mare_protocol::PROTOCOL_VERSION
            ),
        )
        .unwrap();
        let out = mare_cmd()
            .args(["test", spec.to_str().unwrap()])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .run_output();
        assert!(
            out.status.success(),
            "`mare test` must expose mare.api_version == {}: stdout={}\nstderr={}",
            mare_protocol::PROTOCOL_VERSION,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    })
    .await;
}

/// Runtime `mare` primitive surface and hand-written `TY/*.lua` must not retain renamed
/// `handle` fields/params, `mare.subagent.spawn`, or `mare.spill.remove`.
#[tokio::test]
async fn primitive_surface_has_no_handle_or_spawn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let report_json: String = eval(
            &h,
            r#"
            local issues = mare.json.array({})
            local allowed_spawn = { proc = true, pty = true, task = true }
            for ns, tbl in pairs(mare) do
                if type(tbl) == "table" then
                    for fn_name, val in pairs(tbl) do
                        if type(val) == "function" then
                            if fn_name == "spawn" and not allowed_spawn[ns] then
                                issues[#issues + 1] = "runtime:" .. ns .. ".spawn"
                            end
                            if ns == "spill" and fn_name == "remove" then
                                issues[#issues + 1] = "runtime:spill.remove"
                            end
                        end
                    end
                end
            end
            if type(mare.spill) == "table" then
                if type(mare.spill.delete) ~= "function" then
                    issues[#issues + 1] = "runtime:missing:mare.spill.delete"
                end
                local sid = "probe-session"
                local ref = mare.spill.save(sid, "hello")
                if ref.handle ~= nil then
                    issues[#issues + 1] = "runtime:SpillRef.handle"
                end
                if type(ref.spill_id) ~= "string" then
                    issues[#issues + 1] = "runtime:missing:SpillRef.spill_id"
                end
                if type(mare.spill.delete) == "function" and type(ref.spill_id) == "string" then
                    pcall(mare.spill.delete, ref.spill_id, { session_id = sid })
                end
            end
            return mare.json.encode(issues)
            "#,
        )
        .await;
        let mut issues: Vec<String> = serde_json::from_str(&report_json).unwrap();
        h.shutdown().await;

        let fn_re = regex::Regex::new(r"(?m)^function\s+mare\.([a-z0-9_.]+)\.([a-z0-9_]+)\s*\(").unwrap();
        let field_handle_re = regex::Regex::new(r"(?m)^---@field\s+handle\b").unwrap();
        let alias_handle_re = regex::Regex::new(r"(?m)^---@alias\s+\S+.*\bhandle\s*:").unwrap();
        let spill_param_handle_re = regex::Regex::new(r"(?m)^---@param\s+handle\b|\bhandle\s*:").unwrap();

        for (rel, src) in read_handwritten_type_files() {
            for cap in fn_re.captures_iter(&src) {
                let ns = &cap[1];
                let name = &cap[2];
                if name == "spawn" && !matches!(ns, "proc" | "pty" | "task") {
                    issues.push(format!("types:{rel}:mare.{ns}.spawn"));
                }
                if ns == "spill" && name == "remove" {
                    issues.push(format!("types:{rel}:mare.spill.remove"));
                }
            }
            if field_handle_re.is_match(&src) {
                issues.push(format!("types:{rel}:@field handle"));
            }
            if alias_handle_re.is_match(&src) {
                issues.push(format!("types:{rel}:@alias handle"));
            }
            if rel == "mare/spill.lua" && spill_param_handle_re.is_match(&src) {
                issues.push(format!("types:{rel}:handle param/field"));
            }
        }

        assert!(issues.is_empty(), "primitive surface still has banned handle/spawn/remove entries: {issues:#?}");
    })
    .await;
}

/// Every renamed primitive and field listed in the public API surface is declared in
/// `preset/types/`, and every retired name is absent from `preset/types/`.
#[test]
fn types_declare_what_migration_notes_list() {
    let files = read_handwritten_type_files();
    let combined: String = files.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n");

    let required_after = [
        ("mare.api_version", "mare.api_version ="),
        ("mare.spill.read(spill_id, opts)", "function mare.spill.read(spill_id, opts)"),
        ("mare.spill.delete(spill_id, opts)", "function mare.spill.delete(spill_id, opts)"),
        ("mare.SpillRef.spill_id", "mare.SpillRef {spill_id: string"),
        ("mare.subagent.start(opts)", "function mare.subagent.start(opts)"),
        ("mare.subagent.identity parent_session_epoch", "parent_session_epoch: string"),
        ("mare.OutputImageKey.spill_id", "---@field spill_id string"),
        ("mare.OutputImageKey.session_epoch", "---@field session_epoch string"),
        ("mare.store.IndexRow.session_id", "---@field session_id string"),
        ("mare.store.IndexRow.parent_session_id", "---@field parent_session_id? string"),
        ("mare.store.IndexRow.created_at_ms", "---@field created_at_ms integer"),
        ("mare.store.IndexRow.updated_at_ms", "---@field updated_at_ms integer"),
        ("mare.store.IndexRow.archived_at_ms", "---@field archived_at_ms? integer"),
    ];
    let mut missing = Vec::new();
    for (label, needle) in required_after {
        if !combined.contains(needle) {
            missing.push(format!("{label} (missing `{needle}`)"));
        }
    }

    let forbidden_before = [
        ("mare.subagent.spawn", "mare.subagent.spawn"),
        ("mare.spill.remove", "mare.spill.remove"),
        ("mare.spill.read(session_id, handle", "function mare.spill.read(session_id, handle"),
        ("mare.SpillRef {handle:", "mare.SpillRef {handle:"),
        ("parent_generation", "parent_generation"),
        ("PolicyService:rewrite {ok=bool}", "@return {ok: boolean, arguments?: table"),
        ("IndexRow.parent_session", "---@field parent_session? string"),
        ("IndexRow.archived_at", "---@field archived_at? number"),
        ("IndexRow.created", "---@field created number"),
        ("IndexRow.updated", "---@field updated number"),
        ("V2 header.parent_session", "V2 header.parent_session"),
    ];
    let mut leftover = Vec::new();
    for (label, needle) in forbidden_before {
        if combined.contains(needle) {
            leftover.push(format!("{label} (still found `{needle}`)"));
        }
    }
    if let Some((_, policy_src)) = files.iter().find(|(rel, _)| rel == "mare/host/policy.lua")
        && policy_src.contains("@return {ok:")
    {
        leftover.push("PolicyService:rewrite still returns `{ok: ...}` in mare/host/policy.lua".to_string());
    }

    assert!(
        missing.is_empty() && leftover.is_empty(),
        "migration ledger vs preset/types mismatch:\n  missing: {missing:#?}\n  leftover: {leftover:#?}"
    );
}
