//! The runtime config write transaction:
//! one host queue runs each write as stage → owner apply → publish → settle | abort. The cells
//! below form the transaction matrix; every cell checks the return, the committed value and its source, the
//! running `cfg` (and that it equals what a restart computes), the owner's live state, the exact
//! ordered `option/changed` list, and that the queue still answers afterwards.
//!
//! Two stand-in owners (fixtures, not product): `own` applies each leaf as `TX.SCRIPT[path]`
//! says (`ok`, `fail`, `block`, `fail_on_restore`), records every call in `TX.APPLIED` and what
//! it took in `TX.LIVE` (`TX` is `mare.__tx_test`, shared with the test), reports `TX.LIVE` on `option/current` when `TX.STATEFUL`, and resets
//! from `cfg` on `test/reset` (like the approvals plugin on `session/new`); `second` only listens
//! to `option/apply` and follows `TX.SCRIPT2`. Barriers are channels: a blocked apply sends its
//! path on `TX.arrive` and waits for "ok" / "fail" on `TX.release`.
//!
//! Reference tests: Kimi `packages/agent-core-v2/test/app/config/config.test.ts:3244` (a refused
//! write leaves memory and file untouched), `:3048` (all domains or none), `:2993` (announced once
//! in effect); reference implementation (concurrent writes),
//! `:79` (sibling secrets kept), `:151` (refused activation restores), `:22` (unset → default);
//! Codex `codex-rs/app-server/tests/suite/v2/config_rpc.rs:2090` (an invalid batch writes nothing),
//! `:2489` (a batch of edits). Compensating owner state and per-leaf unconfirmed reporting have no
//! counterpart there; those cells use the controllable owners.
use crate::common::*;
use serde_json::{Value, json};

