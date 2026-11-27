use super::*;

/// One validation point: the public set_model, session restoration and tree navigation all select
/// a model through llm's select_model, so a model it refuses (an unknown provider: `nope/x`) is
/// refused on all three with the same code (`llm.unknown_model`) and the current model stays.
/// (Coordinator ruling: the built-in llm is the subject; the old wrapped-replacement version went
/// with the provider stack, and the replacement admission rule is
/// a_replacement_overriding_only_the_public_setter_is_refused.)
#[tokio::test]
async fn one_validation_point_refuses_a_model_on_the_setter_restoration_and_tree_navigation() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let target = env.write_session(
            "unknown.jsonl",
            &[msg("m1", None, "user", "TARGET"), json!({"type":"model", "model":"nope/x", "at_entry_id":"m1"})],
        );
        // a tree: the m2 branch records the refused model, the m3 branch does not
        let tree = env.write_session(
            "tree.jsonl",
            &[
                msg("m1", None, "user", "ROOT"),
                msg("m2", Some("m1"), "user", "TWO"),
                json!({"type":"model", "model":"nope/x", "at_entry_id":"m2"}),
                msg("m3", Some("m1"), "user", "THREE"),
            ],
        );
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        global(&host, "TARGET", target.to_string_lossy().to_string());
        global(&host, "TREE", tree.to_string_lossy().to_string());
        exec(
            &host,
            r#"
            -- restoration and navigation report a refused model as a warning (store.lua warn)
            NOTES = {}
            mare.ui.connected = function() return true end
            mare.ui.notify = function(t) NOTES[#NOTES + 1] = tostring(t) end
            local function refused_notes()
                local n = 0
                for _, t in ipairs(NOTES) do
                    if t:find('session model not restored: llm.unknown_model', 1, true) then n = n + 1 end
                end
                return n
            end
            local P = C.cordis:get('llm')
            assert(P:set_model('faux/echo', { save = false }).ref == 'faux/echo')
            -- the public setter
            local ok, err = pcall(P.set_model, P, 'nope/x', { save = false })
            assert(not ok and type(err) == 'table' and err.code == 'llm.unknown_model', tostring(err))
            assert(P:current().ref == 'faux/echo', 'the setter changed the model')
            -- session restoration
            assert(S:load({ path = TARGET }))
            assert(refused_notes() == 1, 'restoration did not refuse through select_model: ' .. table.concat(NOTES, ' ; '))
            assert(P:current().ref == 'faux/echo', 'restoration went around the check')
            -- tree navigation: the leaf moves, the refused model does not come with it
            assert(S:load({ path = TREE }))
            local ok3, err3 = pcall(S.set_leaf, S, 'm2')
            assert(ok3, 'set_leaf failed: ' .. tostring(err3))
            assert(refused_notes() == 2, 'tree navigation did not refuse through select_model: ' .. table.concat(NOTES, ' ; '))
            assert(P:current().ref == 'faux/echo', 'tree navigation went around the check')
            assert(S:leaf() == 'm2', 'the leaf did not reach the target branch: ' .. tostring(S:leaf()))
        "#,
        )
        .await;
        host.shutdown().await;
    })
    .await
}

