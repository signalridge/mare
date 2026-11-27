use super::*;

/// A provider's exports are derived once per contract and shared by the next provider of the same
/// contract (every reload re-provides every service): a provider of the same name under another
/// contract gets its own derivation, so its schema, not the earlier one, checks the calls.
#[tokio::test]
async fn a_new_contract_under_a_name_is_derived_again() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["derive-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(
            &host,
            r#"
            CONTRACT = function(kind)
                return {contract = {methods = {
                    f = {doc = 'f', expose = 'both', order = {'x'}, result = {type = 'string'},
                        params = {type = 'object', properties = {x = {type = kind}}, required = {'x'},
                            additionalProperties = false}}}}}
            end
            PROVIDE('derive-probe', {f = function(_, _, x) return type(x) end}, CONTRACT('string'))
            assert(RC:call_service('derive-probe', 'f', {x = 'a'}) == 'string')
            assert(not pcall(RC.call_service, RC, 'derive-probe', 'f', {x = 1}))
            -- the next start of the provider plugin declares the other contract
            mare._specs['derive-probe'].opts = CONTRACT('number')
        "#,
        )
        .await;
        let before = host.kernel.derived_exports_id("derive-probe");
        // one full reload: the old derivation is still cached (the reload hold keeps it) when the
        // provider comes back under the new contract
        host.reload_all().await.unwrap();
        let after = host.kernel.derived_exports_id("derive-probe");
        exec(
            &host,
            r#"
            assert(RC:call_service('derive-probe', 'f', {x = 1}) == 'number', 'the earlier contract still checks the calls')
            assert(not pcall(RC.call_service, RC, 'derive-probe', 'f', {x = 'a'}))
        "#,
        )
        .await;
        host.shutdown().await;
        assert!(before.is_some() && after.is_some() && before != after, "the new contract got its own derivation");
    })
    .await
}

/// The derived exports follow the providers: a name nobody provides any more drops its entry, so a
/// client registering and unregistering services leaves the cache where it was, while a full reload
/// (which withdraws and provides every service again) keeps the entries it reuses.
#[tokio::test]
async fn derived_exports_go_with_the_last_provider_of_a_name_and_stay_through_a_reload() {
    local(async {
        let env = TestEnv::new().await;
        let names: Vec<String> =
            std::iter::once("churn-same".to_string()).chain((1..=10).map(|i| format!("churn-{i}"))).collect();
        svc_plugins(&env, &names.iter().map(String::as_str).collect::<Vec<_>>());
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let base = host.kernel.derived_exports_len();
        assert!(base > 0, "the built-in services' exports are derived");
        exec(
            &host,
            r#"
            local function provide(name)
                return PROVIDE(name, {f = function() return 'ok' end}, {contract = {methods = {
                    f = {doc = 'f', expose = 'both', result = {type = 'string'}}}}})
            end
            -- the disposer starts the service fiber's dispose: wait for it (the withdraw is its end)
            local function gone(remove)
                local done = remove()
                if done and done.await then done:await() end
            end
            for _ = 1, 10 do gone(provide('churn-same')) end
            for i = 1, 10 do gone(provide('churn-' .. i)) end
            -- the probe plugins provide nothing on their next start (the reload below)
            mare._specs = {}
        "#,
        )
        .await;
        let after_churn = host.kernel.derived_exports_len();
        let llm = host.kernel.derived_exports_id("llm");
        host.reload_all().await.unwrap();
        let after_reload = host.kernel.derived_exports_len();
        let llm_after = host.kernel.derived_exports_id("llm");
        host.shutdown().await;
        assert_eq!(after_churn, base, "names without a provider kept their derived exports");
        assert_eq!(after_reload, base, "a reload dropped or added entries");
        assert!(llm.is_some() && llm == llm_after, "a reload derived llm's exports again instead of reusing them");
    })
    .await
}