const OWN: &str = r#"
return { name = "own",
  config_schema = {
    flag = { type = "boolean", default = true, apply = "live", desc = "a switch" },
    num = { type = "integer", apply = "live", desc = "no default" },
    str = { type = "string", apply = "live", desc = "no default" },
    obj = { type = "object", apply = "live", desc = "an object", fields = {
      a = { type = "integer", default = 1, desc = "a" }, b = { type = "integer", default = 2, desc = "b" },
      c = { type = "integer", desc = "c" } } },
    list = { type = "list", items = { type = "string" }, apply = "live", desc = "a list" },
    u = { type = "union", apply = "live", desc = "a union", variants = {
      { type = "enum", choices = { "x", "y", "z" } },
      { type = "object", fields = { mode = { type = "string", desc = "m" } } } } },
    srv = { type = "map", apply = "live", desc = "servers", values = { type = "object", fields = {
      tier = { type = "string", desc = "t" }, env = { type = "map", values = { type = "string" }, desc = "e" },
      pin = { type = "string", sensitive = true, desc = "p" } } } },
    cold = { type = "string", desc = "reloads" },
  },
  contracts = {
    txn = {
      owner = "own",
      events = {
        nested = { doc = "test nested transaction probe", mode = "parallel", expose = "process" },
      },
    },
  },
  apply = function(ctx, cfg)
    -- plugins have their own globals; the `mare` table is shared with the test
    mare.__tx_test = mare.__tx_test or {}
    local TX = mare.__tx_test
    TX.CFG_OWN, TX.LIVE, TX.APPLIED = cfg, TX.LIVE or {}, TX.APPLIED or {}
    TX.SCRIPT = TX.SCRIPT or {}
    ctx:on("txn/nested", function()
      local ok, err = pcall(mare.plugin, "own", { str = "nested" })
      TX.NESTED_RESULT = ok and "ok" or tostring(err)
    end)
    ctx:on("option/apply", function(ev)
      if ev.name:sub(1, 12) ~= "plugins.own." then return end
      local act = TX.SCRIPT[ev.name] or "ok"
      TX.APPLIED[#TX.APPLIED + 1] = { name = ev.name, value = ev.value, restore = ev.restore == true }
      if act == "fail_on_restore" then
        if ev.restore then error("own cannot restore " .. ev.name) end
        act = "ok"
      end
      if act == "block" then
        TX.arrive:send(ev.name)
        if TX.release:recv() == "fail" then error("own refuses " .. ev.name) end
      elseif act == "spawn_write" and not ev.restore then
        ctx:parallel("txn/nested"):await()
      elseif act == "fail" and not ev.restore then
        error("own refuses " .. ev.name)
      end
      TX.LIVE[ev.name] = ev.value
    end)
    ctx:on("option/current", function(ev)
      if TX.STATEFUL and ev.name:sub(1, 12) == "plugins.own." then return TX.LIVE[ev.name] end
    end)
    ctx:on("test/reset", function()
      for path in pairs(TX.LIVE) do
        local v = cfg
        for part in path:sub(13):gmatch("[^.]+") do v = type(v) == "table" and v[part] or nil end
        TX.LIVE[path] = v
      end
    end)
  end,
}
"#;

const SECOND: &str = r#"
return { name = "second",
  config_schema = { opt = { type = "string", apply = "live", desc = "the second plugin's option" } },
  apply = function(ctx)
    -- plugins have their own globals; the `mare` table is shared with the test
    mare.__tx_test = mare.__tx_test or {}
    local TX = mare.__tx_test
    TX.SCRIPT2 = TX.SCRIPT2 or {}
    ctx:on("option/apply", function(ev)
      if TX.SCRIPT2[ev.name] == "fail" and not ev.restore then error("second refuses " .. ev.name) end
    end)
  end,
}
"#;

/// Helpers in the host's Lua: `W(lua_table_source)` writes and returns `"ok"` or the error;
/// `STATE(key)` is everything a cell checks about `plugins.own.<key>`.
const HELPERS: &str = r#"
TX = mare.__tx_test
CHANGED = {}
C.cordis:on("option/changed", function(e)
  if e.name:find("^plugins%.") then CHANGED[#CHANGED + 1] = { name = e.name, value = e.value, redacted = e.redacted } end
end)
TX.arrive, TX.arrive_rx = nil, nil
do local s, r = mare.task.channel() TX.arrive, TX.arrive_rx = s, r end
do local s, r = mare.task.channel() TX.release_tx, TX.release = s, r end
function W(f) local ok, e = pcall(f) return ok and "ok" or tostring(e) end
function STATE(key)
  local own = (mare.config.plugins or {}).own or {}
  local src = mare.config_sources()["plugins.own." .. key]
  local committed = own[key]
  local restart = PLUGIN_EFFECTIVE("own", key, committed ~= nil, committed)
  return mare.json.encode({
    committed = committed == nil and "<absent>" or committed,
    source = src and src.layer or "<none>",
    running = TX.CFG_OWN[key] == nil and "<absent>" or TX.CFG_OWN[key],
    restart_equal = mare.json.encode(restart) == mare.json.encode(TX.CFG_OWN[key]),
    changed = CHANGED,
    applied = TX.APPLIED,
  })
end
function OPT_SET(values)
  return C.cordis:get("options"):set(values)
end
function RESET() CHANGED = {} TX.APPLIED = {} end
function TAIL(n) return W(function() mare.plugin("own", { str = "tail" .. n }) end) end
"#;

async fn host(init: &str) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
    std::fs::write(env.config_dir().join("plugins/own.lua"), OWN).unwrap();
    std::fs::write(env.config_dir().join("plugins/second.lua"), SECOND).unwrap();
    std::fs::write(env.config_dir().join("init.lua"), format!("mare.setup {{ model = 'faux/echo' }}\n{init}")).unwrap();
    let h = env.host().user_config(false).no_session().build().await;
    expose(&h);
    let eff = mare_shared::lua::core::config::plugin_effective_fn(h.lua()).unwrap();
    h.lua().globals().set("PLUGIN_EFFECTIVE", eff).unwrap();
    exec(&h, HELPERS).await;
    (env, h)
}

async fn state(h: &mare_host::Host, key: &str) -> Value {
    let s: String = eval(h, &format!("return STATE('{key}')")).await;
    let mut v: Value = serde_json::from_str(&s).unwrap();
    // an empty Lua table encodes as `{}`: the lists are lists
    for k in ["changed", "applied"] {
        if v[k] == json!({}) {
            v[k] = json!([]);
        }
    }
    v
}

async fn write(h: &mare_host::Host, lua: &str) -> String {
    eval(h, &format!("return W(function() {lua} end)")).await
}

async fn tail_ok(h: &mare_host::Host, n: u32) {
    assert_eq!(eval::<String>(h, &format!("return TAIL({n})")).await, "ok", "the queue still answers");
}

fn names(changed: &Value) -> Vec<String> {
    changed.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect()
}

/// The config value domain as Lua literals: false, 0, "", and absent (nil).
const DOMAIN: [&str; 4] = ["false", "0", "''", "nil"];

// ---------- A: success and merging ----------

/// a write commits, runs and is announced once.
#[tokio::test]
async fn a_write_commits_runs_and_is_announced() {
    local(async {
        let (_env, h) = host("").await;
        assert_eq!(write(&h, "mare.plugin('own', { num = 5 })").await, "ok");
        let s = state(&h, "num").await;
        assert_eq!(
            (s["committed"].clone(), s["running"].clone(), s["source"].clone()),
            (json!(5), json!(5), json!("runtime"))
        );
        assert_eq!(s["restart_equal"], true);
        assert_eq!(names(&s["changed"]), vec!["plugins.own.num"]);
        assert_eq!(eval::<i64>(&h, "return TX.LIVE['plugins.own.num']").await, 5);
        tail_ok(&h, 1).await;
        h.shutdown().await;
    })
    .await
}

/// an object written whole or by a dotted path merges into the committed value (absent:
/// only what was written); the running value is default + committed, the announcement only `a`.
#[tokio::test]
async fn an_object_merges_and_runs_with_its_defaults() {
    local(async {
        for dotted in [false, true] {
            let (_env, h) = host("").await;
            let w = if dotted {
                "OPT_SET({ ['plugins.own.obj.a'] = 3 })"
            } else {
                "mare.plugin('own', { obj = { a = 3 } })"
            };
            assert_eq!(write(&h, w).await, "ok");
            let s = state(&h, "obj").await;
            assert_eq!(s["committed"], json!({ "a": 3 }), "dotted={dotted}");
            assert_eq!(s["running"], json!({ "a": 3, "b": 2 }), "dotted={dotted}");
            assert_eq!(s["restart_equal"], true);
            assert_eq!(names(&s["changed"]), vec!["plugins.own.obj.a"]);
            tail_ok(&h, 2).await;
            h.shutdown().await;
        }
    })
    .await
}

/// a union and a list are replaced whole and announced as one leaf.
#[tokio::test]
async fn a_union_and_a_list_are_replaced_whole() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { u = { mode = 'm' }, list = { 'p', 'q' } })").await;
        assert_eq!(write(&h, "mare.plugin('own', { u = 'y' })").await, "ok");
        assert_eq!(write(&h, "mare.plugin('own', { list = { 'r' } })").await, "ok");
        let (u, l) = (state(&h, "u").await, state(&h, "list").await);
        assert_eq!((u["running"].clone(), l["running"].clone()), (json!("y"), json!(["r"])));
        assert_eq!(names(&l["changed"]), vec!["plugins.own.u", "plugins.own.list"]);
        tail_ok(&h, 3).await;
        h.shutdown().await;
    })
    .await
}