/// For work done for a peer, a method a full contract does not declare is refused, and the
/// refusal happens before the provider is read (its `__index` runs no code).
#[tokio::test]
async fn a_peer_cannot_reach_undeclared_methods_and_is_refused_before_the_provider_runs() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["undeclared-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(&host, r#"
            PROVIDE('undeclared-probe', {get = function() return {} end},
                {contract = {methods = {get = {doc = 'g', expose = 'both', access = 'peer', result = {type = 'object'}}}}})
            PROBE = C.cordis:get('undeclared-probe')
            LLM = C.cordis:get('llm')
        "#).await;
        let peer = host.kernel.open_owner("undeclared-peer", "user").unwrap();
        host.kernel.set_caller_access(
            peer,
            Some(mare_kernel::CallerAccess {
                is_peer: true,
                allows: std::rc::Rc::new(|_, _, access| access == "peer"),
                allows_session_path: std::rc::Rc::new(|| false),
            }),
        );
        let secret_err = host
            .kernel
            .call_service_export(peer, None, "undeclared-probe", "secret", json!({}), Default::default())
            .await
            .unwrap_err();
        assert!(matches!(secret_err, mare_kernel::ServiceCallError::MethodUnavailable), "{secret_err}");
        let get_ok = host
            .kernel
            .call_service_export(peer, None, "undeclared-probe", "get", json!({}), Default::default())
            .await;
        assert!(get_ok.is_ok(), "a declared, granted method still works: {get_ok:?}");
        let logout_err = host
            .kernel
            .call_service_export(peer, None, "llm", "logout", json!({"provider": "openai"}), Default::default())
            .await
            .unwrap_err();
        assert_eq!(logout_err.code(), "denied", "llm.logout is primary-only: refused for a peer: {logout_err}");
        // the contract is the whole surface: host code is refused an undeclared method too
        // (REV-BASE p1), and still reaches a declared one
        exec(
            &host,
            r#"local sok, secret = pcall(function() return PROBE.secret end)
               assert(sok and secret == nil, 'an undeclared key reads as nil')
               local nok, numeric = pcall(function() return PROBE[1] end)
               local bok, boolean = pcall(function() return PROBE[true] end)
               assert(not (nok and numeric ~= nil) and not (bok and boolean ~= nil), 'a non-string key reached the provider')
               assert(not pcall(function() return PROBE:secret() end), 'an undeclared method was callable from host code')
               assert(not pcall(RC.call_service, RC, 'undeclared-probe', 'secret', {}), 'an undeclared method was callable as JSON')"#,
        )
        .await;
        host.shutdown().await;
    })
    .await
}

/// A provider's stream error raised inside mare_lib.with_response (how providers read a
/// response) keeps its own `code` and `kind`: context_length_exceeded stays a context overflow
/// (compaction runs), it does not become `internal` or `fatal`.
#[tokio::test]
async fn llm_stream_keeps_a_providers_native_error_code_and_kind() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let kind: String = eval(
            &host,
            r#"
            local llm = C.cordis:get('llm')
            llm:register_provider({ provider = 'native-code', models = { { model_id = 'm', name = 'M' } },
                is_configured = function() return true end,
                stream = function()
                    return require('mare_lib').with_response(llm, {}, function()
                        error({ code = 'context_length_exceeded', kind = 'context_overflow', status = 400,
                            message = 'prompt is too long' }, 0)
                    end)
                end })
            local m = llm:stream({ model = { ref = 'native-code/m', provider = 'native-code', id = 'm' }, messages = {} },
                function() end)
            return tostring(m.error_kind)
            "#,
        )
        .await;
        assert_eq!(kind, "context_overflow");
        host.shutdown().await;
    })
    .await
}

