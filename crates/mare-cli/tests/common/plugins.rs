use std::path::PathBuf;

use super::TestEnv;

pub const APPROVAL_PRIVACY_SECRET: &str = "UNLABELLED_INTERNAL_CREDENTIAL_4039";
pub fn approval_privacy_fixture(home: &std::path::Path) {
    let plugins = home.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("approval-privacy.lua"),
        r#"
        return {name="approval-privacy", inject={"tools","policy"},apply=function(ctx)
            local secret="UNLABELLED_INTERNAL_CREDENTIAL_4039"
            local observed=false
            ctx.tools:register("audit_effect",{description="audit boundary fixture",side_effect="external",
                parameters={type="object",properties={visible={type="string"},opaque={type="string"}},required={"visible"}},
                execute=function(args) assert(observed and args.opaque==secret);return {content="observed"} end})
            ctx.policy:rewriter("tool/rewrite",function(call)
                if call.name=="audit_effect" then return {visible=call.arguments.visible,opaque=secret} end
            end)
            ctx.policy:voter("tool/before",function(call) if call.name=="audit_effect" then return {decision="ask"} end end)
            ctx.policy:voter("tool/approval",function() if mare.mode=="rpc" then return {decision="allow"} end end)
            ctx:on("pipeline/approval-decided",function(event)
                if event.call.name=="audit_effect" then
                    assert(event.call.arguments.opaque==secret)
                    observed=true
                    assert(mare.fs.write(mare.cwd.."/private-audit-observed","yes"))
                end
            end)
        end}
    "#,
    )
    .unwrap();
}

/// Real plugin ctxs for tests (the root ctx `C` is not a plugin ctx — no `spawn`,
/// `with_service`, `service`, `plugins`). One user plugin per name hands its ctx to the test as
/// `mare._pctx[name]`. Build the host with `.user_plugins`; unload one with `unload_plugin` (a
/// rows-driven reload that restarts only changed rows; a full `reload_all` restarts every plugin).
pub fn ctx_probes(env: &TestEnv, names: &[&str]) -> Vec<PathBuf> {
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    names
        .iter()
        .map(|n| {
            let path = dir.join(format!("ctx-{n}.lua"));
            std::fs::write(
                &path,
                format!(
                    "return {{ name = \"ctx-{n}\", inject = {{ \"ui\", \"task\" }}, apply = function(ctx) mare._pctx = mare._pctx or {{}}; mare._pctx[\"{n}\"] = ctx end }}"
                ),
            )
            .unwrap();
            path
        })
        .collect()
}

/// Whether a plugin is running, through the status the host reports (`plugin_status`, which reads
/// the plugin tree) — not the kernel's internal active set.
pub fn plugin_active(k: &mare_kernel::Kernel, name: &str) -> bool {
    k.plugin_status().iter().any(|(n, _, active)| n == name && *active)
}

/// Unload one plugin the way a configuration change does: drop its definition and let the plugin
/// tree follow the new row list. A rows-driven reload: the other plugins keep running
/// (unchanged fingerprints), unlike a user's full /reload, which restarts every plugin (520e66ea).
pub async fn unload_plugin(k: &mare_kernel::Kernel, name: &str) {
    let defs: Vec<_> = k.def_names().into_iter().filter(|n| n != name).filter_map(|n| k.def(&n)).collect();
    k.sync_defs(defs).await;
    k.apply_rows_report().await.unwrap();
}

/// Install a minimal user plugin declaring temporary probe events under `root` (`(short, mode)`
/// pairs) so tests dispatching synthetic events via `ctx.bus` / `C.cordis` satisfy event-registry
/// declaration checks.
pub fn install_probe_events_plugin(env: &TestEnv, root: &str, events: &[(&str, &str)]) -> PathBuf {
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    let plugin_name = format!("probe-events-{root}");
    let mut entries = String::new();
    for (short, mode) in events {
        use std::fmt::Write as _;
        let extra =
            if *mode == "waterfall" { r#", result = { type = "object", additionalProperties = true }"# } else { "" };
        let _ = writeln!(
            entries,
            "[\"{short}\"] = {{ doc = \"test probe event\", mode = \"{mode}\", expose = \"both\", delivery = \"required\", ignorable = false, data = {{ type = \"object\", additionalProperties = true }}, example = {{}}{extra} }},"
        );
    }
    let path = dir.join(format!("{plugin_name}.lua"));
    std::fs::write(
        &path,
        format!(
            "return {{ name = \"{plugin_name}\", contracts = {{ [\"{root}\"] = {{ owner = \"{plugin_name}\", events = {{\n{entries}}} }} }}, apply = function() end }}\n"
        ),
    )
    .unwrap();
    path
}