/// a new map entry keeps the existing one (with its credential) and announces only its leaf.
#[tokio::test]
async fn a_map_entry_keeps_its_siblings() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { srv = { gh = { tier = 'a', env = { TOKEN = 's3cret' } } } })").await;
        assert_eq!(write(&h, "OPT_SET({ ['plugins.own.srv.hub.tier'] = 'x' })").await, "ok");
        let s = state(&h, "srv").await;
        assert_eq!(
            s["running"],
            json!({ "gh": { "tier": "a", "env": { "TOKEN": "s3cret" } }, "hub": { "tier": "x" } })
        );
        assert_eq!(names(&s["changed"]), vec!["plugins.own.srv.hub.tier"]);
        tail_ok(&h, 4).await;
        h.shutdown().await;
    })
    .await
}

/// `false` stays false (not nil) in config, running cfg, owner and announcement.
#[tokio::test]
async fn false_stays_false() {
    local(async {
        let (_env, h) = host("").await;
        assert_eq!(write(&h, "mare.plugin('own', { flag = false })").await, "ok");
        let s = state(&h, "flag").await;
        assert_eq!((s["committed"].clone(), s["running"].clone()), (json!(false), json!(false)));
        assert_eq!(s["changed"], json!([{ "name": "plugins.own.flag", "value": false }]));
        assert_eq!(eval::<String>(&h, "return tostring(TX.LIVE['plugins.own.flag'])").await, "false");
        tail_ok(&h, 5).await;
        h.shutdown().await;
    })
    .await
}

/// writing the value already in effect applies and announces nothing.
#[tokio::test]
async fn the_same_value_applies_and_announces_nothing() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 4 })").await;
        assert_eq!(write(&h, "mare.plugin('own', { num = 4 })").await, "ok");
        let s = state(&h, "num").await;
        assert_eq!((s["changed"].clone(), s["applied"].clone()), (json!([]), json!([])));
        tail_ok(&h, 6).await;
        h.shutdown().await;
    })
    .await
}

/// the owner's live value differs from the config; writing the config's own value still
/// reaches the owner (the leaves come from the owner's view, not the config's).
#[tokio::test]
async fn the_owner_view_decides_what_changed() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { u = 'x' })").await;
        exec(&h, "TX.STATEFUL = true TX.LIVE['plugins.own.u'] = 'z'").await;
        assert_eq!(write(&h, "mare.plugin('own', { u = 'x' })").await, "ok");
        let s = state(&h, "u").await;
        assert_eq!(names(&s["changed"]), vec!["plugins.own.u"]);
        assert_eq!(eval::<String>(&h, "return TX.LIVE['plugins.own.u']").await, "x");
        tail_ok(&h, 7).await;
        h.shutdown().await;
    })
    .await
}