/// An empty Lua table is an array where the contract's schema says array, an object where it
/// says object (a marked empty array included), and stays as before where it says nothing, for
/// results and arguments alike, Lua and JSON callers alike. Scenario after Neovim, whose typed
/// conversion takes `{}` where a Dict is expected (test/functional/lua/ui_event_spec.lua:329
/// `nvim_set_hl(0, 'Pmenu', {})`, buffer_updates_spec.lua:1065; src/nvim/lua/converter.c:886-889).
#[tokio::test]
async fn empty_tables_take_the_shape_the_contract_declares() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["shape", "variant"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(
            &host,
            r#"
            local list = { type = 'object', properties = { data = { type = 'array', items = { type = 'string' } },
                meta = { type = 'object', additionalProperties = { type = 'string' } }, any = {} },
                required = { 'data', 'meta' }, additionalProperties = false }
            local pair = { type = 'object', properties = {
                a = { type = 'object', properties = { item = { type = 'array' } }, required = { 'item' } },
                b = { type = 'object', properties = { item = { type = 'object' } }, required = { 'item' } } },
                required = { 'a', 'b' }, additionalProperties = false }
            REMOVE = PROVIDE('shape', {
                list = function() return { data = {}, meta = mare.json.array(), any = {} } end,
                -- one empty table in two positions that need different shapes
                shared = function() local x = {} return { data = x, meta = x } end,
                -- a shared non-empty parent whose empty field needs a different shape in each place
                nested = function() local p = { item = {} } return { a = p, b = p } end,
                alias_in = function(_, _, a, opts)
                    RECEIVED = mare.json.encode({ a = a, b = opts.b })
                    return 'ok'
                end,
                nested_in = function(_, _, a, opts)
                    RECEIVED = mare.json.encode({ a = a, b = opts.b })
                    return 'ok'
                end,
                -- the provider records what it received, before any result shaping
                tags = function(_, _, tags, opts)
                    RECEIVED = mare.json.encode({ tags = tags, labels = opts.labels })
                    return { data = {}, meta = {} }
                end,
            }, { contract = { methods = {
                list = { doc = 'l', expose = 'both', result = list },
                shared = { doc = 's', expose = 'both', result = list },
                nested = { doc = 'n', expose = 'both', result = pair },
                alias_in = { doc = 'a', expose = 'both', order = { 'a', '...' }, result = { type = 'string' },
                    params = { type = 'object', properties = {
                        a = { type = 'object', properties = { b = { type = 'array' } }, required = { 'b' } },
                        b = { type = 'object' } }, required = { 'a', 'b' }, additionalProperties = false } },
                nested_in = { doc = 'n', expose = 'both', order = { 'a', '...' }, result = { type = 'string' },
                    params = { type = 'object', properties = { a = pair.properties.a, b = pair.properties.b },
                        required = { 'a', 'b' }, additionalProperties = false } },
                tags = { doc = 't', expose = 'both', order = { 'tags', '...' }, result = list,
                    params = { type = 'object', properties = { tags = { type = 'array', items = { type = 'string' } },
                        labels = { type = 'object', additionalProperties = { type = 'string' } } },
                        required = { 'tags', 'labels' }, additionalProperties = false } },
            } } })
            P = C.cordis:get('shape')
            "#,
        )
        .await;
        // a Lua caller: results and arguments validate with plain empty tables
        // what the proxy delivers is shaped too: the caller's result and the provider's arguments
        let delivered: String = eval(&host, "return mare.json.encode(P:list())").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&delivered).unwrap(),
            json!({ "data": [], "meta": {}, "any": {} })
        );
        let passed: String = eval(&host, "P:tags({}, { labels = mare.json.array() }); return RECEIVED").await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&passed).unwrap(), json!({ "tags": [], "labels": {} }));
        // a shared empty table gets a correct shape in each position, results and arguments alike
        let shared: String = eval(&host, "return mare.json.encode(P:shared())").await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&shared).unwrap(), json!({ "data": [], "meta": {} }));
        let aliased: String = eval(&host, "local x = {} P:tags(x, { labels = x }); return RECEIVED").await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&aliased).unwrap(), json!({ "tags": [], "labels": {} }));
        // a shared non-empty parent: each place gets its own shape
        let nested: String = eval(&host, "return mare.json.encode(P:nested())").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&nested).unwrap(),
            json!({ "a": { "item": [] }, "b": { "item": {} } })
        );
        let nested_in: String = eval(&host, "local p = { item = {} } P:nested_in(p, { b = p }); return RECEIVED").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&nested_in).unwrap(),
            json!({ "a": { "item": [] }, "b": { "item": {} } })
        );
        // the positional argument and the opts table are one table needing different shapes:
        // nothing can be delivered right, so the call is refused before the provider runs (b27a)
        let same: String = eval(
            &host,
            "RECEIVED = nil local p = { b = {} } \
             local ok = pcall(P.alias_in, P, p, p) return tostring(ok) .. '|' .. tostring(RECEIVED)",
        )
        .await;
        assert_eq!(same, "false|nil");
        // a refused call leaves the caller's tables as they were (lazy#44 #7: the positional
        // argument's empty table had been marked as an array before the conflict was found)
        let untouched: String = eval(
            &host,
            "local p = { b = {} } local before = mare.json.encode(p) pcall(P.alias_in, P, p, p) \
             return before .. '|' .. mare.json.encode(p)",
        )
        .await;
        assert_eq!(untouched, r#"{"b":{}}|{"b":{}}"#);
        // a JSON caller: arrays where the schema says array, objects where it says object; the
        // untyped field keeps the old encoding (an empty table is an object)
        let listed: String = eval(&host, "return mare.json.encode(RC:call_service('shape', 'list', {}))").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&listed).unwrap(),
            json!({ "data": [], "meta": {}, "any": {} })
        );
        let tagged: String =
            eval(&host, "return mare.json.encode(RC:call_service('shape', 'tags', { tags = {}, labels = {} }))").await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&tagged).unwrap()["data"], json!([]));
        exec(&host, "assert(REMOVE())").await;
        // oneOf with two object branches told apart by `kind` (const): the value's own fields pick
        // the branch, so x = {} is an array for kind a and an object for kind b
        exec(
            &host,
            r#"
            local variant = { oneOf = {
                { type = 'object', properties = { kind = { const = 'a' }, x = { type = 'array' } },
                    required = { 'kind', 'x' }, additionalProperties = false },
                { type = 'object', properties = { kind = { const = 'b' }, x = { type = 'object' } },
                    required = { 'kind', 'x' }, additionalProperties = false },
            } }
            REMOVE = PROVIDE('variant', {
                get = function(_, _, kind) return { kind = kind, x = {} } end,
            }, { contract = { methods = {
                get = { doc = 'g', expose = 'both', order = { 'kind' }, result = variant,
                    params = { type = 'object', properties = { kind = { type = 'string' } }, required = { 'kind' },
                        additionalProperties = false } },
            } } })
            V = C.cordis:get('variant')
            "#,
        )
        .await;
        exec(&host, "assert(V:get('a').kind == 'a' and V:get('b').kind == 'b')").await;
        for (kind, x) in [("a", json!([])), ("b", json!({}))] {
            let encoded: String = eval(
                &host,
                &format!("return mare.json.encode(RC:call_service('variant', 'get', {{ kind = '{kind}' }}))"),
            )
            .await;
            assert_eq!(serde_json::from_str::<serde_json::Value>(&encoded).unwrap(), json!({ "kind": kind, "x": x }));
        }
        exec(&host, "assert(REMOVE())").await;
        host.shutdown().await;
    })
    .await
}

