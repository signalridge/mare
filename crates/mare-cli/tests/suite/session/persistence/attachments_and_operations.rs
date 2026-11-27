//! Image attachment persistence and generic operation receipts/claims.

use super::*;

#[tokio::test]
async fn image_reference_repairs_partial_objects_and_refuses_real_attachment_write_failure() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().faux(json!([{"text":"one"},{"text":"two"},{"text":"three"}])).build().await;
        expose(&h);
        let image=json!({"type":"image","mime":"image/png","data":"AQIDBA=="});
        let first=h.prompt("first",json!({"images":[image.clone()]})).await.unwrap();
        assert!(first["error"].is_null(),"{first}");
        let path: String=eval(&h,"return S:info().path").await;
        let directory=std::path::PathBuf::from(path.trim_end_matches(".jsonl").to_owned()+".attachments");
        let name: String=eval(&h,"return mare.crypto.sha256(string.char(1,2,3,4))..'.png'").await;
        let object=directory.join(name);
        std::fs::write(&object,b"partial previous crash").unwrap();
        let repaired=h.prompt("repair",json!({"images":[image.clone()]})).await.unwrap();
        assert!(repaired["error"].is_null(),"{repaired}");
        assert_eq!(std::fs::read(&object).unwrap(),[1,2,3,4],"an existing path cannot stand in for a complete object");
        let backup=directory.with_extension("backup");
        std::fs::rename(&directory,&backup).unwrap();
        std::fs::write(&directory,b"not a directory").unwrap();
        let failed=h.prompt("refused image",json!({"images":[image]})).await.unwrap();
        assert!(failed["error"]["message"].is_string(),"real attachment write failure must refuse the turn: {failed}");
        let persisted:i64=eval(&h,"local n=0;for _,e in ipairs(S:entries()) do if e.type=='message' and e.message.role=='user' then n=n+1 end end;return n").await;
        assert_eq!(persisted,2,"a failed attachment reference must not enter the durable journal");
        h.shutdown().await;
        std::fs::remove_file(&directory).unwrap();
        std::fs::rename(&backup,&directory).unwrap();
    }).await
}