// ---------- B: refusal and rollback ----------

/// a refused write leaves everything old and announces nothing.
#[tokio::test]
async fn a_refused_write_changes_nothing() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        exec(&h, "RESET() TX.SCRIPT['plugins.own.num'] = 'fail'").await;
        let e = write(&h, "mare.plugin('own', { num = 5 })").await;
        assert!(e.contains("own refuses plugins.own.num") && !e.contains("unconfirmed"), "{e}");
        let s = state(&h, "num").await;
        assert_eq!(
            (s["committed"].clone(), s["running"].clone(), s["source"].clone()),
            (json!(1), json!(1), json!("user"))
        );
        assert_eq!(s["changed"], json!([]));
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil").await;
        tail_ok(&h, 8).await;
        h.shutdown().await;
    })
    .await
}

/// one plugin's option applies, the other plugin's is refused; the first
/// owner is brought back (to `nil` when the old value was absent) and everything is old — `false`
/// stays `false`.
#[tokio::test]
async fn a_refused_batch_brings_the_first_owner_back() {
    local(async {
        for old in DOMAIN {
            // each value on an option of its type: false → flag, 0 → num, "" / absent → str
            let (key, new) = match old {
                "false" => ("flag", "true"),
                "0" => ("num", "5"),
                _ => ("str", "'new'"),
            };
            let init = if old == "nil" { String::new() } else { format!("mare.plugin('own', {{ {key} = {old} }})") };
            let (_env, h) = host(&init).await;
            exec(&h, "RESET() TX.SCRIPT2['plugins.second.opt'] = 'fail'").await;
            let e = write(&h, &format!("OPT_SET({{ ['plugins.own.{key}'] = {new}, ['plugins.second.opt'] = 'z' }})")).await;
            assert!(e.contains("second refuses plugins.second.opt"), "{old}: {e}");
            let s = state(&h, key).await;
            let expect = match old {
                "false" => json!(false),
                "0" => json!(0),
                "''" => json!(""),
                _ => json!("<absent>"),
            };
            assert_eq!(s["committed"], expect, "{old}");
            assert_eq!(s["running"], expect, "{old}: the running cfg is strictly the old value");
            assert_eq!(s["changed"], json!([]), "{old}");
            let restored: String = eval(&h, "local a = TX.APPLIED[#TX.APPLIED] local v = a.value if v == nil then v = '<nil>' end return mare.json.encode({ a.restore, v })").await;
            let expect_owner = match old {
                "false" => "[true,false]".to_string(),
                "nil" => "[true,\"<nil>\"]".to_string(),
                "0" => "[true,0]".to_string(),
                _ => "[true,\"\"]".to_string(),
            };
            assert_eq!(restored, expect_owner, "{old}: the owner is told the old value");
            exec(&h, "TX.SCRIPT2['plugins.second.opt'] = nil").await;
            tail_ok(&h, 9).await;
            h.shutdown().await;
        }
    })
    .await
}

/// a refused nested update leaves no new leaf in the running cfg.
#[tokio::test]
async fn a_refused_nested_update_leaves_no_new_leaf() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { obj = { a = 1, b = 2 } })").await;
        exec(&h, "RESET() TX.SCRIPT2['plugins.second.opt'] = 'fail'").await;
        let e = write(&h, "OPT_SET({ ['plugins.own.obj'] = { c = 9 }, ['plugins.second.opt'] = 'z' })").await;
        assert!(e.contains("second refuses"), "{e}");
        let s = state(&h, "obj").await;
        assert_eq!(s["running"], json!({ "a": 1, "b": 2 }));
        assert_eq!(s["committed"], json!({ "a": 1, "b": 2 }));
        exec(&h, "TX.SCRIPT2['plugins.second.opt'] = nil").await;
        tail_ok(&h, 10).await;
        h.shutdown().await;
    })
    .await
}

/// `own` takes the leaf, `second` refuses the same leaf: `own` is brought back (the leaf was
/// attempted before it was offered).
#[tokio::test]
async fn a_later_listener_refusing_brings_the_earlier_back() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        exec(&h, "RESET() TX.SCRIPT2['plugins.own.num'] = 'fail'").await;
        let e = write(&h, "mare.plugin('own', { num = 5 })").await;
        assert!(e.contains("second refuses plugins.own.num"), "{e}");
        assert_eq!(eval::<i64>(&h, "return TX.LIVE['plugins.own.num']").await, 1);
        exec(&h, "TX.SCRIPT2['plugins.own.num'] = nil").await;
        tail_ok(&h, 11).await;
        h.shutdown().await;
    })
    .await
}

