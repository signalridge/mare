//! One event bus on the cordis tree (cke-interface.md "One bus on the tree"): what the Rust
//! emitters dispatch reaches a native plugin's listeners on the root's events service, with mare's
//! semantics (mare_kernel/bus.lua), and a Rust policy decision folds the policy service's voters.

use crate::common::*;

/// A native plugin listening the native way, on its own cordis ctx.
const LISTENER: &str = r#"return { name = "listener", inject = { "policy" },
  contracts = {
    custom = {
      owner = "listener",
      events = {
        note = { doc = "note", mode = "emit", expose = "process" },
        checked = { doc = "checked", mode = "emit", expose = "process" },
        ask = { doc = "ask", mode = "bail", expose = "process" },
        text = { doc = "text", mode = "waterfall", expose = "process" },
        ["text-broken"] = { doc = "broken", mode = "waterfall", expose = "process" },
      },
    },
  },
  apply = function(ctx)
  mare._seen = {}
  ctx:on("custom/note", function(v) table.insert(mare._seen, "a:" .. v.n) return "ignored" end)
  ctx:on("custom/note", function(v) error("broken listener") end)
  ctx:on("custom/note", function(v) table.insert(mare._seen, "c:" .. v.n) end)
  ctx:on("custom/checked", function() error("refuse") end)
  ctx:on("custom/ask", function(q) if q.who == "me" then return { answer = 42 } end end)
  -- around form: the first registered is outermost and passes its value on with next(value, ...)
  ctx:on("custom/text", function(text, sep, next) return next(text .. sep .. "x", sep) end)
  ctx:on("custom/text", function(text, sep, next) return next(text .. sep .. "y", sep) end)
  ctx:on("custom/text-broken", function(text, next) error("bad step") end)
  ctx.policy:voter("net/approval", function(req)
    table.insert(mare._seen, "voter:" .. req.host)
    if req.host == "evil.example" then return { decision = "deny", reason = "evil" } end
  end)
end }"#;

const SEEN: &str = "local s = table.concat(mare._seen, ',') mare._seen = {} return s";

#[tokio::test]
async fn rust_emitters_reach_native_listeners_on_the_tree() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("listener.lua"), LISTENER).unwrap();
        let h = mare_host::Host::new(env.host().user_plugins().no_session().opts).await.unwrap();
        expose(&h);
        let k = &h.kernel;

        // emit: every listener in order, a failing one does not stop the rest, results ignored
        k.emit_json("custom/note", serde_json::json!({ "n": 1 })).await;
        assert_eq!(eval::<String>(&h, SEEN).await, "a:1,c:1", "{}", k.problems().join("\n"));
        assert!(k.problems().iter().any(|p| p.contains("broken listener")), "the failure is reported");

        // emit_checked: the first failure fails the dispatch, naming the plugin
        let lua = h.lua();
        let err = k.emit_checked("custom/checked", mlua::MultiValue::new()).await.unwrap_err().to_string();
        assert!(err.contains("custom/checked listener listener failed") && err.contains("refuse"), "{err}");

        // bail: the first non-nil answer
        assert_eq!(
            k.bail_json("custom/ask", serde_json::json!({ "who": "me" })).await,
            Some(serde_json::json!({ "answer": 42 }))
        );
        assert_eq!(k.bail_json("custom/ask", serde_json::json!({ "who": "you" })).await, None);

        // waterfall: in registration order, each step's value passed on
        let sep = mlua::Value::String(lua.create_string("-").unwrap());
        let start = mlua::Value::String(lua.create_string("s").unwrap());
        let text = k.waterfall("custom/text", start.clone(), mlua::MultiValue::from_vec(vec![sep])).await;
        assert_eq!(text.as_string().map(|s| s.to_string_lossy()), Some("s-x-y".into()));
        // unchecked keeps the value on a failure; checked fails
        let kept = k.waterfall("custom/text-broken", start.clone(), mlua::MultiValue::new()).await;
        assert_eq!(kept.as_string().map(|s| s.to_string_lossy()), Some("s".into()));
        assert!(k.waterfall_checked("custom/text-broken", start, mlua::MultiValue::new()).await.is_err());

        // a Rust policy decision folds the policy service's voters
        let ask = |host: &str| {
            let t = lua.create_table().unwrap();
            t.set("host", host).unwrap();
            t.set("port", 443).unwrap();
            mlua::MultiValue::from_vec(vec![mlua::Value::Table(t)])
        };
        let denied = k.policy("net/approval", ask("evil.example")).await.unwrap();
        assert_eq!(denied.get::<String>("decision").unwrap(), "deny");
        assert_eq!(eval::<String>(&h, SEEN).await, "voter:evil.example");
        h.shutdown().await;
    })
    .await
}