/// Review a-reload2 (should-fix): an admission refused after the exports were derived (the
/// implementation lacks a required method) keeps no derivation: the cache is written when the
/// provider registers.
#[tokio::test]
async fn a_refused_admission_keeps_no_derived_exports() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        // outside any reload (a reload's end sweeps names left without a provider): a plugin whose
        // service lacks a method its contract requires, refused at startup
        std::fs::write(
            dir.join("refused-probe.lua"),
            r#"return { name = "refused-probe", provide = { "refused-probe" }, apply = function(ctx)
  local S = require("cordis").class("RefusedProbe", mare.Service)
  function S:constructor(c) S.super.constructor(self, c, "refused-probe", { contract = { methods = {
    run = { doc = "r", expose = "both", result = { type = "string" } },
    other = { doc = "o", expose = "both", result = { type = "string" } } } } }) end
  function S:other() return "x" end
  ctx:plugin(S)
end }"#,
        )
        .unwrap();
        let host = mare_host::Host::new(env.host().user_plugins().faux(json!([])).opts).await.unwrap();
        let problems = host.kernel.problems().join("\n");
        let kept = host.kernel.derived_exports_id("refused-probe");
        host.shutdown().await;
        assert!(problems.contains("run"), "the admission is refused for the missing method: {problems}");
        assert_eq!(kept, None, "a refused admission left its derived exports");
    })
    .await
}

/// A name a reload leaves without a provider (the plugin's new definition no longer provides it)
/// drops its derived exports when the reload ends.
#[tokio::test]
async fn a_name_a_reload_leaves_without_a_provider_drops_its_derived_exports() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        let plugin = dir.join("sweep-probe.lua");
        std::fs::write(
            &plugin,
            r#"return { name = "sweep-probe", apply = function(ctx)
  local S = require("cordis").class("SweepSvc", mare.Service)
  function S:constructor(c) S.super.constructor(self, c, "sweep-svc", { contract = { methods = {
    f = { doc = "f", expose = "both", result = { type = "string" } } } } }) end
  function S:f() return "ok" end
  ctx:plugin(S)
end }"#,
        )
        .unwrap();
        let host = env.host().user_plugins().faux(json!([])).build().await;
        let before = host.kernel.derived_exports_id("sweep-svc");
        std::fs::write(&plugin, r#"return { name = "sweep-probe", apply = function(ctx) end }"#).unwrap();
        host.reload_all().await.unwrap();
        let after = host.kernel.derived_exports_id("sweep-svc");
        host.shutdown().await;
        assert!(before.is_some(), "the plugin's service was derived");
        assert_eq!(after, None, "a name without a provider after the reload kept its derived exports");
    })
    .await
}