/// the owner's live value X differs from the config's C0; a refused batch brings it back to X.
#[tokio::test]
async fn the_owner_returns_to_its_own_live_value() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { u = 'x' })").await;
        exec(&h, "TX.STATEFUL = true TX.LIVE['plugins.own.u'] = 'z' RESET() TX.SCRIPT2['plugins.second.opt'] = 'fail'")
            .await;
        let e = write(&h, "OPT_SET({ ['plugins.own.u'] = 'y', ['plugins.second.opt'] = 'q' })").await;
        assert!(e.contains("second refuses"), "{e}");
        assert_eq!(eval::<String>(&h, "return TX.LIVE['plugins.own.u']").await, "z", "not the config's x");
        exec(&h, "TX.SCRIPT2['plugins.second.opt'] = nil").await;
        tail_ok(&h, 12).await;
        h.shutdown().await;
    })
    .await
}

/// an owner that refuses before changing anything, and reports its live value, is not sent a
/// restore.
#[tokio::test]
async fn an_unchanged_owner_is_not_sent_a_restore() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        exec(&h, "TX.STATEFUL = true TX.LIVE['plugins.own.num'] = 1 RESET() TX.SCRIPT['plugins.own.num'] = 'fail'")
            .await;
        let e = write(&h, "mare.plugin('own', { num = 5 })").await;
        assert!(e.contains("own refuses") && !e.contains("unconfirmed"), "{e}");
        let restores: i64 =
            eval(&h, "local n = 0 for _, a in ipairs(TX.APPLIED) do if a.restore then n = n + 1 end end return n")
                .await;
        assert_eq!(restores, 0);
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil").await;
        tail_ok(&h, 13).await;
        h.shutdown().await;
    })
    .await
}

// ---------- C: compensation failures ----------

/// the restore of `num` fails: the error lists exactly that leaf; config and running cfg are
/// old, nothing is announced, the owner keeps the new value it could not give back.
#[tokio::test]
async fn a_failed_restore_is_listed_and_nothing_is_published() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        exec(&h, "RESET() TX.SCRIPT['plugins.own.num'] = 'fail_on_restore' TX.SCRIPT2['plugins.second.opt'] = 'fail'")
            .await;
        let e = write(&h, "OPT_SET({ ['plugins.own.num'] = 5, ['plugins.second.opt'] = 'z' })").await;
        assert!(e.contains("second refuses") && e.contains("owner state unconfirmed: plugins.own.num"), "{e}");
        let s = state(&h, "num").await;
        assert_eq!(
            (s["committed"].clone(), s["running"].clone(), s["changed"].clone()),
            (json!(1), json!(1), json!([]))
        );
        assert_eq!(eval::<i64>(&h, "return TX.LIVE['plugins.own.num']").await, 5);
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil TX.SCRIPT2['plugins.second.opt'] = nil").await;
        tail_ok(&h, 14).await;
        h.shutdown().await;
    })
    .await
}

/// within one object, `b`'s restore fails and `a`'s succeeds: only `obj.b` is unconfirmed.
#[tokio::test]
async fn unconfirmed_is_per_leaf() {
    local(async {
        let (_env, h) = host("").await;
        exec(
            &h,
            "RESET() TX.SCRIPT['plugins.own.obj.b'] = 'fail_on_restore' TX.SCRIPT2['plugins.second.opt'] = 'fail'",
        )
        .await;
        let e = write(&h, "OPT_SET({ ['plugins.own.obj'] = { a = 7, b = 8 }, ['plugins.second.opt'] = 'z' })").await;
        assert!(e.ends_with("owner state unconfirmed: plugins.own.obj.b"), "{e}");
        let live: String =
            eval(&h, "return mare.json.encode({ TX.LIVE['plugins.own.obj.a'], TX.LIVE['plugins.own.obj.b'] })").await;
        assert_eq!(live, "[1,8]", "a restored to its default, b kept the new value");
        let s = state(&h, "obj").await;
        assert_eq!(
            (s["committed"].clone(), s["running"].clone(), s["changed"].clone()),
            (json!("<absent>"), json!({ "a": 1, "b": 2 }), json!([]))
        );
        exec(&h, "TX.SCRIPT['plugins.own.obj.b'] = nil TX.SCRIPT2['plugins.second.opt'] = nil").await;
        tail_ok(&h, 15).await;
        h.shutdown().await;
    })
    .await
}

/// two plugins applied, a third option refused, one restore fails: the other plugin is fully
/// restored and only the failing leaf is listed.
#[tokio::test]
async fn one_failed_restore_does_not_stop_the_others() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1, str = 's' })").await;
        exec(&h, "RESET() TX.SCRIPT['plugins.own.num'] = 'fail_on_restore' TX.SCRIPT2['plugins.second.opt'] = 'fail'")
            .await;
        let e =
            write(&h, "OPT_SET({ ['plugins.own.num'] = 5, ['plugins.own.str'] = 't', ['plugins.second.opt'] = 'z' })")
                .await;
        assert!(e.ends_with("owner state unconfirmed: plugins.own.num"), "{e}");
        assert_eq!(eval::<String>(&h, "return TX.LIVE['plugins.own.str']").await, "s");
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil TX.SCRIPT2['plugins.second.opt'] = nil").await;
        tail_ok(&h, 16).await;
        h.shutdown().await;
    })
    .await
}