/// llm.providers is a peer-readable operation: it gives each provider as data, never the
/// provider's own functions (stream, login, credentials), which work done for a peer must not
/// reach.
#[tokio::test]
async fn llm_providers_are_data_without_the_providers_functions() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let functions: i64 = eval(
            &host,
            r#"local n = 0
               for _, p in ipairs(C.cordis:get('llm'):providers()) do
                 for _, v in pairs(p) do if type(v) == 'function' then n = n + 1 end end
               end
               return n"#,
        )
        .await;
        assert_eq!(functions, 0);
        let faux: String =
            eval(&host, "for _, p in ipairs(C.cordis:get('llm'):providers()) do if p.name == 'faux' then return tostring(p.configured) end end").await;
        assert_eq!(faux, "true");
        host.shutdown().await;
    })
    .await
}

/// A call bound to the session holds it across a session provider's replacement: a reload gives
/// a new provider instance, and that instance must not switch sessions while the host-side
/// `Host::bind_session_call()` guard lasts.
#[tokio::test]
async fn a_session_hold_outlives_the_session_providers_replacement() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &[]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let bound_call = host.bind_session_call().unwrap();
        host.reload_all().await.unwrap();
        let switched: String = eval(
            &host,
            "local ok, e = pcall(function() return C.cordis:get('session'):create() end) return tostring(ok) .. '|' .. tostring(type(e) == 'table' and e.code or e)",
        )
        .await;
        assert!(switched.starts_with("false|busy"), "{switched}");
        drop(bound_call);
        let after: bool = eval(&host, "return (pcall(function() return C.cordis:get('session'):create() end))").await;
        assert!(after, "the switch goes through once the bound call guard is released");
        host.shutdown().await;
    })
    .await
}