fn order_of(owners: &[String], wanted: &[&str]) -> Vec<String> {
    owners.iter().filter(|o| wanted.contains(&o.as_str())).cloned().collect()
}

/// Whether `seen` lists its owners in `order`'s order (each at most once; absent ones allowed).
fn in_order(seen: &[String], order: &[&str]) -> bool {
    let pos: Vec<usize> = seen.iter().filter_map(|o| order.iter().position(|w| w == o)).collect();
    pos.len() == seen.len() && pos.windows(2).all(|w| w[0] < w[1])
}

/// The ordered chains the old engine kept by numeric priority (priority-sites.md) keep that order on
/// the bus until they move to their owners' ordered registrations: request/ready voters
/// llm → mcp → context (after the host's vote). Readiness preparation is llm's ordered chain
/// (llm:readiness, READY_ORDER) and no longer an event of llm's: what is still on request/prepare
/// and request/ready keeps the old order until it moves to the chain too. input/submit and
/// key/interrupt left the bus for ui's ordered chains (ui:handler, 44d80b8d; their order is
/// tui::keys::ui_handlers_run_in_order_and_the_first_that_handles_stops_the_chain): on the bus only
/// ui's one dispatcher listens for them.
#[tokio::test]
async fn the_old_engines_ordered_chains_keep_their_order_on_the_bus() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(serde_json::json!([])).build().await;
        let owners = |e: &str| h.kernel.listener_owners(e);
        let prepare = order_of(&owners("request/prepare"), &["llm", "mcp", "context"]);
        assert!(!prepare.iter().any(|o| o == "llm"), "llm prepares through its chain: {prepare:?}");
        assert!(in_order(&prepare, &["mcp", "context"]), "{prepare:?}");
        let ready = owners("request/ready");
        assert_eq!(ready.first().map(String::as_str), Some("<root>"), "the host's vote first: {ready:?}");
        let voters = order_of(&ready, &["llm", "mcp", "context"]);
        assert_eq!(voters.first().map(String::as_str), Some("llm"), "{ready:?}");
        assert!(in_order(&voters, &["llm", "mcp", "context"]), "{ready:?}");
        for event in ["input/submit", "key/interrupt"] {
            let stepped = order_of(&owners(event), &["commands", "shell-mode", "review", "compact"]);
            assert!(stepped.is_empty(), "{event}: the steps are ui:handler registrations now: {stepped:?}");
        }
        h.shutdown().await;
    })
    .await
}

/// a Rust host listener (Kernel::host_on) is a native registration on the host's fiber:
/// it runs in the host's context (`exec_ctx == nil`), not in a restricted emitter's `exec_ctx`.
#[tokio::test]
async fn a_host_listener_runs_in_the_hosts_context_whoever_emits() {
    local(async {
        let env = TestEnv::new().await;
        install_probe_events_plugin(&env, "custom", &[("host-x1", "emit")]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let seen = std::rc::Rc::new(std::cell::Cell::new(None::<bool>));
        let record = seen.clone();
        let f = h
            .lua()
            .create_function(move |lua, _: mlua::MultiValue| {
                record.set(Some(mare_shared::exec_ctx::current(lua).is_some()));
                Ok(())
            })
            .unwrap();
        let (ctx, owner) = (h.kernel.host_ctx().unwrap(), h.kernel.host_owner().unwrap());
        h.kernel.host_on(&ctx, owner, "custom/host-x1", f, mare_kernel::HostOn::default()).unwrap();
        exec(
            &h,
            r#"
            local restricted = {sandbox={level="read-only"}}
            mare.exec.run(restricted, function()
                C.cordis.bus:emit("custom/host-x1")
            end)
        "#,
        )
        .await;
        let owners = h.kernel.listener_owners("custom/host-x1");
        h.shutdown().await;
        assert_eq!(seen.get(), Some(false), "the host listener ran under the restricted emitter's exec_ctx");
        assert_eq!(owners, ["<root>"]);
    })
    .await
}

#[tokio::test]
async fn a_restricted_registrants_native_listener_does_not_get_the_emitters_rights() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local sid = S:info().session_id
            ITEM = mare.spill.save(sid, "secret")
            local restricted = {sandbox={level="read-only",network=true,
                deny_read={mare.paths.state .. "/spill-results"}, accept_partial={"fs_deny_read"}}}
            assert(mare.exec.run(restricted, function()
                C.cordis:on("custom/x1", function() NATIVE = pcall(mare.spill.read, ITEM.spill_id) end)
                return true
            end))
            "#,
        )
        .await;
        h.kernel.emit_json("custom/x1", serde_json::json!({})).await;
        let native: Option<bool> = eval(&h, "return NATIVE").await;
        assert_eq!(native, Some(false), "the native listener must have run and been refused (nil = never ran)");
        h.shutdown().await;
    })
    .await
}