// ---------- H: config_clear ----------

/// Value domain (`flag` defaults to true and starts false): a runtime write then a clear
/// brings back the startup value and source; only changed leaves are announced.
#[tokio::test]
async fn clear_brings_back_the_startup_value() {
    local(async {
        for (start, runtime) in [("false", "true"), ("0", "7"), ("''", "'x'"), ("nil", "'x'")] {
            let key = if start == "false" {
                "flag"
            } else if start == "0" {
                "num"
            } else {
                "str"
            };
            let init =
                if start == "nil" { String::new() } else { format!("mare.plugin('own', {{ {key} = {start} }})") };
            let (_env, h) = host(&init).await;
            let before = state(&h, key).await;
            assert_eq!(write(&h, &format!("mare.plugin('own', {{ {key} = {runtime} }})")).await, "ok");
            exec(&h, "RESET()").await;
            assert_eq!(write(&h, &format!("mare.config_clear('plugins.own.{key}')")).await, "ok");
            let s = state(&h, key).await;
            assert_eq!(
                (s["committed"].clone(), s["source"].clone()),
                (before["committed"].clone(), before["source"].clone()),
                "{start}"
            );
            assert_eq!(s["running"], before["running"], "{start}");
            assert_eq!(names(&s["changed"]), vec![format!("plugins.own.{key}")], "{start}");
            tail_ok(&h, 17).await;
            h.shutdown().await;
        }
    })
    .await
}

/// When the owner refuses a clear, the runtime value stays everywhere; a subsequent clear succeeds.
#[tokio::test]
async fn a_refused_clear_keeps_the_runtime_value() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        assert_eq!(write(&h, "mare.plugin('own', { num = 5 })").await, "ok");
        exec(&h, "RESET() TX.SCRIPT['plugins.own.num'] = 'fail'").await;
        assert!(write(&h, "mare.config_clear('plugins.own.num')").await.contains("own refuses"));
        let s = state(&h, "num").await;
        assert_eq!(
            (s["committed"].clone(), s["running"].clone(), s["source"].clone()),
            (json!(5), json!(5), json!("runtime"))
        );
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil").await;
        assert_eq!(write(&h, "mare.config_clear('plugins.own.num')").await, "ok");
        assert_eq!(state(&h, "num").await["running"], json!(1));
        tail_ok(&h, 18).await;
        h.shutdown().await;
    })
    .await
}

/// H3: a clear after a refused write, and a clear of what was never written, do nothing.
#[tokio::test]
async fn a_clear_with_nothing_to_clear_does_nothing() {
    local(async {
        let (_env, h) = host("mare.plugin('own', { num = 1 })").await;
        exec(&h, "TX.SCRIPT['plugins.own.num'] = 'fail'").await;
        assert!(write(&h, "mare.plugin('own', { num = 5 })").await.contains("own refuses"));
        exec(&h, "TX.SCRIPT['plugins.own.num'] = nil RESET()").await;
        assert_eq!(write(&h, "mare.config_clear('plugins.own.num')").await, "ok");
        assert_eq!(write(&h, "mare.config_clear('plugins.own.str')").await, "ok");
        let s = state(&h, "num").await;
        assert_eq!(
            (s["applied"].clone(), s["changed"].clone(), s["running"].clone()),
            (json!([]), json!([]), json!(1))
        );
        tail_ok(&h, 19).await;
        h.shutdown().await;
    })
    .await
}

// ---------- G: redaction ----------

const SRV_INIT: &str = "mare.plugin('own', { srv = { gh = { tier = 'a', env = { TOKEN = 's3cret' } } } })";