#[tokio::test]
async fn service_exports_validate_arguments_results_and_exact_registration_disposal() {
    local(async {
        let env=TestEnv::new().await;
        svc_plugins(&env, &["bad-export", "bad-result", "export-probe", "lua-only", "shape-probe", "unmapped"]);
        let host=env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(&host,r#"
            CALLS=0
            local function echo_contract()
                return {methods={echo={doc='echo',expose='both',
                    params={type='object',properties={text={type='string'}},required={'text'},additionalProperties=false},
                    order={'text'},result={type='string'}}}}
            end
            local function provide(value)
                return PROVIDE('export-probe',{echo=function(_,caller,text)
                    -- the handler gets the provider's own fiber as its calling ctx (the p-export-probe plugin's service fiber)
                    local who=require('mare_lib').caller_name(caller)
                    assert(who=='P_export-probe', tostring(who));CALLS=CALLS+1;return value..text
                end},{contract=echo_contract()})
            end
            REMOVE_OLD=provide('old:')
            local old_handle = (mare._handles or {})[mare._specs['export-probe']]
            assert(old_handle and rawequal(REMOVE_OLD, old_handle.dispose), 'the remover is not the native registration disposer')
            assert(RC:call_service('export-probe','echo',{text='one'})=='old:one')
            assert(not pcall(RC.call_service,RC,'export-probe','echo',{text=42}))
            assert(not pcall(RC.call_service,RC,'export-probe','echo',{text='one',unknown=true}))
            assert(not pcall(RC.call_service,RC,'export-probe','_hidden',{}))
            local cancelled=mare.task.token();cancelled:cancel()
            assert(not pcall(RC.call_service,RC,'export-probe','echo',{text='cancelled'},{cancel=cancelled}))
            assert(CALLS==1,'invalid or cancelled input reached the handler')
            -- the remover is the registration's effect disposer:
            -- idempotent — calling it again is no error and does nothing more — and the provider is
            -- gone after the first call. Native disposal is asynchronous (Cordis yields a tick
            -- before tearing down): await the disposer (its `await`) before asserting
            REMOVE_OLD(); REMOVE_OLD(); REMOVE_OLD:await()
            assert(not pcall(RC.call_service,RC,'export-probe','echo',{text='gone'}),'a removed provider still answers')
            REMOVE_NEW=provide('new:')
            REMOVE_OLD(); REMOVE_OLD:await()
            assert(RC:call_service('export-probe','echo',{text='two'})=='new:two','an old remover touched the new provider')
            assert(not pcall(PROVIDE,'bad-export',{echo=function()end},{contract={methods={
                echo={doc='bad',expose='both',params={type='object',unknownKeyword=true},result={}}}}}))
            assert(C.cordis:get('bad-export')==nil,'failed declaration published a provider')
            -- every parameter maps to one Lua argument; an unmapped one is refused at declaration
            assert(not pcall(PROVIDE,'unmapped',{f=function()end},{contract={methods={f={doc='f',expose='both',
                params={type='object',properties={a={type='string'}},additionalProperties=false},result={}}}}}))
            -- lua-only methods are not reachable as JSON
            local lua_only=PROVIDE('lua-only',{f=function()return 1 end},{contract={methods={f={doc='f',result={type='integer'}}}}})
            assert(not pcall(RC.call_service,RC,'lua-only','f',{}))
            assert(lua_only())
            local remove=PROVIDE('bad-result',{get=function()return 'wrong' end},{contract={methods={
                get={doc='get',expose='both',result={type='integer'}}}}})
            assert(not pcall(RC.call_service,RC,'bad-result','get',{}))
            -- trailing opts, nil+message as an error, nothing as {}, registrations as ids
            local released=false
            local remove_shape=PROVIDE('shape-probe',{
                opts=function(_,_,a,o)return {a=a,b=o and o.b or 'none'} end,
                fail=function()return nil,'expected failure' end,
                void=function()end,
                reg=function()return function()released=true end end,
            },{contract={methods={
                opts={doc='o',expose='both',params={type='object',properties={a={type='string'},b={type='string'}},required={'a'},additionalProperties=false},
                    order={'a','...'},result={type='object'}},
                fail={doc='f',expose='both',result={type='object'}},
                void={doc='v',expose='both',result={type='object',additionalProperties=false}},
                reg={doc='r',expose='both',registration=true},
            }}})
            local o=RC:call_service('shape-probe','opts',{a='x',b='y'})
            assert(o.a=='x' and o.b=='y')
            assert(RC:call_service('shape-probe','opts',{a='x'}).b=='none')
            -- nil + a plain message is an error; undeclared, it is `internal` (in-process callers
            -- also see the original; RPC callers do not, see errors_rpc.py)
            local ok,err=pcall(RC.call_service,RC,'shape-probe','fail',{})
            -- (ctx:call_service raises the error as a value printing `code: message`)
            -- (native: the guard wraps the message twice, `internal: shape-probe.fail failed: shape-probe.fail failed: expected failure`)
            assert(not ok and tostring(err):find('internal: shape-probe.fail failed:',1,true) and tostring(err):find('expected failure',1,true),tostring(err))
            assert(next(RC:call_service('shape-probe','void',{}))==nil)
            local reg=RC:call_service('shape-probe','reg',{})
            assert(type(reg.registration_id)=='string' and not released)
            assert(remove_shape())
            assert(remove() and REMOVE_NEW())
        "#).await;
        host.shutdown().await;
    }).await
}