#[tokio::test]
async fn full_attachment_digest_keeps_colliding_short_prefixes_as_distinct_objects() {
    local(async {
        // Two real inputs whose SHA-256 digests share the 10-hex prefix `869e226e41` (found by a
        // birthday search): a short-prefix object name would merge them, the full digest must not.
        // Real digests (no mocked mare.crypto.sha256) so restart-time content verification of
        // attachment objects (store/attachments.rs) checks the same identities the writer used.
        const FIRST:&str="MARE-OBJ-2458626";
        const SECOND:&str="MARE-OBJ-2559885";
        const FIRST_SHA:&str="869e226e4129f6fb6aff5ebd351b84dfe50a053ce78626c4172294d0ee85cb56";
        const SECOND_SHA:&str="869e226e41fd1c42fa4341c913e4089a6d70f1be2d28c9367402244a648f9b4b";
        assert_eq!(FIRST_SHA[..10],SECOND_SHA[..10],"the samples must collide on the short prefix");
        let env=TestEnv::new().await;
        let h=env.host().faux(json!([{"text":"first"},{"text":"second"}])).build().await;
        expose(&h);
        for (bytes,sha) in [(FIRST,FIRST_SHA),(SECOND,SECOND_SHA)] {
            global(&h,"OBJECT_BYTES",bytes.to_owned());
            let actual:String=eval(&h,"return mare.crypto.sha256(OBJECT_BYTES)").await;
            assert_eq!(actual,sha,"sample digest");
            let image:String=eval(&h,"return mare.crypto.base64_encode(OBJECT_BYTES)").await;
            let result=h.prompt(bytes,json!({"images":[{"type":"image","mime":"image/png","data":image}]})).await.unwrap();
            assert!(result["error"].is_null(),"{result}");
        }
        let path:String=eval(&h,"return S:info().path").await;
        let objects=std::path::PathBuf::from(path.trim_end_matches(".jsonl").to_owned()+".attachments");
        let first=format!("{FIRST_SHA}.png");
        let second=format!("{SECOND_SHA}.png");
        assert_eq!(std::fs::read(objects.join(&first)).unwrap(),FIRST.as_bytes());
        assert_eq!(std::fs::read(objects.join(&second)).unwrap(),SECOND.as_bytes());
        let raw=std::fs::read_to_string(&path).unwrap();
        let names:Vec<String>=raw.lines().map(|line|serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|entry|entry["type"]=="message" && entry["message"]["role"]=="user")
            .flat_map(|entry|entry["message"]["content"].as_array().unwrap().clone())
            .filter_map(|block|block["attachment"].as_str().map(str::to_owned)).collect();
        assert_eq!(names,vec![first,second],"the journal must retain distinct complete SHA256 identities");
        h.shutdown().await;
        let h=env.host().resume().build().await;
        expose(&h);
        let data:Vec<String>=eval(&h,"local out={};for _,m in ipairs(A:list_messages().data) do for _,b in ipairs(m.content or {}) do if b.type=='image' then out[#out+1]=mare.crypto.base64_decode(b.data) end end end;return out").await;
        assert_eq!(data,vec![FIRST.to_owned(),SECOND.to_owned()],"both image references survive the short-prefix collision and restart with their own bytes");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn failed_image_staging_cannot_remove_a_concurrent_committed_message() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().faux(json!([{"text":"seeded"}])).build().await;
        expose(&h);
        h.prompt("seed",json!({})).await.unwrap();
        let state:String=eval(&h,r#"
            local entered_tx,entered_rx=mare.task.channel()
            local release_tx,release_rx=mare.task.channel()
            local done_tx,done_rx=mare.task.channel()
            local original=mare.fs.write
            mare.fs.write=function(path,bytes,opts)
                if path:find('%.attachments/') and bytes=='FAIL-A' then
                    entered_tx:send(true)
                    release_rx:recv()
                    return nil,'controlled image staging failure'
                end
                return original(path,bytes,opts)
            end
            local a={role='user',content={{type='text',text='failed A'},{type='image',mime='image/png',data=mare.crypto.base64_encode('FAIL-A')}}}
            local b={role='user',content={{type='text',text='committed B'}}}
            local preparing=mare.task.spawn(function()
                local ok, err = pcall(S.persist_message, S, {message=a,durable=true})
                done_tx:send({ok=ok, err=err})
            end)
            entered_rx:recv()
            local b_id=S:persist_message({message=b,durable=true})
            assert(type(b_id)=='string' and b_id~='','concurrent B must really commit')
            release_tx:send(true)
            local rejected=done_rx:recv()
            preparing:wait()
            mare.fs.write=original
            local a_count,b_count=0,0
            -- session entries hold detached copies: identify them by id and text, not identity
            for _,entry in ipairs(S:entries()) do
                if entry.type=='message' then
                    local text=entry.message.content[1].text
                    if text=='failed A' then a_count=a_count+1 end
                    if entry.entry_id==b_id and text=='committed B' then b_count=b_count+1 end
                end
            end
            return mare.json.encode({a_count=a_count,b_count=b_count,a_id=S:entry_id(a) or false,
                b_id=b_id,leaf=S:leaf(),error=rejected and not rejected.ok and (type(rejected.err)=='table' and rejected.err.message or tostring(rejected.err))})
        "#).await;
        let state:serde_json::Value=serde_json::from_str(&state).unwrap();
        assert_eq!(state["a_count"],0,"failed image A must leave no session-list entry: {state}");
        assert_eq!(state["b_count"],1,"A must never delete B's committed list tail: {state}");
        assert_eq!(state["a_id"],false,"failed A must not retain an entry identity: {state}");
        assert_eq!(state["leaf"],state["b_id"],"leaf and committed message index must agree: {state}");
        assert!(state["error"].is_string(),"{state}");
        h.shutdown().await;
        let h=env.host().resume().build().await;
        expose(&h);
        let history:String=eval(&h,"return mare.json.encode(A:list_messages().data)").await;
        assert!(history.contains("committed B") && !history.contains("failed A"),"journal and in-memory commit must agree after restart: {history}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn generic_operation_receipt_is_atomic_detached_and_restored_before_a_compaction_tail() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().build().await;
        expose(&h);
        let (replayed,queued,path):(bool,i64,String)=eval(&h,r#"
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
            A:prompt('cancelled image',{when_busy='queue',images={{type='image',mime='image/png',data='AQIDBA=='}}})
            local identity={request_id='operation-1',kind='test/clear',fingerprint=mare.crypto.sha256('{}')}
            local claim=assert(S:claim_operation(identity,mare.task.token()))
            assert(claim.status=='claimed')
            local canceled = A:cancel({ turn_id = held_turn.turn_id, clear_queue = true })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            local input=canceled.removed
            local result={ok=true,cancelled=mare.json.array(input)}
            local operation={request_id=identity.request_id,kind=identity.kind,fingerprint=identity.fingerprint,claim_id=claim.claim_id,result=result}
            assert(S:commit({version=1,messages={},inbox={steer={},follow_up={}},operation=operation},{}))
            local first=assert(S:claim_operation(identity,mare.task.token()))
            assert(first.status=='replayed' and first.receipt.result.cancelled[1].images[1].data=='AQIDBA==')
            first.receipt.result.cancelled[1].text='mutated receipt snapshot'
            local second=assert(S:claim_operation(identity,mare.task.token()))
            assert(second.receipt.result.cancelled[1].text=='cancelled image')
            local held_tx2, held_rx2 = mare.task.channel()
            local rel_tx2, rel_rx2 = mare.task.channel()
            C.cordis:get('policy'):voter('request/ready', function()
                if held_tx2 then
                    local tx = held_tx2; held_tx2 = nil
                    tx:send(true)
                    rel_rx2:recv()
                    return { decision = 'deny', reason = 'gate' }
                end
            end)
            local held_turn2 = A:prompt('hold2')
            held_rx2:recv()
            A:prompt('newer pending input', { when_busy = 'queue' })
            A:cancel({ turn_id = held_turn2.turn_id, clear_queue = false })
            rel_tx2:send(true)
            A:wait(held_turn2.turn_id)
            local conflict,err=S:claim_operation({request_id=identity.request_id,kind='different',fingerprint=identity.fingerprint},mare.task.token())
            assert(not conflict and err:find('conflict',1,true))
            assert(S:append_compaction('receipt recovery summary',nil,1,{}, {method='local'}))
            return second.status=='replayed',#A:list_queue().data,S:info().path
        "#).await;
        assert!(replayed);
        assert_eq!(queued,1,"replaying a result cannot clear a newer input");
        h.shutdown().await;
        let h=env.host().build().await;
        expose(&h);
        global(&h,"SESSION_PATH",path);
        let partial:bool=eval(&h,r#"
            local ro=mare.store.open(SESSION_PATH,{existing=true,defer_activation=true})
            local tail=mare.store.read_tail_snapshot(ro)
            ro:close()
            return tail.partial
        "#).await;
        assert!(partial,"the receipt must precede the actual partial-restore boundary");
        h.shutdown().await;
        let h=env.host().resume().build().await;
        expose(&h);
        let (text,data,queued):(String,String,i64)=eval(&h,r#"
            local replay=assert(S:claim_operation({request_id='operation-1',kind='test/clear',fingerprint=mare.crypto.sha256('{}')},mare.task.token()))
            assert(replay.status=='replayed')
            local input=replay.receipt.result.cancelled[1]
            return input.text,input.images[1].data,#A:list_queue().data
        "#).await;
        assert_eq!((text,data,queued),("cancelled image".into(),"AQIDBA==".into(),1));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn generic_operation_claim_coordinates_duplicates_and_caller_cancellation() {
    local(async {
        let env=TestEnv::new().await;
        let h=env.host().build().await;
        expose(&h);
        let (waited,retried):(bool,bool)=eval(&h,r#"
            local identity={request_id='duplicate-1',kind='test/operation',fingerprint=mare.crypto.sha256('input')}
            local leader_cancel=mare.task.token()
            local leader=assert(S:claim_operation(identity,leader_cancel))
            local started_tx,started_rx=mare.task.channel()
            local result_tx,result_rx=mare.task.channel()
            local duplicate=mare.task.spawn(function()
                started_tx:send(true)
                result_tx:send(table.pack(S:claim_operation(identity,mare.task.token())))
            end)
            started_rx:recv()
            local result={value='one committed result'}
            assert(S:commit({version=1,messages={},inbox={steer={},follow_up={}},operation={
                request_id=identity.request_id,kind=identity.kind,fingerprint=identity.fingerprint,claim_id=leader.claim_id,result=result,
            }},{}))
            local answer=result_rx:recv();duplicate:wait()
            assert(answer[1] and answer[1].status=='replayed')
            local failed={request_id='cancelled-claim',kind='test/operation',fingerprint=identity.fingerprint}
            local cancelled=mare.task.token()
            local pending=assert(S:claim_operation(failed,cancelled))
            cancelled:cancel()
            local accepted=S:commit({version=1,messages={},inbox={steer={},follow_up={}},operation={
                request_id=failed.request_id,kind=failed.kind,fingerprint=failed.fingerprint,claim_id=pending.claim_id,result={should_not_commit=true},
            }},{})
            assert(not accepted)
            S:release_operation(pending,'cancelled')
            local retry=assert(S:claim_operation(failed,mare.task.token()))
            S:release_operation(retry,'test complete')
            return answer[1].receipt.result.value=='one committed result',retry.status=='claimed'
        "#).await;
        assert!(waited && retried);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn pre_allocated_ids_are_not_leaves_until_committed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let report: serde_json::Value = eval::<String>(
            &h,
            r#"
            local sess = C.cordis:get("session")
            local ui = C.cordis:get("ui")
            local llm = C.cordis:get("llm")
            local interleaved_id
            llm:register_provider({
                provider = "prealloc-probe",
                is_configured = function() return true end,
                models = { { model_id = "m1", name = "m1", context_window = 128000, max_output = 4096 } },
                stream = function(req, sink)
                    sink({ type = "text_delta", delta = "hello " })
                    sink({ type = "text_delta", delta = "world" })
                    -- Insert another tree message before this turn commits so commit-time
                    -- parent binding sees the moved leaf rather than the pre-stream leaf.
                    local info = sess:info()
                    interleaved_id = assert(sess:persist_message({
                        message = {
                            role = "user",
                            content = { { type = "text", text = "interleaved" } },
                            timestamp = mare.task.now(),
                        },
                        session_id = info.session_id,
                        session_epoch = info.session_epoch,
                    }))
                    return {
                        role = "assistant",
                        content = { { type = "text", text = "hello world" } },
                        stop_reason = "stop",
                        usage = { input = 5, output = 2 },
                    }
                end,
            })
            llm:set_model("prealloc-probe/m1")

            local observed = {}
            C.cordis:on("ui/card", function(ev)
                if ev.title == "StreamNotice" then
                    observed.after_during_stream = ev.after_entry_id
                end
            end)
            C.cordis:on("item/started", function(ev)
                local item = ev and ev.item
                if item and item.type == "assistant_message" and not observed.pre_id then
                    local id = ev.item_id or item.id
                    observed.pre_id = id
                    observed.leaf_during_stream = sess:leaf()
                    observed.entry_id_during_stream = sess:entry_id(item.message)
                    local leaf_ok, leaf_err = pcall(sess.set_leaf, sess, id)
                    observed.set_leaf_pre_ok = leaf_ok == true
                    observed.set_leaf_pre_err = tostring(leaf_err)
                    for _, e in ipairs(sess:entries({ expand = false })) do
                        assert(e.entry_id ~= id, "pre_id must not be in entries during stream")
                    end
                    if ui and ui.card then
                        assert(ui:card({
                            title = "StreamNotice",
                            markdown = "during stream",
                            persist = true,
                        }))
                    end
                end
            end)

            local ok_empty, err_empty = pcall(sess.add_record, sess, "probe/bad", {}, { entry_id = "" })
            assert(not ok_empty and err_empty, "empty external id must fail")
            local ok_rec = assert(sess:add_record("probe/ext", { x = 1 }, { entry_id = "ext-rec-1", sync = true }))
            local ok_dup, err_dup = pcall(sess.add_record, sess, "probe/dup", { x = 2 }, { entry_id = "ext-rec-1" })
            assert(not ok_dup and err_dup, "duplicate external id must fail")

            local res = A:wait(A:prompt("hi").turn_id)
            assert(res and not res.error, tostring(res and res.error))
            local msgs = A:messages()
            local user_msg = msgs[#msgs - 1]
            local asst_msg = msgs[#msgs]
            observed.user_id = sess:entry_id(user_msg)
            observed.interleaved_id = interleaved_id
            observed.asst_id = sess:entry_id(asst_msg)
            observed.leaf_after_commit = sess:leaf()
            observed.path = sess:info().path
            return mare.json.encode(observed)
        "#,
        )
        .await
        .parse()
        .unwrap();
        h.shutdown().await;

        let pre_id = report["pre_id"].as_str().expect("pre_id present on message-start");
        assert!(!pre_id.is_empty());
        assert_eq!(
            report["leaf_during_stream"], report["user_id"],
            "leaf must stay on committed user message during stream: {report}"
        );
        assert_ne!(
            report["leaf_during_stream"], report["pre_id"],
            "pre-allocated id must never be leaf before commit: {report}"
        );
        assert!(report["entry_id_during_stream"].is_null(), "uncommitted stream_message has no entry_id: {report}");
        assert_eq!(report["set_leaf_pre_ok"], false, "uncommitted pre_id cannot become leaf: {report}");
        assert_eq!(
            report["after_during_stream"], report["user_id"],
            "ui:card during stream defaults after_entry_id to committed leaf, never pre_id: {report}"
        );
        assert_ne!(report["interleaved_id"], report["user_id"], "interleaved message created new leaf: {report}");
        assert_eq!(report["asst_id"], pre_id, "committed entry uses pre-allocated id: {report}");
        assert_eq!(report["leaf_after_commit"], pre_id);

        let entries = mare_store::read_entries(std::path::Path::new(report["path"].as_str().unwrap())).unwrap();
        let asst_entry = entries
            .iter()
            .find(|e| e["type"] == "message" && e["entry_id"] == pre_id)
            .expect("assistant entry persisted with pre_id");
        assert_eq!(
            asst_entry["parent_entry_id"], report["interleaved_id"],
            "parent bound to interleaved leaf at commit time, not pre-stream leaf"
        );
    })
    .await
}

#[tokio::test]
async fn a_retry_resets_the_id_and_an_aborted_empty_message_leaves_no_hole() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let report: serde_json::Value = eval::<String>(
            &h,
            r#"
            local sess = C.cordis:get("session")
            local llm = C.cordis:get("llm")
            local call = 0
            llm:register_provider({
                provider = "retry-id-probe",
                is_configured = function() return true end,
                models = { { model_id = "m1", name = "m1", context_window = 128000, max_output = 4096 } },
                stream = function(req, sink)
                    call = call + 1
                    if call == 1 then
                        sink({ type = "text_delta", delta = "partial-discarded" })
                        error({ message = "transient server error", retryable = true, kind = "server" })
                    elseif call == 2 then
                        sink({ type = "text_delta", delta = "recovered" })
                        return {
                            role = "assistant",
                            content = { { type = "text", text = "recovered" } },
                            stop_reason = "stop",
                            usage = { input = 5, output = 1 },
                        }
                    elseif call == 3 then
                        error({ message = "fatal empty error", retryable = false, kind = "fatal" })
                    else
                        sink({ type = "text_delta", delta = "after-empty" })
                        return {
                            role = "assistant",
                            content = { { type = "text", text = "after-empty" } },
                            stop_reason = "stop",
                            usage = { input = 5, output = 1 },
                        }
                    end
                end,
            })
            llm:set_model("retry-id-probe/m1")

            local start_ids = {}
            local asst_lifecycle = {}
            C.cordis:on("item/started", function(ev)
                local item = ev and ev.item
                if item and item.type == "assistant_message" then
                    local id = ev.item_id or item.id
                    start_ids[#start_ids + 1] = id
                    asst_lifecycle[#asst_lifecycle + 1] = "start:" .. tostring(id)
                end
            end)
            C.cordis:on("item/completed", function(ev)
                local item = ev and ev.item
                if item and item.type == "assistant_message" then
                    local id = ev.item_id or item.id
                    asst_lifecycle[#asst_lifecycle + 1] = "end:" .. tostring(id)
                end
            end)

            local r1 = A:wait(A:prompt("first").turn_id)
            assert(r1 and not r1.error, tostring(r1 and r1.error))
            local msgs1 = A:messages()
            local asst1_id = sess:entry_id(msgs1[#msgs1])

            local r2 = A:wait(A:prompt("fail-empty").turn_id)
            assert(r2 and r2.error, "empty failure returns error")
            local leaf_after_empty = sess:leaf()

            local r3 = A:wait(A:prompt("third").turn_id)
            assert(r3 and not r3.error, tostring(r3 and r3.error))

            return mare.json.encode({
                start_ids = start_ids,
                asst_lifecycle = asst_lifecycle,
                asst1_id = asst1_id,
                leaf_after_empty = leaf_after_empty,
                leaf_end = sess:leaf(),
                path = sess:info().path,
            })
        "#,
        )
        .await
        .parse()
        .unwrap();
        h.shutdown().await;

        let start_ids = report["start_ids"].as_array().unwrap();
        assert_eq!(start_ids.len(), 3, "expected 2 start_ids from retry + 1 from turn 3: {report}");
        let id1 = start_ids[0].as_str().unwrap();
        let id2 = start_ids[1].as_str().unwrap();
        assert!(!id1.is_empty() && !id2.is_empty());
        assert_ne!(id1, id2, "retry must reset pre-allocated id: {report}");
        assert_eq!(report["asst1_id"], id2, "committed entry keeps final attempt id: {report}");
        let lifecycle: Vec<&str> =
            report["asst_lifecycle"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            &lifecycle[..4],
            &[format!("start:{id1}"), format!("end:{id1}"), format!("start:{id2}"), format!("end:{id2}")],
            "failed streaming attempt must close with item/completed before retry start: {lifecycle:?}"
        );

        let entries = mare_store::read_entries(std::path::Path::new(report["path"].as_str().unwrap())).unwrap();
        let msg_entries: Vec<_> = entries.iter().filter(|e| e["type"] == "message").collect();
        // first user + first assistant + fail-empty user + third user + third assistant = 5 (no hole from failed empty assistant!)
        assert_eq!(msg_entries.len(), 5, "empty error must leave no hole in message entries: {msg_entries:?}");
        assert!(!entries.iter().any(|e| e["entry_id"] == id1), "discarded attempt 1 id must not appear on disk");
        assert_eq!(msg_entries[2]["entry_id"], report["leaf_after_empty"]);
        assert_eq!(
            msg_entries[3]["parent_entry_id"], msg_entries[2]["entry_id"],
            "turn 3 user parent chains directly after turn 2 user"
        );
        assert_eq!(msg_entries[4]["parent_entry_id"], msg_entries[3]["entry_id"]);
        assert_eq!(msg_entries[4]["entry_id"], report["leaf_end"]);
    })
    .await
}