/// a dotted write offers and announces exactly the leaf it names — no untouched
/// sibling (credential) becomes an event.
#[tokio::test]
async fn only_the_written_leaf_is_applied_and_announced() {
    local(async {
        let (_env, h) = host(SRV_INIT).await;
        exec(&h, "RESET()").await;
        assert_eq!(write(&h, "OPT_SET({ ['plugins.own.srv.gh.tier'] = 'b' })").await, "ok");
        let s = state(&h, "srv").await;
        let applied: Vec<String> =
            s["applied"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(applied, vec!["plugins.own.srv.gh.tier"]);
        assert_eq!(names(&s["changed"]), vec!["plugins.own.srv.gh.tier"]);
        h.shutdown().await;
    })
    .await
}

/// a leaf declared sensitive (a name no keyword rule catches) is announced redacted and
/// flagged; the plugin runs with the real value.
#[tokio::test]
async fn a_declared_sensitive_leaf_is_announced_redacted() {
    local(async {
        let (_env, h) = host(SRV_INIT).await;
        exec(&h, "RESET()").await;
        assert_eq!(write(&h, "OPT_SET({ ['plugins.own.srv.gh.pin'] = '1234' })").await, "ok");
        let s = state(&h, "srv").await;
        assert_eq!(
            s["changed"],
            json!([{ "name": "plugins.own.srv.gh.pin", "value": "[REDACTED]", "redacted": true }])
        );
        assert_eq!(eval::<String>(&h, "return TX.CFG_OWN.srv.gh.pin").await, "1234");
        h.shutdown().await;
    })
    .await
}

/// after a dotted write next to it, the plugin still runs with the credential
/// itself (equality, so a lost credential fails), and reading it back shows it redacted.
#[tokio::test]
async fn the_plugin_keeps_the_credential_and_readers_see_it_redacted() {
    local(async {
        let (_env, h) = host(SRV_INIT).await;
        assert_eq!(write(&h, "OPT_SET({ ['plugins.own.srv.gh.tier'] = 'b' })").await, "ok");
        assert_eq!(eval::<String>(&h, "return TX.CFG_OWN.srv.gh.env.TOKEN").await, "s3cret");
        let shown: String = eval(
            &h,
            "local d = mare.config_describe({ 'plugins.own.srv.gh.env.TOKEN' }) return d['plugins.own.srv.gh.env.TOKEN'].effective",
        )
        .await;
        assert_eq!(shown, "[REDACTED]");
        h.shutdown().await;
    })
    .await
}

// ---------- F: re-entrancy ----------

/// A Cordis parallel listener spawned inside option/apply inherits the transaction.
#[tokio::test]
async fn option_apply_spawn_inherits_transaction_and_rejects_nested_write() {
    local(async {
        let (_env, h) = host("").await;
        exec(&h, "TX.SCRIPT['plugins.own.num'] = 'spawn_write'; RESET()").await;
        let outer =
            tokio::time::timeout(std::time::Duration::from_secs(4), write(&h, "mare.plugin('own', { num = 5 })")).await;
        assert_eq!(outer.expect("spawned write deadlocked the enclosing option/apply"), "ok");
        let nested: String = eval(&h, "return TX.NESTED_RESULT").await;
        assert!(nested.contains("config write inside option/apply"), "{nested}");
        tail_ok(&h, 41).await;
        h.shutdown().await;
    })
    .await
}

/// F4: a public `option/changed` listener writes another option and waits for it: both writes
/// succeed, announced outer first, inner second (the notifier never waits on a listener).
#[tokio::test]
async fn a_write_from_option_changed_completes() {
    local(async {
        let (_env, h) = host("").await;
        exec(
            &h,
            "C.cordis:on('option/changed', function(e)
               if e.name == 'plugins.own.num' then INNER = W(function() mare.plugin('own', { str = 'inner' }) end) end
             end)
             RESET()",
        )
        .await;
        let outer =
            tokio::time::timeout(std::time::Duration::from_secs(10), write(&h, "mare.plugin('own', { num = 5 })"))
                .await;
        assert_eq!(outer.expect("no deadlock between the notifier and a listener's write"), "ok");
        assert_eq!(eval::<String>(&h, "return INNER").await, "ok");
        let s = state(&h, "str").await;
        assert_eq!(names(&s["changed"]), vec!["plugins.own.num", "plugins.own.str"]);
        h.shutdown().await;
    })
    .await
}