/// Releasing an owner's registrations can be cancelled midway (a shutdown deadline): every
/// handle stays queued until released, so a later retry still releases it (
/// handles taken out of the queue were dropped with the cancelled future).
#[tokio::test]
async fn a_cancelled_release_keeps_its_handles_for_the_retry() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["slow-release"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let (owner, owner_id) = json_caller_as_owner(&host);
        global(&host, "OWNER", owner);
        exec(
            &host,
            r#"RELEASED = 0
               PROVIDE('slow-release', { reg = function()
                   return function() mare.task.sleep(200) RELEASED = RELEASED + 1 end
               end }, { contract = { methods = { reg = { doc = 'r', expose = 'both', registration = true } } } })
               OWNER:call_service('slow-release', 'reg', {})
               OWNER:call_service('slow-release', 'reg', {})"#,
        )
        .await;
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(50), host.kernel.dispose_ctx(owner_id)).await;
        assert!(cancelled.is_err(), "the release was cancelled midway");
        assert_eq!(host.kernel.retry_releases().await, 0, "nothing left pending after the retry");
        assert_eq!(eval::<i64>(&host, "return RELEASED").await, 2, "both registrations were released");
        host.shutdown().await;
    })
    .await
}

/// A `registration = true` JSON export whose result is not delivered (the call is cancelled,
/// caller disposed, or provider gone before the result is adopted) releases the returned disposer
/// (function or EffectDisposer callable table) at once so no registration leaks .
#[tokio::test]
async fn a_cancelled_registration_export_releases_its_disposer_at_once() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["cancel-reg"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let (_, owner_id) = json_caller_as_owner(&host);
        let cancel = tokio_util::sync::CancellationToken::new();
        let fire = cancel.clone();
        let fire_fn = host
            .lua()
            .create_function(move |_, ()| {
                fire.cancel();
                Ok(())
            })
            .unwrap();
        global(&host, "FIRE_CANCEL", fire_fn);
        exec(
            &host,
            r#"RELEASED = 0
               DISPOSABLE = setmetatable({}, { __call = function() RELEASED = RELEASED + 1 end })
               PROVIDE('cancel-reg', {
                   reg = function()
                       FIRE_CANCEL()
                       return DISPOSABLE
                   end,
               }, { contract = { methods = {
                   reg = { doc = 'r', expose = 'both', registration = true },
               } } })"#,
        )
        .await;
        let err =
            host.kernel.call_service_export(owner_id, None, "cancel-reg", "reg", json!({}), cancel).await.unwrap_err();
        assert_eq!(err.code(), "cancelled");
        assert_eq!(
            eval::<i64>(&host, "return RELEASED").await,
            1,
            "callable-table disposer must run immediately on request cancel"
        );
        assert_eq!(host.kernel.retry_releases().await, 0);
        host.shutdown().await;
    })
    .await
}

/// A contract declaring deleted `authorizes` or declaring `access` on a Lua-only method is
/// refused at parse/admission time (access unification: non-remote methods have no access
/// tier, and `authorizes` is removed).
#[tokio::test]
async fn contract_rejects_authorizes_and_non_remote_access() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["bad-authorizes", "bad-lua-access"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let (bad_auth, bad_lua): (String, String) = eval(
            &host,
            r#"local auth_c = { contract = { methods = {
                   set_mode = { doc = 'setter', expose = 'both', params = { type = 'object', properties = {}, additionalProperties = false }, order = {}, result = { type = 'object' } },
                   select_mode = { doc = 'performs set_mode', authorizes = 'set_mode' },
               } } }
               local lua_c = { contract = { methods = {
                   internal = { doc = 'lua-only', access = 'peer' },
               } } }
               local ok1, e1 = pcall(PROVIDE, 'bad-authorizes', { set_mode = function() return {} end, select_mode = function() return {} end }, auth_c)
               local ok2, e2 = pcall(PROVIDE, 'bad-lua-access', { internal = function() return {} end }, lua_c)
               return (ok1 and 'accepted' or tostring(e1)), (ok2 and 'accepted' or tostring(e2))"#,
        )
        .await;
        host.shutdown().await;
        assert!(bad_auth.contains("authorizes") || bad_auth.contains("unknown key"), "authorizes must fail: {bad_auth}");
        assert!(bad_lua.contains("non-remote") && bad_lua.contains("access"), "non-remote access must fail: {bad_lua}");
    })
    .await
}