#[tokio::test]
/// Withdrawing a provider closes its in-flight calls (their `<close>` runs once, in the caller's
/// execution context) and nothing falls back: with one slot per name the name has no
/// provider until one provides it again.
async fn withdrawing_a_service_closes_in_flight_calls_and_nothing_falls_back() {
    local(async {
        let env=TestEnv::new().await;
        svc_plugins(&env, &["withdraw-probe"]);let host=env.host().user_plugins().faux(json!([])).build().await;expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(&host,r#"
            OPTIONS={contract={methods={get={doc='get',expose='both',result={type='integer'}}}}}
            CLOSED=0;STARTED=false
            REMOVE=PROVIDE('withdraw-probe',{get=function()
                local guard <close> = setmetatable({},{__close=function()
                    assert(mare.exec.current().tool_call_id=='cancel-context','close lost its caller context')
                    CLOSED=CLOSED+1
                end})
                STARTED=true;mare.task.sleep(10000);return 2
            end},OPTIONS)
        "#).await;
        let function=host.lua().load("return mare.exec.run({tool='service-close',tool_call_id='cancel-context'},function()return RC:call_service('withdraw-probe','get',{})end)").into_function().unwrap();
        let call=function.call_async::<mlua::Value>(());tokio::pin!(call);
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            tokio::select! {
                result=&mut call=>panic!("service call completed before withdrawal: {result:?}"),
                _=async {while !eval::<bool>(&host,"return STARTED").await {tokio::task::yield_now().await;}}=>{}
            }
        }).await.unwrap();
        exec(&host,"assert(REMOVE())").await;
        assert!(call.await.is_err());
        exec(&host,r#"
            assert(CLOSED==1)
            assert(not pcall(RC.call_service,RC,'withdraw-probe','get',{}),'something fell back after the withdrawal')
            local again=PROVIDE('withdraw-probe',{get=function()return 1 end},OPTIONS)
            assert(RC:call_service('withdraw-probe','get',{})==1);assert(again())
        "#).await;
        host.shutdown().await;
    }).await
}

#[tokio::test]
async fn exported_handlers_keep_the_callers_execution_context() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["context-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(
            &host,
            r#"
            local remove=PROVIDE('context-probe',{get=function()
                local current=mare.exec.current();return current and current.tool_call_id or 'missing'
            end},{contract={methods={get={doc='get',expose='both',result={type='string'}}}}})
            local result=mare.exec.run({tool='context-test',tool_call_id='bound-call'},function()
                return RC:call_service('context-probe','get',{})
            end)
            assert(result=='bound-call','generic service invocation dropped its execution context')
            assert(remove())
        "#,
        )
        .await;
        host.shutdown().await;
    })
    .await
}

/// Lua callers meet the same contract as JSON callers; explicit nil is never success; JSON
/// registrations are released by the kernel with their owner and a failed release is retryable.
#[tokio::test]
async fn lua_proxies_registrations_and_results_follow_the_contract() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["guard-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let (owner, owner_id) = json_caller_as_owner(&host);
        global(&host, "OWNER", owner);
        exec(
            &host,
            r#"
            RELEASED, FAIL_ONCE, FAILS = 0, true, 2
            REMOVE = PROVIDE('guard-probe', {
                pick = function(_, _, name, opts) return {name = name, save = opts and opts.save or false} end,
                none = function() return nil end,
                reg = function() return function()
                    if FAILS > 0 then FAILS = FAILS - 1; FAIL_ONCE = false; error('release failed') end
                    RELEASED = RELEASED + 1
                end end,
            }, {contract = {methods = {
                pick = {doc = 'p', expose = 'both', order = {'name', '...'}, result = {type = 'object'},
                    params = {type = 'object', properties = {name = {type = 'string'}, save = {type = 'boolean'}},
                        required = {'name'}, additionalProperties = false}},
                none = {doc = 'n', expose = 'both', result = {type = 'object'}},
                reg = {doc = 'g', expose = 'both', registration = true},
            }}})
            local probe = C.cordis:get('guard-probe')
            assert(probe:pick('dark', {save = true}).save == true)
            -- the Lua proxy validates like RPC: wrong option types never reach the provider
            assert(not pcall(probe.pick, probe, 'dark', {save = 'yes'}))
            assert(not pcall(probe.pick, probe, 12))
            assert(not pcall(probe.pick, probe, 'dark', {name = 'other'}), 'opts may not override a positional')
            -- explicit nil is an error on both paths, not {}
            local _, lua_nil = pcall(probe.none, probe)
            local _, json_nil = pcall(RC.call_service, RC, 'guard-probe', 'none', {})
            assert(lua_nil.code == 'invalid_result' and json_nil.code == 'invalid_result', tostring(lua_nil) .. ' / ' .. tostring(json_nil))
            -- a JSON caller's registration is held by the kernel for its owner
            HELD = OWNER:call_service('guard-probe', 'reg', {}).registration_id
        "#,
        )
        .await;
        // another owner record as the JSON caller (release_registration is the kernel's per-owner
        // release, the RPC path) cannot release OWNER's registration while it is live: a lookup
        // across owners would find it here (a-step3 #2)
        let (caller, caller_id) = json_caller_as_owner(&host);
        global(&host, "RC", caller);
        let id: String = eval(&host, "return HELD").await;
        assert!(!host.kernel.release_registration(caller_id, &id).await.unwrap(), "foreign id is not released");
        assert!(eval::<bool>(&host, "return FAIL_ONCE").await, "the foreign release ran OWNER's disposer");
        host.kernel.dispose_ctx(owner_id).await;
        assert!(!eval::<bool>(&host, "return FAIL_ONCE").await, "disposing the owner did not call its disposer");
        // failed at disposal and at the immediate retry: kept pending, not dropped
        assert_eq!(eval::<i64>(&host, "return RELEASED").await, 0, "both attempts failed");
        // the failed release is kept and retried: the resource is released in the end
        assert_eq!(host.kernel.retry_releases().await, 0, "nothing left pending after the retry");
        assert_eq!(eval::<i64>(&host, "return RELEASED").await, 1, "the registration was finally released");
        exec(&host, "RELEASED = 0").await;
        // a held registration survives a failed release and can be released again
        exec(&host, "FAILS = 1; HELD2 = RC:call_service('guard-probe', 'reg', {}).registration_id").await;
        let held: String = eval(&host, "return HELD2").await;
        assert!(host.kernel.release_registration(caller_id, &held).await.is_err(), "the disposer failed");
        assert!(host.kernel.release_registration(caller_id, &held).await.unwrap(), "the handle stayed for a retry");
        assert!(!host.kernel.release_registration(caller_id, &held).await.unwrap(), "released once");
        assert_eq!(eval::<i64>(&host, "return RELEASED").await, 1);
        exec(&host, "assert(REMOVE())").await;
        host.shutdown().await;
    })
    .await
}