/// lazy#46 #1: a map key containing dots (`srv["api.example.com"]`) still finds the `sensitive`
/// declaration below it: the leaf is redacted for readers and in the announcement.
#[tokio::test]
async fn g_a_dotted_map_key_keeps_its_sensitive_field_redacted() {
    local(async {
        let (_env, h) = host("").await;
        exec(&h, "RESET()").await;
        assert_eq!(write(&h, "mare.plugin('own', { srv = { ['api.example.com'] = { pin = '5678' } } })").await, "ok");
        let s = state(&h, "srv").await;
        let out = s["changed"].to_string();
        assert!(!out.contains("5678") && out.contains("[REDACTED]"), "{out}");
        let shown: String =
            eval(&h, "return mare.json.encode((mare.config_redact('plugins.own.srv', mare.config.plugins.own.srv)))")
                .await;
        assert!(!shown.contains("5678"), "{shown}");
        assert_eq!(eval::<String>(&h, "return TX.CFG_OWN.srv['api.example.com'].pin").await, "5678");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn h_runtime_write_triggering_reload_failure_reports_error_without_rolling_back_committed_config() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/coldplug.lua"),
            r#"return { name = "coldplug",
                config_schema = {
                    mode = { type = "string", default = "ok", desc = "mode" },
                },
                apply = function(_, cfg)
                    if cfg.mode == "boom" then error("rejected in apply") end
                    mare._coldplug_mode = cfg.mode
                end }"#,
        )
        .unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert_eq!(eval::<String>(&h, "return mare._coldplug_mode").await, "ok");
        let err: String = eval(
            &h,
            "local ok, e = pcall(mare.plugin, 'coldplug', { mode = 'boom' }); return ok and 'ok' or tostring(e)",
        )
        .await;
        assert!(err.contains("rejected in apply"), "reload failure must be returned to the caller: {err}");
        assert_eq!(eval::<String>(&h, "return mare._coldplug_mode").await, "ok");
        assert_eq!(
            h.kernel.config().pointer("/plugins/coldplug/mode"),
            Some(&json!("boom")),
            "committed config is kept as-is without post-reload rollback: {}",
            h.kernel.config()
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn i_same_value_retry_after_busy_reload_schedules_reload_and_applies() {
    local(async {
        let (_env, h) = host("").await;
        exec(
            &h,
            r#"C.cordis:on('request/build', function(req, next)
                mare._busy_started = true
                for _ = 1, 12000 do if mare._busy_release then break end mare.task.sleep(5) end
                return next()
            end)"#,
        )
        .await;
        let (prompt, ()) = tokio::join!(h.prompt("hello", json!({})), async {
            wait_until(WAIT_SECS, "busy_started", || async {
                eval::<bool>(&h, "return mare._busy_started == true").await
            })
            .await;
            let err: String = eval(
                &h,
                "local ok, e = pcall(mare.plugin, 'approvals', { review_all = true }); return ok and 'ok' or tostring(e)",
            )
            .await;
            assert!(err.contains("reload") || err.contains("model request"), "busy reload must fail: {err}");
            exec(&h, "mare._busy_release = true").await;
        });
        prompt.unwrap();
        let retry: String = eval(
            &h,
            "local ok, e = pcall(mare.plugin, 'approvals', { review_all = true }); return ok and 'ok' or tostring(e)",
        )
        .await;
        assert_eq!(retry, "ok", "same-value retry must succeed: {retry}");
        global(&h, "AP", h.service_table("approvals").unwrap());
        assert!(eval::<bool>(&h, "return AP:review_required()").await, "approvals must apply review_all = true");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn j_required_plugin_disabled_after_failed_reload_recovers_when_fixed() {
    local(async {
        let (_env, h) = host("").await;
        let err: String = eval(
            &h,
            "local ok, e = pcall(mare.plugin, 'approvals', { review_timeout_ms = 0 }); return ok and 'ok' or tostring(e)",
        )
        .await;
        assert_ne!(err, "ok", "review_timeout_ms = 0 must fail reload: {err}");
        assert!(!h.kernel.plugin_active("approvals"), "failed reload leaves approvals inactive");
        let fixed: String = eval(
            &h,
            "local ok, e = pcall(mare.plugin, 'approvals', { review_timeout_ms = 30000 }); return ok and 'ok' or tostring(e)",
        )
        .await;
        assert_eq!(fixed, "ok", "fixing review_timeout_ms must reload and recover approvals: {fixed}");
        assert!(h.kernel.plugin_active("approvals"), "approvals must be active again after fix");
        h.shutdown().await;
    })
    .await
}

/// Repeated writes to the same config key keep the last write, and cancelling an unrelated
/// task token while a config transaction is in progress does not abort the started transaction.
#[tokio::test]
async fn k_same_key_last_write_wins_and_cancel_does_not_abort_started_txn() {
    local(async {
        let (_env, h) = host("").await;
        let (final_val, txn_ok, txn_val): (i64, bool, i64) = eval(
            &h,
            r#"
            mare.setup { plugins = { approvals = { review_timeout_ms = 11000 } } }
            mare.setup { plugins = { approvals = { review_timeout_ms = 22000 } } }
            local final_val = mare.config.plugins.approvals.review_timeout_ms

            local tok = mare.task.token()
            local txn_ok, txn_err = pcall(function()
                tok:cancel()
                mare.plugin("approvals", { review_timeout_ms = 33000 })
            end)
            assert(txn_ok, tostring(txn_err))
            return final_val, txn_ok, mare.config.plugins.approvals.review_timeout_ms
            "#,
        )
        .await;
        assert_eq!(final_val, 22000, "last write to the same key must win");
        assert!(txn_ok && txn_val == 33000, "started config transaction must commit cleanly");
        h.shutdown().await;
    })
    .await
}
