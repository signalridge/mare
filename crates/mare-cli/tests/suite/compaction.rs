//! Compaction (pi incremental summary, reference implementation and threshold,
//! Codex compact / compact_remote / history protocol units, Kimi overflow limit): staged,
//! re-measured after each stage, a summary replaces only when it succeeded, originals stay in the
//! log, dropped messages are recorded and said.

use crate::common::*;
use serde_json::json;

mod cancellation;
mod images;
mod model_switch;
mod prepared_request;
mod remote;
mod stages;
mod summary;
mod usage_resume;

/// Record every summary request (llm:complete) as JSON in the global SENT (a list).
const RECORD_REQUESTS: &str = r#"local orig = L.complete
    SENT = {}
    L.complete = function(self, req) SENT[#SENT + 1] = mare.json.encode(req.messages) return orig(self, req) end"#;

/// A long synthetic history: `n` user/assistant pairs of ~200 words each, tagged OLD-<i>.
fn long_history(n: usize) -> String {
    format!(
        r#"local msgs = {{}}
        for i = 1, {n} do
            msgs[#msgs + 1] = {{ role = "user", content = {{ {{ type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) }} }} }}
            msgs[#msgs + 1] = {{ role = "assistant", content = {{ {{ type = "text", text = "ack" }} }} }}
        end
        A:set_messages(msgs)"#
    )
}

/// After `compact:apply`, all cold submodules must stay absent from `package.loaded` until
/// actually used, and on-demand methods continue to work identically.
#[tokio::test]
async fn compact_apply_keeps_cold_submodules_unloaded_until_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["compact"]).faux(json!([])).no_session().config(json!({
            "logo": false,
            "plugins": { "onboarding": { "plugins": false } }
        }));
        b.opts.mode = mare_shared::ui::Mode::Tui;
        let h = b.build().await;
        assert!(h.kernel.plugin_active("compact"), "compact plugin must be active after apply");
        let loaded: Vec<String> = eval(
            &h,
            r#"
            local cold = {
                "compact.measure",
                "compact.service",
                "compact.hooks",
                "compact.stages",
                "compact.projection",
                "compact.context",
                "compact.stages.cut",
                "compact.stages.commit",
                "compact.stages.prompts",
                "compact.stages.remote",
                "compact.stages.local_summary",
                "compact.stages.summarize",
            }
            local found = {}
            for _, name in ipairs(cold) do
                if package.loaded[name] ~= nil then
                    found[#found + 1] = name
                end
            end
            return found
            "#,
        )
        .await;
        assert!(loaded.is_empty(), "cold compact submodules loaded during apply: {loaded:?}");

        // On-demand threshold still works and leaves heavier compaction stages unloaded.
        global(&h, "COMPACT", h.service_table("compact").unwrap());
        let (thresh, stages_loaded): (Option<i64>, bool) = eval(
            &h,
            r#"
            local t = COMPACT:threshold()
            return t, package.loaded["compact.stages"] ~= nil or package.loaded["compact.context"] ~= nil
            "#,
        )
        .await;
        assert!(thresh.is_some_and(|v| v > 0), "compact:threshold() must work on demand: {thresh:?}");
        assert!(!stages_loaded, "threshold() must not eagerly load compact.stages or compact.context");
        h.shutdown().await;
    })
    .await
}

/// After `/reload` (when `app/ready` will not replay), `compact` re-runs its model budget check
/// so an invalid model budget configured on reload is reported immediately.
#[tokio::test]
async fn compact_reload_reruns_model_budget_check_after_startup() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init_lua = env.config_dir().join("init.lua");
        std::fs::write(&init_lua, "mare.setup({ model = 'faux/echo' })\n").unwrap();
        let h = env
            .host()
            .user_config(false)
            .faux(json!([]))
            .no_session()
            .config(json!({ "plugins": { "onboarding": { "plugins": false } } }))
            .build()
            .await;
        expose(&h);

        exec(
            &h,
            r#"
            RELOAD_NOTICES = {}
            C.cordis:on("host/notice", function(ev)
                if type(ev) == "table" and ev.text then
                    RELOAD_NOTICES[#RELOAD_NOTICES + 1] = tostring(ev.text)
                end
            end)
            "#,
        )
        .await;

        // Reconfigure compact with headroom exceeding the model window and reload config.
        std::fs::write(
            &init_lua,
            "mare.setup({ model = 'faux/echo' })\nmare.plugin('compact', { headroom_tokens = 1000000 })\n",
        )
        .unwrap();
        h.reload_all().await.unwrap();
        wait_until(WAIT_SECS, "budget error notice after config reload", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"for _, n in ipairs(RELOAD_NOTICES) do
                        if n:find("no pressure budget", 1, true) then return true end
                    end
                    return false"#,
                )
                .await
            }
        })
        .await;

        // Also verify single-plugin reload (`kernel.reload(Some("compact"))`) re-runs the check.
        exec(&h, "RELOAD_NOTICES = {}").await;
        h.kernel.reload(Some("compact")).await.unwrap();
        wait_until(WAIT_SECS, "budget error notice after single-plugin reload", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"for _, n in ipairs(RELOAD_NOTICES) do
                        if n:find("no pressure budget", 1, true) then return true end
                    end
                    return false"#,
                )
                .await
            }
        })
        .await;

        h.shutdown().await;
    })
    .await
}
