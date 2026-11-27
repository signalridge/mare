//! `mare_lib.follow`: an optional service read on each call. The getter reads the active provider
//! (`ctx:get`), so a read in the follower's own `apply` already sees a provider that is up. A
//! follower fiber would only start one job hop later (cordis-lua fiber.lua `_reload`, deviations.md
//! D1); that lost the session's approval policy on a targeted reload. Ports cordis
//! packages/core/tests/reflect.spec.ts:40-44 (`get` gives the provided value, nothing otherwise)
//! and replace.spec.ts:120-141 (`ctx.get` in `apply` reads the current, then the replacing value).

use super::{host, write_plugin};
use crate::common::*;

/// Each apply provides `fx` with the next id.
const PROVIDER: &str = r#"return { name = "fx-provider", provide = { "fx" }, apply = function(ctx)
  mare._fx_n = (mare._fx_n or 0) + 1
  local id = mare._fx_n
  local Fx = require("cordis").class("Fx", mare.Service)
  function Fx:constructor(c)
    Fx.super.constructor(self, c, "fx", { contract = { methods = {
      id = { doc = "id", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "integer" } },
    } } })
  end
  function Fx:id() return id end
  ctx:plugin(Fx)
end }"#;

/// Reads `fx` in its own apply (0 = not seen) and keeps each apply's getter.
const FOLLOWER: &str = r#"return { name = "fx-follower", apply = function(ctx)
  local get = require("mare_lib").follow(ctx, "fx")
  local now = get()
  mare._fx_seen = mare._fx_seen or {}
  mare._fx_seen[#mare._fx_seen + 1] = now and now:id() or 0
  mare._fx_getters = mare._fx_getters or {}
  mare._fx_getters[#mare._fx_getters + 1] = get
end }"#;

/// A targeted reload of the follower, the provider up: the read in its apply sees it.
#[tokio::test]
async fn a_follower_reads_a_provider_that_is_up_in_its_own_apply() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "fx-provider", PROVIDER);
        write_plugin(&env, "fx-follower", FOLLOWER);
        let h = host(&env).await;
        let seen: Vec<i64> = eval(
            &h,
            r#"local kernel = C.cordis:get("kernel")
               kernel:reload("fx-follower")
               kernel:reload_barrier()
               return mare._fx_seen"#,
        )
        .await;
        h.shutdown().await;
        // the startup read is not asserted: nothing orders the two plugins' start
        assert_eq!(seen.last(), Some(&1), "the read in the reloaded apply: {seen:?}");
    })
    .await
}

/// The getter reads the provider that is current (a reloaded provider: the new one), returns nil
/// when the provider goes away, and a getter handed out by an apply that has since been unloaded
/// reads nothing.
#[tokio::test]
async fn a_follower_reads_the_current_provider_and_ends_with_its_plugin() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "fx-provider", PROVIDER);
        write_plugin(&env, "fx-follower", FOLLOWER);
        let h = host(&env).await;
        let (replaced, stale_gone, current): (i64, bool, i64) = eval(
            &h,
            r#"local kernel = C.cordis:get("kernel")
               kernel:reload("fx-provider")
               kernel:reload_barrier()
               local first = mare._fx_getters[1]
               local replaced = first() and first():id() or 0
               kernel:reload("fx-follower")
               kernel:reload_barrier()
               local latest = mare._fx_getters[#mare._fx_getters]()
               return replaced, first() == nil, latest and latest:id() or 0"#,
        )
        .await;
        assert_eq!((replaced, stale_gone, current), (2, true, 2));
        unload_plugin(&h.kernel, "fx-provider").await;
        let after_provider_gone: bool = eval(&h, "return mare._fx_getters[#mare._fx_getters]() == nil").await;
        h.shutdown().await;
        assert!(after_provider_gone, "getter must return nil once the provider goes away");
    })
    .await
}

/// Product-level regression (inventory item 2): during a targeted reload of `changes`, `configure()`
/// runs synchronously inside `changes`'s `apply` and reads the active `session` through
/// `mare_lib.follow` at once, pointing `mare.snapshot.configure` at `<session>.snapshots` instead
/// of falling back to the per-run dir first. Runs in `changes`'s `apply` with no `inject` fiber
/// ordering dependency.
#[tokio::test]
async fn changes_reload_configures_session_snapshots_in_apply() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["changes"]).faux(serde_json::json!([])).build().await;
        expose(&h);
        let (expected_dir, configured_dirs): (String, Vec<String>) = eval(
            &h,
            r#"local info = C.cordis:get("session"):info()
               local expected = (info.path or info.planned) .. ".snapshots"
               local dirs = {}
               local orig = mare.snapshot.configure
               mare.snapshot.configure = function(opts)
                 if type(opts) == "table" and opts.dir then
                   dirs[#dirs + 1] = opts.dir
                 end
                 return orig(opts)
               end
               local ok, err = pcall(function()
                 local kernel = C.cordis:get("kernel")
                 kernel:reload("changes")
                 kernel:reload_barrier()
               end)
               mare.snapshot.configure = orig
               assert(ok, tostring(err))
               return expected, dirs"#,
        )
        .await;
        h.shutdown().await;
        assert!(!configured_dirs.is_empty(), "changes reload never called mare.snapshot.configure");
        assert_eq!(
            configured_dirs[0], expected_dir,
            "first mare.snapshot.configure inside changes apply used {configured_dirs:?}, expected {expected_dir}"
        );
    })
    .await
}