/// Public setter and in-process SPI restoration share the provider's constraints, while
/// connection-level `CallerAccess` gates remote `llm.set_model` exports.
#[tokio::test]
async fn spi_setters_need_the_public_operation_and_share_its_constraints() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        global(&host, "L", host.service_table("llm").unwrap());
        exec(&host, "P = C.cordis:get('llm')").await;
        let caller = host.kernel.open_owner("peer-list-only", "user").unwrap();
        host.kernel.set_caller_access(
            caller,
            Some(mare_kernel::CallerAccess {
                is_peer: true,
                allows: std::rc::Rc::new(|_, method, access| access == "peer" && method == "list_models"),
                allows_session_path: std::rc::Rc::new(|| false),
            }),
        );
        let export_err = host
            .kernel
            .call_service_export(caller, None, "llm", "set_model", json!({"model": "faux/echo2"}), Default::default())
            .await
            .unwrap_err();
        assert_eq!(export_err.code(), "denied", "remote set_model remains gated for peers: {export_err}");
        let restore_ok: bool = eval(
            &host,
            r#"
            return pcall(P.restore_selection, P, { model = 'faux/echo2' })
        "#,
        )
        .await;
        assert!(restore_ok, "in-process restore_selection is trusted");
        // one admission constraint (`llm/model-changing`), seen by both set_model and restore_selection
        exec(
            &host,
            r#"
            local off = C.cordis:on("llm/model-changing", function(ev)
                if ev.model and ev.model.ref == 'faux/echo2' then
                    ev.refuse('model refused by the provider')
                end
            end)
            local ok, err = pcall(P.set_model, P, 'faux/echo2', {save = false})
            assert(not ok and tostring(err):find('refused by the provider', 1, true), tostring(err))
            local ok2 = pcall(P.restore_selection, P, { model = 'faux/echo2' })
            assert(not ok2)
            assert(P:set_model('faux/echo', {save = false}).ref == 'faux/echo', 'the picker passes save')
            off()
        "#,
        )
        .await;
        host.shutdown().await;
    })
    .await
}

