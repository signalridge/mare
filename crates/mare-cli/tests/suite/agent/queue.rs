//! Durable queue, frontend queue acknowledgements, inbox commits, and abort queue retention.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn durable_queue_atomic_consume_survives_restart_and_append_failure() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        exec(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local gating = true
            local rm_vote
            rm_vote = C.cordis:get('policy'):voter('request/ready', function()
                if gating then
                    gating = false
                    held_tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'seed queue gate' }
                end
            end)
            local seed_turn = A:prompt('seed')
            held_rx:recv()
            local qres = A:prompt('queued', {when_busy='queue', images={{type='image',data='AQID',mime_type='image/png'}}})
            assert(qres.status == 'queued', tostring(qres.status))
            A:cancel({ turn_id = seed_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(seed_turn.turn_id)
            rm_vote()
            local q = A:list_queue().data
            QUEUE_ID = q[1].input_id
            assert(type(QUEUE_ID)=='string')
            -- The actual loop now commits consumption with its user message atomically.
            -- Before that boundary, the accepted complete entry remains pending.
            assert(#q == 1)
            for _,message in ipairs(A:list_messages().data) do assert(message._input_id~=QUEUE_ID) end
        "#).await;
        h.shutdown().await;
        let h = env.host().faux(json!([{ "text": "done" }])).resume().build().await;
        expose(&h);
        let (count, text, mime): (i64, String, String) = eval(&h, "local q=A:list_queue().data; return #q,q[1].text,q[1].images[1].mime").await;
        assert_eq!((count,text,mime),(1,"queued".into(),"image/png".into()));
        exec(&h, r#"
            FAIL_QUEUE_APPEND=true
            C.cordis:get('policy'):voter('session/commit-ready',function(e) if FAIL_QUEUE_APPEND and #e.messages>0 then return {decision='deny',reason='injected queue append failure'} end end)
        "#).await;
        let result = h.prompt("trigger", json!({})).await.unwrap();
        assert!(result["error"]["message"].is_string(), "{result}");
        h.shutdown().await;
        let h = env.host().faux(json!([{ "text": "done" }])).resume().build().await;
        expose(&h);
        let count: i64 = eval(&h,"return #A:list_queue().data").await;
        assert_eq!(count,1,"failed message append must retain durable input");
        let result = h.prompt("retry",json!({})).await.unwrap();
        assert!(result["error"].is_null(),"{result}");
        h.shutdown().await;
        let h = env.host().faux(json!([])).resume().build().await;
        expose(&h);
        let (queued, delivered): (i64,i64) = eval(&h,"local n=0; for _,m in ipairs(A:list_messages().data) do if m._input_id then n=n+1 end end; return #A:list_queue().data,n").await;
        assert_eq!((queued,delivered),(0,1),"committed input must not reappear after restart");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn durable_queue_rejects_unacknowledged_enqueue() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (ok,error,count,rej_busy): (bool,String,i64,bool) = eval(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'stop gate' }
                end
            end)
            local started = A:prompt('start')
            held_rx:recv()
            local rej_ok, rejected = pcall(A.prompt, A, 'busy reject', { when_busy = 'reject' })
            local rm_voter = C.cordis:get('policy'):voter('session/commit-ready',function() return {decision='deny',reason='injected queue failure'} end)
            local ok,err=pcall(function() A:prompt('must reject', { when_busy = 'queue' }) end)
            rm_voter()
            local qcount = #A:list_queue().data
            rel_tx:send(true)
            A:wait(started.turn_id)
            return ok,tostring(err),qcount,(not rej_ok and type(rejected) == 'table' and rejected.code == 'busy')
        "#).await;
        assert!(!ok && error.contains("injected queue failure"),"{error}");
        assert_eq!(count,0);
        assert!(rej_busy);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn frontend_queue_acknowledges_errors_and_returns_cancelled_inputs() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        exec(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:on('turn/started', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                end
            end)
            HELD_TURN = A:prompt('hold')
            HELD_RELEASE = rel_tx
            held_rx:recv()
        "#).await;
        h.follow_up_with("recover me", json!({"images":[{"type":"image","mime":"image/png","data":"bytes"}]}))
            .await
            .unwrap();
        let reply = h.abort_with_inputs().await.unwrap();
        assert_eq!(reply["removed"][0]["text"], "recover me");
        assert_eq!(reply["removed"][0]["images"][0]["data"], "bytes");
        exec(&h, "HELD_RELEASE:send(true); A:wait(HELD_TURN.turn_id)").await;
        exec(&h, "C.cordis:get('policy'):voter('session/commit-ready', function() return {decision='deny',reason='frontend disk failure'} end)").await;
        exec(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:on('turn/started', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                end
            end)
            HELD_TURN2 = A:prompt('hold2')
            HELD_RELEASE2 = rel_tx
            held_rx:recv()
        "#).await;
        let err = h.follow_up("must reject").await.unwrap_err();
        assert!(err.to_string().contains("frontend disk failure"), "{err}");
        assert!(h.steer("must reject too").await.is_err());
        let pending: i64 = eval(&h, "return #A:list_queue().data").await;
        assert_eq!(pending, 0);
        exec(&h, "HELD_RELEASE2:send(true); A:wait(HELD_TURN2.turn_id)").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn frontend_queue_rejects_missing_or_invalid_acknowledgements() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("bad_agent.lua"),
            r#"
            return {
                name = "bad-agent",
                provide = { "agent" },
                apply = function(ctx)
                    local svc = {}
                    for _, m in ipairs({
                        "prompt", "steer", "cancel", "continue", "wait", "get", "list_queue",
                        "update_queued", "delete_queued", "list_messages", "messages",
                        "prepare_commit", "activity", "register_activity", "set_messages",
                        "adopt_session", "session_revision", "check_session_revision", "set_tool_states",
                        "set_pending_request", "pending_request", "append", "persist_error", "notify",
                        "interrupted", "set_session_id", "set_queued", "baseline_cut",
                    }) do
                        svc[m] = function() return {} end
                    end
                    svc.get = function() return { busy = false, queued = 0 } end
                    svc.activity = function() return { version = 1, busy = false, queued = 0, turn_cancelled = false } end
                    svc.list_queue = function() return { data = mare.json.array() } end
                    svc.list_messages = function() return { data = mare.json.array() } end
                    svc.interrupted = function() return false end
                    svc.persist_error = function() return nil end
                    svc.prompt = function() return mare._bad_reply end
                    svc.cancel = function() return mare._bad_reply end
                    require("mare_lib").service(ctx, "agent", svc)
                end,
            }
            "#,
        )
        .unwrap();
        let h = env.host().user_plugins().config(json!({ "plugins": { "agent": { "enabled": false } } })).build().await;
        for reply in [
            "{}",
            "false",
            "{ status = 'cancelled' }",
            "{ status = 'cancelled', removed = { { id = 'q1' } } }",
            "{ status = 'cancelled', removed = mare.json.array({ { text = 'missing id' } }) }",
        ] {
            exec(&h, &format!("mare._bad_reply = {reply}")).await;
            assert!(h.follow_up("must reject").await.is_err(), "accepted {reply}");
            assert!(h.abort_with_inputs().await.is_err(), "accepted {reply}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn durable_queue_recovery_reads_claims_before_compaction_tail() {
    local(async {
        let env = TestEnv::new().await;
        let mut committed = msg("delivered",None,"user","already delivered");
        committed["input_id"] = json!("done-id");
        env.write_session("queue-before-compaction.jsonl",&[
            committed,
            json!({"type":"custom","kind":"agent/queue","provenance":"session-host","data":{"steer":[],"follow_up":[{"input_id":"done-id","text":"already delivered"},{"input_id":"pending-id","text":"still pending"}]}}),
            msg("kept",Some("delivered"),"assistant","retained"),
            json!({"type":"compaction","entry_id":"compact","parent_entry_id":"kept","summary":"summary","first_kept_entry_id":"kept"}),
        ]);
        let h = env.host().faux(json!([])).resume().build().await;
        expose(&h);
        let (count,text): (i64,String) = eval(&h,"local q=A:list_queue().data; return #q,q[1] and q[1].text or ''").await;
        assert_eq!((count,text),(1,"still pending".into()),"replay must retain pending input and suppress committed input across compaction");
        h.shutdown().await;
    }).await
}

/// Refusing a durable queue clear must retain the complete entries and must not transfer a
/// partial draft to the frontend; a successful retry returns every original attachment.
#[tokio::test]
async fn abort_queue_write_failure_retains_full_inputs() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"init"},{"text":"hold"}])).build().await;
        expose(&h);
        h.prompt("initial", json!({})).await.unwrap();
        exec(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                end
            end)
            HELD_TURN = A:prompt("hold")
            HELD_REL = rel_tx
            held_rx:recv()
            A:steer("one [Image #1]", {images={{type="image",mime="image/png",data="first",metadata={source="one"}}},image_markers={{text="[Image #1]",offset=4}}})
            A:prompt("two [Image #1]", {when_busy="queue", images={{type="image",mime="image/gif",data="second",metadata={source="two"}}},image_markers={{text="[Image #1]",offset=4}}})
            BEFORE_ABORT = mare.json.encode(A:list_queue().data)
            REJECT_QUEUE = true
            C.cordis:get("policy"):voter("session/commit-ready", function() if REJECT_QUEUE then return {decision="deny",reason="queue clear refused"} end end)
            EDITOR_CALLS = 0
            mare.ui.connected = function() return true end
            mare.ui.set_editor = function(text, opts) EDITOR_CALLS=EDITOR_CALLS+1; EDITOR_TEXT=text; EDITOR_IMAGES=opts.images end
        "#).await;
        let (ok, error, unchanged, calls): (bool, String, bool, i64) = eval(&h, r#"
            local ok, err = pcall(A.cancel, A)
            return ok, tostring(err), BEFORE_ABORT == mare.json.encode(A:list_queue().data), EDITOR_CALLS
        "#).await;
        assert!(!ok);
        assert!(error.contains("queue clear refused"), "{error}");
        assert!(unchanged, "failed clear must retain stable identities and all nested attachment fields");
        assert_eq!(calls, 0, "no editor transfer before durable clear succeeds");
        let encoded: String = eval(&h, r#"
            REJECT_QUEUE = false
            local cancel_res = A:cancel()
            HELD_REL:send(true)
            A:wait(HELD_TURN.turn_id)
            return mare.json.encode({entries=cancel_res.removed, images=EDITOR_IMAGES, text=EDITOR_TEXT, calls=EDITOR_CALLS, pending=#A:list_queue().data})
        "#).await;
        let result: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(result["entries"].as_array().unwrap().len(), 2);
        assert_eq!(result["images"], json!([
            {"type":"image","mime":"image/png","data":"first","metadata":{"source":"one"}},
            {"type":"image","mime":"image/gif","data":"second","metadata":{"source":"two"}},
        ]));
        assert_eq!(result["text"], "one [Image #1]\n\ntwo [Image #2]");
        assert_eq!(result["calls"], 1);
        assert_eq!(result["entries"][0]["images"][0]["data"], "first");
        assert_eq!(result["entries"][1]["images"][0]["data"], "second");
        assert_eq!(result["pending"], 0);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn abort_cancels_active_token_even_when_queue_clear_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let result: (bool, bool, i64) = eval(&h, r#"
            local state={busy=true,turn_id='t1',steer={},follow={{id='pending',text='keep'}},token=mare.task.token()}
            -- the internal agent table, before agent.service_events publishes it (`mare_lib.service`)
            local service
            local loaded=package.loaded
            local events,session=loaded['agent.service_events'],loaded['agent.service_session']
            loaded['agent.service_events']=function(d) service=d.agent end
            loaded['agent.service_session']=function() end
            local ctx={on=function() end,on_local=function() end,on_dispose=function() end,with_service=function() end,inject=function() end,bus={bail=function() end,emit=function() end}}
            -- the plugin's D: optional services are followers (no session here)
            local built,err=pcall(require('agent.service'),{ctx=ctx,cfg={},A=state,persist={},inputs={mutate=function() error('disk failure') end},loop={},follow={session=function() end}})
            loaded['agent.service_events'],loaded['agent.service_session']=events,session
            assert(built,err)
            local ok=pcall(service.cancel,service,nil,{})
            return ok,state.token:is_cancelled(),#state.follow
        "#).await;
        assert_eq!(result, (false, true, 1));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn concurrent_inbox_acceptance_is_ordered_and_broadcast_to_all_observers() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (first, second, detached, observers): (bool, bool, bool, i64) = eval(
            &h,
            r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'gate' }
                end
            end)
            local held_turn = A:prompt('hold')
            held_rx:recv()
            local entered_tx,entered_rx=mare.task.channel()
            local release_tx,release_rx=mare.task.channel()
            local done_tx,done_rx=mare.task.channel()
            local gate=true
            C.cordis:get('policy'):voter('session/commit-ready',function(event)
                if gate and #event.inbox.follow_up==1 then
                    gate=false
                    entered_tx:send(true)
                    release_rx:recv()
                end
            end)
            local notifications=0
            C.cordis:on('queue/changed',function() notifications=notifications+1; return true end)
            C.cordis:on('queue/changed',function() notifications=notifications+1 end)
            local a=mare.task.spawn(function() done_tx:send(pcall(A.prompt, A, 'A', { when_busy = 'queue' })) end)
            entered_rx:recv()
            local started_tx,started_rx=mare.task.channel()
            local b=mare.task.spawn(function() started_tx:send(true); done_tx:send(pcall(A.prompt, A, 'B', { when_busy = 'queue' })) end)
            started_rx:recv()
            release_tx:send(true)
            local first,second=done_rx:recv(),done_rx:recv()
            a:wait();b:wait()
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            local q=A:list_queue().data
            q[1].text='mutated external snapshot'
            table.remove(q,2)
            local real=A:list_queue().data
            return first,second,#real==2 and real[1].text=='A' and real[2].text=='B',notifications
        "#,
        )
        .await;
        assert!(first && second);
        assert!(detached, "external queue snapshots must not mutate authoritative entries");
        assert_eq!(observers, 4, "both observers see both committed changes despite return true");
        h.shutdown().await;
        let h = env.host().resume().build().await;
        expose(&h);
        let texts: String = eval(&h, "local q=A:list_queue().data; return q[1].text..','..q[2].text").await;
        assert_eq!(texts, "A,B", "both confirmed inputs survive restart in order");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn abort_during_input_commit_returns_the_uncommitted_image_once() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().faux(json!([{"text":"done"}])).build().await;
        expose(&h);
        let result: String=eval(&h,r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local gating = true
            local rm_gate
            rm_gate = C.cordis:get('policy'):voter('request/ready', function()
                if gating then
                    gating = false
                    held_tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'seed' }
                end
            end)
            local seed_turn = A:prompt('seed')
            held_rx:recv()
            A:prompt('image waiting',{when_busy='queue',images={{type='image',mime='image/png',data='AQID'}}})
            A:cancel({ turn_id = seed_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(seed_turn.turn_id)
            rm_gate()
            local blocked_tx,blocked_rx=mare.task.channel()
            local release_tx,release_rx=mare.task.channel()
            C.cordis:get('policy'):voter('session/commit-ready',function(event)
                if #event.messages>0 then blocked_tx:send(true);release_rx:recv() end
            end)
            local turn = A:prompt('trigger')
            blocked_rx:recv()
            local started_tx,started_rx=mare.task.channel()
            local reply_tx,reply_rx=mare.task.channel()
            local abort=mare.task.spawn(function() started_tx:send(true);reply_tx:send(A:cancel().removed) end)
            started_rx:recv()
            release_tx:send(true)
            abort:wait(); A:wait(turn.turn_id)
            local entries=reply_rx:recv()
            local delivered=0
            for _,message in ipairs(A:list_messages().data) do if message._input_id then delivered=delivered+1 end end
            return mare.json.encode({entries=entries,delivered=delivered,pending=#A:list_queue().data})
        "#).await;
        let result: serde_json::Value=serde_json::from_str(&result).unwrap();
        assert_eq!(result["entries"].as_array().unwrap().len(),1);
        assert_eq!(result["entries"][0]["images"][0]["data"],"AQID");
        assert_eq!(result["delivered"],0,"cancelled candidate must never append after abort");
        assert_eq!(result["pending"],0);
        h.shutdown().await;
        let h=env.host().resume().build().await;
        expose(&h);
        let (queued,delivered):(i64,i64)=eval(&h,"local n=0;for _,m in ipairs(A:list_messages().data) do if m._input_id then n=n+1 end end;return #A:list_queue().data,n").await;
        assert_eq!((queued,delivered),(0,0),"the returned input has a single owner after restart");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn queued_image_reference_repairs_existing_bytes_and_rejects_real_write_failure() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"one"},{"text":"two"},{"text":"three"}])).build().await;
        expose(&h);
        let image = json!({"type":"image","mime":"image/png","data":"AQIDBA=="});
        h.prompt("first", json!({"images":[image.clone()]})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        let attachment_dir = std::path::PathBuf::from(path.trim_end_matches(".jsonl").to_owned() + ".attachments");
        let name: String = eval(&h, "return mare.crypto.sha256(string.char(1,2,3,4))..'.png'").await;
        let object = attachment_dir.join(&name);
        std::fs::write(&object, b"partial previous crash").unwrap();
        let second = h.prompt("repair", json!({"images":[image.clone()]})).await.unwrap();
        assert!(second["error"].is_null(), "{second}");
        assert_eq!(
            std::fs::read(&object).unwrap(),
            [1, 2, 3, 4],
            "an existing partial object must be atomically repaired before reference commit"
        );
        exec(&h, r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local gating = true
            C.cordis:get('policy'):voter('request/ready', function()
                if gating then
                    gating = false
                    held_tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'seed' }
                end
            end)
            SEED_TURN = A:prompt('seed')
            SEED_REL = rel_tx
            held_rx:recv()
        "#).await;
        h.follow_up_with("pending image", json!({"images":[image]})).await.unwrap();
        exec(&h, "A:cancel({ turn_id = SEED_TURN.turn_id, clear_queue = false }); SEED_REL:send(true); A:wait(SEED_TURN.turn_id)").await;
        let backup = attachment_dir.with_extension("backup");
        std::fs::rename(&attachment_dir, &backup).unwrap();
        std::fs::write(&attachment_dir, b"not a directory").unwrap();
        let rejected = h.prompt("trigger failed consume", json!({})).await.unwrap();
        assert!(rejected["error"]["message"].is_string(), "{rejected}");
        let queued: i64 = eval(&h, "return #A:list_queue().data").await;
        assert_eq!(queued, 1, "failed reference preparation retains the accepted complete input");
        std::fs::remove_file(&attachment_dir).unwrap();
        std::fs::rename(&backup, &attachment_dir).unwrap();
        h.shutdown().await;
        let h = env.host().resume().build().await;
        expose(&h);
        let (count, mime): (i64, String) = eval(&h, "local q=A:list_queue().data;return #q,q[1].images[1].mime").await;
        assert_eq!((count, mime), (1, "image/png".into()), "failed image consumption remains recoverable after restart");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn inbox_commit_from_a_tool_reuses_the_session_owned_writer() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().build().await;
        expose(&h);
        let text:String=eval(&h,r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'gate' }
                end
            end)
            local turn = A:prompt('hold')
            held_rx:recv()
            assert(type(S:record_tool_state({tool_call_id='queue-from-tool',name='probe',state='started',durable=true}))=='string')
            mare.exec.run({tool='probe',tool_call_id='queue-from-tool'},function()
                A:prompt('accepted from protected tool context', {when_busy='queue'})
            end)
            local res_text = A:list_queue().data[1].text
            A:cancel({ turn_id = turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(turn.turn_id)
            return res_text
        "#).await;
        assert_eq!(text,"accepted from protected tool context");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn steering_waiting_at_commit_cannot_cross_into_a_new_turn() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().build().await;
        expose(&h);
        let (accepted,error,pending):(bool,String,i64)=eval(&h,r#"
            local first_tx,first_rx=mare.task.channel()
            local second_tx,second_rx=mare.task.channel()
            local release1,wait1=mare.task.channel()
            local release2,wait2=mare.task.channel()
            local requests=0
            C.cordis:get('policy'):voter('request/ready',function()
                requests=requests+1
                if requests==1 then first_tx:send(true);wait1:recv()
                elseif requests==2 then second_tx:send(true);wait2:recv() end
            end)
            local queue_tx,queue_rx=mare.task.channel()
            local release_queue,wait_queue=mare.task.channel()
            C.cordis:get('policy'):voter('session/commit-ready',function(batch)
                if #batch.inbox.steer>0 then queue_tx:send(true);wait_queue:recv() end
            end)
            local old=A:prompt('old turn')
            first_rx:recv()
            local reply_tx,reply_rx=mare.task.channel()
            local steering=mare.task.spawn(function() reply_tx:send(table.pack(pcall(A.steer, A, 'old-turn-only input'))) end)
            queue_rx:recv()
            A:cancel({clear_queue=false})
            release1:send(true)
            A:wait(old.turn_id)
            local fresh=A:prompt('new turn')
            second_rx:recv()
            release_queue:send(true)
            local result=reply_rx:recv()
            steering:wait()
            local pending=#A:list_queue().data
            A:cancel({clear_queue=false})
            release2:send(true)
            A:wait(fresh.turn_id)
            return result[1],tostring(result[2]),pending
        "#).await;
        assert!(!accepted,"stale steering must not be acknowledged for a replacement turn");
        assert!(error.contains("stale") || error.contains("changed"),"{error}");
        assert_eq!(pending,0,"old-turn steering cannot leak into the new turn inbox");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn malformed_attachment_marker_identity_is_rejected_before_queue_acceptance() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().build().await;
        expose(&h);
        let rejected:i64=eval(&h,r#"
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:on('turn/started', function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                end
            end)
            HELD_M = A:prompt('hold')
            HELD_M_REL = rel_tx
            held_rx:recv()
            local rejected=0
            for _,markers in ipairs({
                {{text='[Image #1]'}},
                {{text='[Image #1]',offset=-1}},
                {{text='[Image #99]',offset=0}},
                {},
            }) do
                local ok=pcall(A.prompt,A,'[Image #1]',{when_busy='queue',images={{type='image',mime='image/png',data='AQID'}},image_markers=markers})
                assert(not ok,'malformed marker identity must not be accepted')
                rejected=rejected+1
            end
            assert(#A:list_queue().data==0)
            A:prompt('empty image collection',{when_busy='queue',images={},image_markers={}})
            A:cancel({turn_id=HELD_M.turn_id, clear_queue=false})
            HELD_M_REL:send(true)
            A:wait(HELD_M.turn_id)
            return rejected
        "#).await;
        assert_eq!(rejected,4);
        let reply=h.abort_with_inputs().await.unwrap();
        assert_eq!(reply["removed"][0]["text"],"empty image collection");
        assert_eq!(reply["removed"][0]["images"],json!([]),"known attachment arrays retain their array identity even when empty");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn abort_keep_inbox_during_consume_retains_queued_followup_across_restart() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"text": "first turn"},
                {"text": "should not run"}
            ]))
            .build()
            .await;
        expose(&h);
        let (delivered, follow_len, follow_text): (i64, usize, String) = eval(
            &h,
            r#"
            local at_commit_tx, at_commit_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local started_tx, started_rx = mare.task.channel()
            local done_tx, done_rx = mare.task.channel()
            local delivered = 0
            C.cordis:on("step/started", function()
                delivered = delivered + 1
            end)
            local gating = true
            local rm_g
            rm_g = C.cordis:get("policy"):voter("request/ready", function()
                if gating then
                    gating = false
                    at_commit_tx:send(true)
                    release_rx:recv()
                    return { decision = "deny", reason = "seed" }
                end
            end)
            local seed_turn = A:prompt("seed")
            at_commit_rx:recv()
            assert(A:prompt("keep me", { when_busy = "queue" }).status == "queued")
            A:cancel({ turn_id = seed_turn.turn_id, clear_queue = false })
            release_tx:send(true)
            A:wait(seed_turn.turn_id)
            rm_g()
            C.cordis:get("policy"):voter("session/commit-ready", function(ev)
                for _, m in ipairs(ev.messages or {}) do
                    if m._input_id then
                        at_commit_tx:send(true)
                        release_rx:recv()
                        break
                    end
                end
            end)
            local started = A:prompt("go")
            assert(at_commit_rx:recv())
            local abort_task = mare.task.spawn(function()
                started_tx:send(true)
                assert(C.cordis:get("agent"):cancel({ clear_queue = false }).turn_id == started.turn_id)
            end)
            assert(started_rx:recv())
            release_tx:send(true)
            abort_task:wait()
            local res = A:wait(started.turn_id)
            assert(type(res.error) == "table" and res.error.code == "aborted", mare.json.encode(res))
            local q = A:list_queue().data
            return delivered, #q, q[1] and q[1].text or ""
        "#,
        )
        .await;
        assert_eq!(delivered, 1);
        assert_eq!(follow_len, 1);
        assert_eq!(follow_text, "keep me");
        h.shutdown().await;

        let h2 = env.host().resume().build().await;
        expose(&h2);
        let (follow_len2, follow_text2): (usize, String) = eval(
            &h2,
            r#"
            local q = A:list_queue().data
            return #q, q[1] and q[1].text or ""
        "#,
        )
        .await;
        assert_eq!(follow_len2, 1);
        assert_eq!(follow_text2, "keep me");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn abort_keep_inbox_during_turn_stopping_stops_before_next_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"text": "first turn"},
                {"text": "should not run"}
            ]))
            .build()
            .await;
        expose(&h);
        let (turns, has_continue): (i64, bool) = eval(
            &h,
            r#"
            local stopping_tx, stopping_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local started_tx, started_rx = mare.task.channel()
            local turns = 0
            C.cordis:on("step/started", function()
                turns = turns + 1
            end)
            C.cordis:on("agent/turn-stopping", function()
                stopping_tx:send(true)
                release_rx:recv()
                return "continue"
            end)
            local started = A:prompt("go")
            assert(stopping_rx:recv())
            local abort_task = mare.task.spawn(function()
                started_tx:send(true)
                assert(C.cordis:get("agent"):cancel({ clear_queue = false }).turn_id == started.turn_id)
            end)
            assert(started_rx:recv())
            release_tx:send(true)
            abort_task:wait()
            local res = A:wait(started.turn_id)
            assert(type(res.error) == "table" and res.error.code == "aborted", mare.json.encode(res))
            local has_continue = false
            for _, m in ipairs(A:list_messages().data) do
                for _, block in ipairs(m.content or {}) do
                    if block.text == "continue" then
                        has_continue = true
                    end
                end
            end
            return turns, has_continue
        "#,
        )
        .await;
        assert_eq!(turns, 1);
        assert!(!has_continue);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn frontend_abort_turn_retains_queued_followup_across_restart() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"text": "first turn"},
                {"text": "should not run"}
            ]))
            .build()
            .await;
        expose(&h);
        let (delivered, follow_len, follow_text): (i64, usize, String) = eval(
            &h,
            r#"
            local at_commit_tx, at_commit_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local started_tx, started_rx = mare.task.channel()
            local delivered = 0
            C.cordis:on("step/started", function()
                delivered = delivered + 1
            end)
            local gating = true
            local rm_g
            rm_g = C.cordis:get("policy"):voter("request/ready", function()
                if gating then
                    gating = false
                    at_commit_tx:send(true)
                    release_rx:recv()
                    return { decision = "deny", reason = "seed" }
                end
            end)
            local seed_turn = A:prompt("seed")
            at_commit_rx:recv()
            assert(A:prompt("keep on pause", { when_busy = "queue" }).status == "queued")
            A:cancel({ turn_id = seed_turn.turn_id, clear_queue = false })
            release_tx:send(true)
            A:wait(seed_turn.turn_id)
            rm_g()
            C.cordis:get("policy"):voter("session/commit-ready", function(ev)
                for _, m in ipairs(ev.messages or {}) do
                    if m._input_id then
                        at_commit_tx:send(true)
                        release_rx:recv()
                        break
                    end
                end
            end)
            local started = A:prompt("go")
            assert(at_commit_rx:recv())
            local abort_task = mare.task.spawn(function()
                started_tx:send(true)
                local tid = A:get().turn_id
                assert(A:cancel({
                    turn_id = tid,
                    clear_queue = false,
                }).turn_id == tid)
            end)
            assert(started_rx:recv())
            release_tx:send(true)
            abort_task:wait()
            local res = A:wait(started.turn_id)
            assert(type(res.error) == "table" and res.error.code == "aborted", mare.json.encode(res))
            local q = A:list_queue().data
            return delivered, #q, q[1] and q[1].text or ""
        "#,
        )
        .await;
        assert_eq!(delivered, 1);
        assert_eq!(follow_len, 1);
        assert_eq!(follow_text, "keep on pause");
        h.shutdown().await;

        let h2 = env.host().resume().build().await;
        expose(&h2);
        let (follow_len2, follow_text2): (usize, String) = eval(
            &h2,
            r#"
            local q = A:list_queue().data
            return #q, q[1] and q[1].text or ""
        "#,
        )
        .await;
        assert_eq!(follow_len2, 1);
        assert_eq!(follow_text2, "keep on pause");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn queued_prompt_refused_by_readiness_preserves_queue_without_persisting() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"ok after ready"}])).build().await;
        expose(&h);
        let (not_sent, queued_len, history_len, sent_text): (bool, i64, i64, String) = eval(
            &h,
            r#"
            local refuse = true
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local gating = true
            C.cordis:get('policy'):voter('request/ready', function()
                if gating then
                    gating = false
                    hold_tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'seed' }
                end
                if refuse then
                    return { decision = 'deny', reason = 'not ready yet' }
                end
            end)
            local seed_turn = A:prompt('seed')
            hold_rx:recv()
            assert(A:prompt('queued before ready', { when_busy = 'queue' }).status == 'queued')
            rel_tx:send(true)
            A:wait(seed_turn.turn_id)
            local queued_before = A:list_queue().data
            assert(#queued_before == 1)
            local id_before = queued_before[1].input_id

            local res = A:wait(A:continue().turn_id)
            assert(res.not_sent == true, 'refused queued prompt must return not_sent')
            local queued_after = A:list_queue().data
            assert(#queued_after == 1 and queued_after[1].input_id == id_before, 'queued item and its identity must stay in queue')
            local msgs_after = #A:list_messages().data
            assert(msgs_after == 0, 'refused queued prompt must not persist queued items to history')

            refuse = false
            local ok_res = A:wait(A:continue().turn_id)
            assert(not ok_res.not_sent and not ok_res.error, tostring(ok_res.error))
            return res.not_sent == true, #A:list_queue().data, msgs_after, ok_res.text
            "#,
        )
        .await;
        assert!(not_sent);
        assert_eq!(queued_len, 0, "accepted queued prompt consumes the queue");
        assert_eq!(history_len, 0, "refused queued prompt must not persist to history");
        assert_eq!(sent_text, "ok after ready");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn queued_prompt_refused_by_request_lease_preserves_queue_and_attachments() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"ok after lease"}])).build().await;
        expose(&h);
        let (not_sent, lease_err, queued_len, history_len, sent_text, image_count): (
            bool,
            String,
            i64,
            i64,
            String,
            i64,
        ) = eval(
            &h,
            r#"
            local request_lease = function(...) return C.cordis:get('kernel'):request_lease(...) end
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local gating = true
            C.cordis:on('turn/started', function()
                if gating then
                    gating = false
                    held_tx:send(true)
                    rel_rx:recv()
                end
            end)
            local seed_turn = A:prompt('seed')
            held_rx:recv()
            assert(A:prompt('queued [Image #1]', {
                when_busy = 'queue',
                images = { { type = 'image', mime = 'image/png', data = 'AQID' } },
                image_markers = { { text = '[Image #1]', offset = 7 } },
            }).status == 'queued')
            A:cancel({ turn_id = seed_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(seed_turn.turn_id)
            local msgs_before = #A:list_messages().data
            local queued_before = A:list_queue().data
            assert(#queued_before == 1)
            local id_before = queued_before[1].input_id

            local held_lease = assert(request_lease())
            local res = A:wait(A:continue().turn_id)
            assert(res.not_sent == true, 'lease-refused queued prompt must return not_sent')
            local queued_after = A:list_queue().data
            assert(#queued_after == 1 and queued_after[1].input_id == id_before, string.format('queued item and identity must stay in queue on lease failure: len=%d id=%s expected=%s err=%s', #queued_after, tostring(queued_after[1] and queued_after[1].input_id), tostring(id_before), tostring(res.error)))
            assert(#(queued_after[1].images or {}) == 1, 'queued image attachment must stay in queue on lease failure')
            local msgs_after = #A:list_messages().data
            assert(msgs_after == msgs_before, 'lease-refused queued prompt must not persist queued items to history')

            held_lease:release()
            local ok_res = A:wait(A:continue().turn_id)
            assert(not ok_res.not_sent and not ok_res.error, tostring(ok_res.error))
            local img_n = 0
            for _, m in ipairs(A:list_messages().data) do
                if m.role == 'user' then
                    for _, b in ipairs(m.content or {}) do
                        if b.type == 'image' then img_n = img_n + 1 end
                    end
                end
            end
            return res.not_sent == true, tostring(type(res.error) == "table" and res.error.message or res.error or ''), #A:list_queue().data, msgs_after - msgs_before, ok_res.text, img_n
            "#,
        )
        .await;
        assert!(not_sent);
        assert!(lease_err.contains("another request is in progress"), "{lease_err}");
        assert_eq!(queued_len, 0, "accepted queued prompt consumes the queue");
        assert_eq!(history_len, 0, "lease-refused queued prompt must not persist to history");
        assert_eq!(sent_text, "ok after lease");
        assert_eq!(image_count, 1, "preserved queued attachment must be sent after lease is released");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_queue_observer_notifications_arrive_in_commit_order() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (seen, final_queued): (String, i64) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            eval(
                &h,
                r#"
                local hold_tx, hold_rx = mare.task.channel()
                local rel_tx, rel_rx = mare.task.channel()
                C.cordis:get('policy'):voter('request/ready', function()
                    if hold_tx then
                        local tx = hold_tx; hold_tx = nil
                        tx:send(true)
                        rel_rx:recv()
                        return { decision = 'deny', reason = 'gate' }
                    end
                end)
                local held_turn = A:prompt('hold')
                hold_rx:recv()
                local entered_tx, entered_rx = mare.task.channel()
                local release_tx, release_rx = mare.task.channel()
                local gate = true
                local order = {}
                local function follow_count(ev)
                    local n = 0
                    for _, item in ipairs((ev and ev.items) or {}) do
                        if item.lane == 'follow_up' then n = n + 1 end
                    end
                    return n
                end
                C.cordis:on('queue/changed', function(ev)
                    if gate and follow_count(ev) == 1 then
                        gate = false
                        entered_tx:send(true)
                        release_rx:recv()
                    end
                end)
                C.cordis:on('queue/changed', function(ev)
                    order[#order + 1] = tostring(follow_count(ev))
                end)

                local t1 = mare.task.spawn(function() assert(A:prompt('first', { when_busy = 'queue' }).status == 'queued') end)
                entered_rx:recv()
                local t2 = mare.task.spawn(function() assert(A:prompt('second', { when_busy = 'queue' }).status == 'queued') end)
                t2:wait()
                release_tx:send(true)
                t1:wait()
                A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
                rel_tx:send(true)
                A:wait(held_turn.turn_id)
                return table.concat(order, ','), #A:list_queue().data
                "#,
            ),
        )
        .await
        .expect("timed out waiting for queue observer notifications");
        assert_eq!(seen, "1,2", "observers must receive queue snapshots in commit order");
        assert_eq!(final_queued, 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn queue_remove_is_persisted_before_it_returns() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let removed_ok: bool = eval(
            &h,
            r#"
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if hold_tx then
                    local tx = hold_tx; hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'gate' }
                end
            end)
            local held_turn = A:prompt('hold')
            hold_rx:recv()
            assert(A:prompt({
                { type = 'text', text = 'persist remove' },
                { type = 'image', data = 'AQID', mime = 'image/png' },
            }, { when_busy = 'queue' }).status == 'queued')
            A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            local listed = A:list_queue()
            assert(#listed.data == 1 and listed.data[1].lane == 'follow_up')
            assert(listed.data[1].images[1].mime == 'image/png' and listed.data[1].images[1].data == nil)
            A:delete_queued(listed.data[1].input_id)
            assert(#A:list_queue().data == 0)
            return true
            "#,
        )
        .await;
        assert!(removed_ok);
        h.shutdown().await;
        let h = env.host().resume().build().await;
        expose(&h);
        let count: i64 = eval(&h, "return #A:list_queue().data").await;
        assert_eq!(count, 0, "removed queue item must stay gone after restart");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn queue_list_remove_and_clear_project_registered_secrets() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let raw: String = eval(
            &h,
            r#"
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if hold_tx then
                    local tx = hold_tx; hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = 'deny', reason = 'gate' }
                end
            end)
            local held_turn = A:prompt('hold')
            hold_rx:recv()
            local secret = "QUEUE_VIEW_SECRET_91"
            assert(A:prompt("pre-queued " .. secret, {
                when_busy = "queue",
                images = { { type = "image", mime = "image/png", data = "AQID", path = "/tmp/" .. secret .. "/a.png" } },
                content = {
                    { type = "text", text = "block " .. secret },
                    { type = "image", mime = "image/png", path = "/tmp/" .. secret .. "/b.png" },
                },
            }).status == "queued")
            local kept = mare.projection.register(secret)
            local listed = A:list_queue()
            assert(#listed.data == 1)
            local item_id = listed.data[1].input_id

            local secret2 = "QUEUE_VIEW_SECRET_92"
            local kept2 = mare.projection.register(secret2)
            assert(A:steer("[m " .. secret2 .. "] " .. secret2, {
                images = { { type = "image", mime = "image/png", data = "AQID", path = "/tmp/" .. secret2 .. "/c.png" } },
                image_markers = { { text = "[m " .. secret2 .. "]", offset = 0 } },
                content = { { type = "text", text = "content " .. secret2 } },
            }).input_id ~= nil)
            local listed2 = A:list_queue()
            A:delete_queued(item_id)
            local cleared = A:cancel({ clear_queue = true }).removed
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            return mare.json.encode({
                ref1 = kept.reference,
                ref2 = kept2.reference,
                listed = listed,
                listed2 = listed2,
                cleared = cleared,
            })
        "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ref1 = report["ref1"].as_str().unwrap();
        let ref2 = report["ref2"].as_str().unwrap();
        for field in ["listed", "listed2"] {
            let body = report[field].to_string();
            assert!(!body.contains("QUEUE_VIEW_SECRET_91"), "raw secret 91 leaked in {field}: {body}");
            assert!(body.contains(ref1), "projected reference missing in {field}: {body}");
        }
        for field in ["listed2", "cleared"] {
            let body = report[field].to_string();
            assert!(!body.contains("QUEUE_VIEW_SECRET_92"), "raw secret 92 leaked in {field}: {body}");
            assert!(body.contains(ref2), "projected reference missing in {field}: {body}");
        }
        h.shutdown().await;
    })
    .await
}

/// A `wake = false` notification must survive a restart/resume via the durable inbox and
/// be delivered once (without leaking into the user-visible input queue or repeating on
/// a subsequent prompt).
#[tokio::test]
async fn wake_false_notification_survives_restart_and_delivers_once_on_next_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let session_id: String = {
            let h = env.host().faux(json!([{ "text": "first reply" }])).build().await;
            expose(&h);
            let id: String = eval(
                &h,
                r#"
                local id = S:create({ name = "durable-note-session" }).session_id
                local turn = A:prompt("first hello")
                local res = A:wait(turn.turn_id)
                assert(not res.error, "first prompt failed: " .. tostring(res.error))
                A:notify("DURABLE_BG_NOTICE_731", { wake = false })
                -- Notes must not masquerade as user-queued inputs and must not be wiped by clear_queue.
                assert(#A:list_queue().data == 0, "wake=false note must not appear in list_queue")
                A:cancel({ clear_queue = true })
                return id
                "#,
            )
            .await;
            h.shutdown().await;
            id
        };

        // Restart host, resume the same session, and send two prompts:
        // prompt 2 must receive the persisted note; prompt 3 must NOT receive it again.
        let h2 = env.host().faux(json!([{ "text": "second reply" }, { "text": "third reply" }])).build().await;
        expose(&h2);
        let report_raw: String = eval(
            &h2,
            &format!(
                r#"
                assert(S:load({{ session_id = {id:?} }}))
                assert(#A:list_queue().data == 0, "resumed note must not appear in list_queue")

                local seen = {{}}
                local unsub = C.cordis:on("request/build", function(req)
                    local texts = {{}}
                    for _, m in ipairs(req.messages or {{}}) do
                        if type(m.content) == "string" then
                            texts[#texts + 1] = m.content
                        elseif type(m.content) == "table" then
                            for _, part in ipairs(m.content) do
                                if part.type == "text" and type(part.text) == "string" then
                                    texts[#texts + 1] = part.text
                                end
                            end
                        end
                    end
                    seen[#seen + 1] = table.concat(texts, "\n")
                end)

                local t2 = A:prompt("after resume")
                local r2 = A:wait(t2.turn_id)
                assert(not r2.error, "second prompt failed: " .. tostring(r2.error))
                local t3 = A:prompt("third turn")
                local r3 = A:wait(t3.turn_id)
                assert(not r3.error, "third prompt failed: " .. tostring(r3.error))
                unsub()

                local count_in_history = 0
                for _, m in ipairs(A:list_messages().data) do
                    local body = type(m.content) == "string" and m.content or mare.json.encode(m.content or {{}})
                    if body:find("DURABLE_BG_NOTICE_731", 1, true) then
                        count_in_history = count_in_history + 1
                    end
                end
                return mare.json.encode({{
                    req2_has_note = (seen[1] or ""):find("DURABLE_BG_NOTICE_731", 1, true) ~= nil,
                    count_in_history = count_in_history,
                }})
                "#,
                id = session_id,
            ),
        )
        .await;
        h2.shutdown().await;
        let report: serde_json::Value = serde_json::from_str(&report_raw).unwrap();
        assert_eq!(
            report["req2_has_note"].as_bool(),
            Some(true),
            "persisted wake=false note must be delivered on next prompt after restart: {report}"
        );
        assert_eq!(
            report["count_in_history"].as_u64(),
            Some(1),
            "persisted wake=false note must be committed exactly once: {report}"
        );
    })
    .await
}