/// Admission walks a provider's class chain to mare.Service; a chain deeper than its limit is
/// refused, never admitted unchecked (a-contracts review should-fix 3).
#[tokio::test]
async fn a_class_chain_deeper_than_the_limit_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["deep-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let refused: String = eval(
            &host,
            r#"local parent = mare.Service
               for i = 1, 20 do parent = require('cordis').class('Deep' .. i, parent) end
               local ok, e = pcall(PROVIDE, 'deep-probe', { f = function() return {} end },
                 { contract = { methods = { f = { doc = 'f', expose = 'both', result = { type = 'object' } } } } }, parent)
               return ok and 'accepted' or tostring(e)"#,
        )
        .await;
        host.shutdown().await;
        assert!(refused.contains("deeper than"), "{refused}");
    })
    .await
}

/// A class chain that comes back to a table it already passed is refused, not admitted with the
/// layers seen before the repeat checked (review a-contracts-q 3).
#[tokio::test]
async fn a_class_chain_that_repeats_a_table_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["loop-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let refused: String = eval(
            &host,
            r#"local cordis = require('cordis')
               local A = cordis.class('LoopA', mare.Service)
               function A:constructor(c, ...) mare.Service.constructor(self, c, ...) end
               local B = cordis.class('LoopB', A)
               rawset(A, 'super', B)
               local ok, e = pcall(PROVIDE, 'loop-probe', { f = function() return {} end },
                 { contract = { methods = { f = { doc = 'f', expose = 'both', result = { type = 'object' } } } } }, B)
               return ok and 'accepted' or tostring(e)"#,
        )
        .await;
        host.shutdown().await;
        assert!(refused.contains("repeats a table"), "{refused}");
    })
    .await
}

/// A provider whose method is inherited from a parent class is one provider: the methods Lua
/// callers can reach are the methods JSON callers see listed and can call (one way of reading it).
#[tokio::test]
async fn inherited_class_methods_are_exposed_the_same_to_lua_and_json() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["meta-probe"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        exec(
            &host,
            r#"
            local Base = require('cordis').class('MetaBase', mare.Service)
            function Base:extra() return {from = 'extra'} end
            REMOVE = PROVIDE('meta-probe', {get = function() return {from = 'base'} end}, {contract = {methods = {
                get = {doc = 'g', expose = 'both', result = {type = 'object'}},
                extra = {doc = 'e', expose = 'both', optional = true, result = {type = 'object'}},
                absent = {doc = 'a', expose = 'both', optional = true, result = {type = 'object'}},
            }}}, Base)
            assert(C.cordis:get('meta-probe'):extra().from == 'extra')
            assert(RC:call_service('meta-probe', 'extra', {}).from == 'extra', 'an inherited method is callable as JSON')
            assert(not pcall(RC.call_service, RC, 'meta-probe', 'absent', {}), 'an unimplemented optional method is not')
        "#,
        )
        .await;
        let listed = host.kernel.service_exports();
        let methods =
            listed.iter().find(|s| s["name"] == "meta-probe").unwrap()["methods"].as_object().unwrap().clone();
        assert!(
            methods.contains_key("extra") && methods.contains_key("get") && !methods.contains_key("absent"),
            "{methods:?}"
        );
        exec(&host, "assert(REMOVE())").await;
        host.shutdown().await;
    })
    .await
}