/// Concurrent releases of one registration run its disposer once.
#[tokio::test]
async fn concurrent_releases_of_one_registration_run_its_disposer_once() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["slow-release"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        // the registration is made and released by an owner record: release_registration is the
        // kernel's per-owner release (the path RPC callers take); on the tree `C` is the transition
        // layer's table, whose registrations the contracts host plugin holds
        let (caller, caller_id) = json_caller_as_owner(&host);
        global(&host, "RC", caller);
        exec(
            &host,
            r#"
            RUNS = 0
            PROVIDE('slow-release', {reg = function() return function()
                RUNS = RUNS + 1; mare.task.sleep(50)
            end end}, {contract = {methods = {reg = {doc = 'r', expose = 'both', registration = true}}}})
            HELD = RC:call_service('slow-release', 'reg', {}).registration_id
        "#,
        )
        .await;
        let id: String = eval(&host, "return HELD").await;
        let (first, second) = tokio::join!(
            host.kernel.release_registration(caller_id, &id),
            host.kernel.release_registration(caller_id, &id)
        );
        assert_eq!([first.unwrap(), second.unwrap()].iter().filter(|released| **released).count(), 1);
        assert_eq!(eval::<i64>(&host, "return RUNS").await, 1);
        host.shutdown().await;
    })
    .await
}

/// Restoring a session for a caller without llm.set_model / llm.set_effort is refused before
/// the session is published: nothing changes, and no default fallback runs in its place.
#[tokio::test]
async fn session_restoration_needs_the_model_operations_and_changes_nothing_when_refused() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let target = env.write_session(
            "restore-denied.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"model", "model":"faux/echo2", "effort":"high", "at_entry_id":"m1"}),
            ],
        );
        let target_id = {
            let line = std::fs::read_to_string(&target).unwrap().lines().next().unwrap().to_owned();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["session_id"].as_str().unwrap().to_owned()
        };
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        global(&host, "L", host.service_table("llm").unwrap());
        global(&host, "TARGET", target.to_string_lossy().to_string());
        exec(&host, "mare.store.index_append({ path = TARGET })").await;
        exec(&host, "BEFORE_ID, BEFORE_MODEL, BEFORE_LEAF = S:info(nil).id, L:current().ref, S:info(nil).leaf").await;
        // a peer granted session.load (and session.update/session.list), but neither llm.set_model nor llm.set_effort
        let caller = host.kernel.open_owner("restore-denied-peer", "user").unwrap();
        let reader: mlua::Value = host
            .lua()
            .load("local id = ...; local rpc = C.cordis:get('rpc'); rpc:connect(id); return rpc:context(id)")
            .call(caller)
            .unwrap();
        host.kernel.set_caller_access(
            caller,
            Some(mare_kernel::CallerAccess {
                is_peer: true,
                allows: std::rc::Rc::new(|service, method, access| {
                    (access == "peer" || (service == "session" && method == "update")) && !method.starts_with("set_")
                }),
                allows_session_path: std::rc::Rc::new(|| false),
            }),
        );
        // Peer cannot load by path even with session.update granted.
        let path_err = host
            .kernel
            .call_service_export(caller, Some(reader.clone()), "session", "load", json!({"path": target}), Default::default())
            .await
            .unwrap_err();
        assert_eq!(path_err.code(), "invalid_arguments", "{path_err}");
        // Loading by session_id reaches settings authorization and is denied without llm.set_model / llm.set_effort.
        let err = host
            .kernel
            .call_service_export(
                caller,
                Some(reader.clone()),
                "session",
                "load",
                json!({"session_id": target_id}),
                Default::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "denied", "{err}");
        // tree navigation restores settings too: calling set_leaf through `reader:get("session")` exercises
        // `fork.lua:12-25` (`D.authorize_settings(caller)`) before touching the leaf
        global(&host, "READER", reader);
        let (moved, why): (bool, String) = eval(
            &host,
            "local ok, err = pcall(function() return READER:get('session'):set_leaf(nil) end); return ok, type(err) == 'table' and tostring(err.code) or tostring(err)",
        )
        .await;
        assert!(!moved && why == "denied", "{why}");
        exec(&host, "assert(S:info(nil).leaf == BEFORE_LEAF, 'the leaf moved')").await;
        exec(
            &host,
            "assert(S:info(nil).id == BEFORE_ID, 'the session was published'); assert(L:current().ref == BEFORE_MODEL)",
        )
        .await;
        host.shutdown().await;
    })
    .await
}