/// Whether a provider has an optional method is read once, at registration, and both Lua and
/// JSON callers go by that: a method added to the implementation afterwards is absent for both,
/// one present at registration is there for both.
#[tokio::test]
async fn optional_methods_exist_for_lua_and_json_alike() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["opt-svc", "opt-svc2"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        json_caller(&host);
        let seen: String = eval(
            &host,
            r#"local impl = { base = function() return "b" end }
               local spec = { doc = 'x', expose = 'both', result = { type = 'string' } }
               PROVIDE('opt-svc', impl, { contract = { methods = { base = spec,
                   extra = { doc = 'x', expose = 'both', result = { type = 'string' }, optional = true } } } })
               impl.extra = function() return "late" end
               local json = pcall(RC.call_service, RC, 'opt-svc', 'extra', {})
               local p = C.cordis:get('opt-svc')
               return tostring(json) .. '|' .. tostring(p.extra)"#,
        )
        .await;
        assert_eq!(seen, "false|nil");
        // the other direction: a method present at registration is there for every caller
        let present: String = eval(
            &host,
            r#"local impl = { base = function() return "b" end, extra = function() return "e" end }
               local spec = { doc = 'x', expose = 'both', result = { type = 'string' } }
               PROVIDE('opt-svc2', impl, { contract = { methods = { base = spec,
                   extra = { doc = 'x', expose = 'both', result = { type = 'string' }, optional = true } } } })
               impl.extra = nil
               local p = C.cordis:get('opt-svc2')
               return type(p.extra) .. '|' .. tostring(RC:call_service('opt-svc2', 'extra', {}))"#,
        )
        .await;
        assert_eq!(present, "function|e");
        host.shutdown().await;
    })
    .await
}

/// A registration's reference belongs to it: once withdrawn it stays refused, even when the same
/// implementation is provided again.
#[tokio::test]
async fn a_withdrawn_registrations_reference_stays_refused_when_the_impl_is_provided_again() {
    local(async {
        let env = TestEnv::new().await;
        svc_plugins(&env, &["view-c"]);
        let host = env.host().user_plugins().faux(json!([])).build().await;
        expose(&host);
        svc(&host).await;
        let revived: String = eval(
            &host,
            r#"local impl = { f = function() return "f" end }
               local base = { doc = 'x', expose = 'both', result = { type = 'string' } }
               local remove = PROVIDE('view-c', impl, { contract = { methods = { f = base } } })
               local old = C.cordis:get('view-c')
               assert(old:f() == "f")
               remove()
               PROVIDE('view-c', impl, { contract = { methods = { f = base } } })
               local ok = pcall(old.f, old)
               return tostring(ok) .. '|' .. tostring(C.cordis:get('view-c'):f())"#,
        )
        .await;
        assert_eq!(revived, "false|f");
        host.shutdown().await;
    })
    .await
}

/// A plugin written against the public contracts checks a service's methods the way they were
/// always documented: `type(ctx.agent.activity) == "function"` (mare-plugins session-search). A
/// built-in published as a mare.Service keeps that, read through a native ctx (cordis-lua D2: a traced method that is a Lua function reads as one),
/// and calling the methods works.
#[tokio::test]
async fn a_plugin_checking_public_service_methods_sees_functions() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("native-contract-probe.lua"),
            r#"return { name = "native-contract-probe", inject = { "agent", "spill", "session", "llm" },
              apply = function(ctx)
                assert(type(ctx.agent.activity) == "function", "agent.activity is not a function")
                assert(type(ctx.spill.save) == "function", "spill.save is not a function")
                assert(type(ctx.session.info) == "function", "session.info is not a function")
                assert(type(ctx.llm.current) == "function", "llm.current is not a function")
                assert(rawequal(ctx.agent.activity, ctx.agent.activity), "a method read twice is not the same")
                assert(type(ctx.agent:activity()) == "table", "agent:activity() did not answer")
                assert(type(ctx.session:info().session_id) == "string", "session:info() did not answer")
                mare.g.native_contract_probe = true
              end }"#,
        )
        .unwrap();
        let h = env.host().builtins(&["spill"]).faux(json!([])).user_plugins().build().await;
        let problems = h.kernel.problems();
        let native: bool = eval(&h, "return mare.g.native_contract_probe == true").await;
        h.shutdown().await;
        assert!(native, "the native plugin did not start: {problems:?}");
    })
    .await
}
