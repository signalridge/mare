//! Public output-backed image materialization, budgets and lifecycle.
use crate::common::*;

#[tokio::test]
async fn artifact_image_loads_bytes_saved_by_spill_service() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["spill"]).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local spill = C.cordis:get("spill")
            local owner = S:info().session_id
            local bytes = "\0\255image from spill service"
            local saved = {}
            local backend = { identity = "image-test" }
            function backend.save_bytes(session_id, content)
                assert(session_id == owner)
                saved["image"] = content
                return { spill_id = "image", size = #content }
            end
            function backend.read_bytes(spill_id, opts)
                assert(opts and opts.session_id == owner)
                local value = assert(saved[spill_id])
                return { bytes = value, size = #value, more = false }
            end
            function backend.save() error("unused") end
            function backend.read() error("unused") end
            function backend.delete(spill_id) saved[spill_id] = nil end
            function backend.dispose() end
            KEEP_IMAGE_BACKEND = spill:register("image_test", backend)
            local artifact = spill:save_bytes(owner, bytes, { backend = "image_test" })
            assert(type(artifact.spill_id) == "string" and artifact.handle == nil)
            artifact.session_id, artifact.sha256 = owner, mare.crypto.sha256(bytes)
            artifact.width, artifact.height = 4, 2
            IMAGE_FROM_SPILL = C.cordis:get("ui"):artifact_image(artifact).resource
            "#,
        )
        .await;
        let resource: mlua::Table = h.lua().globals().get("IMAGE_FROM_SPILL").unwrap();
        let key: String = resource.get("key").unwrap();
        let parsed: mare_protocol::OutputImageKey = serde_json::from_str(&key)
            .unwrap_or_else(|e| panic!("artifact_image key must match protocol OutputImageKey: {e}: {key}"));
        assert!(!parsed.session_id.is_empty() && !parsed.session_epoch.is_empty() && !parsed.spill_id.is_empty());
        assert_eq!(parsed.sha256.len(), 64);
        let reference =
            mare_shared::image_resource::Reference::new(resource.get("resolver_id").unwrap(), key, 4, 2).unwrap();
        let bytes = tokio::task::spawn_blocking(move || reference.load()).await.unwrap().unwrap();
        assert_eq!(&*bytes, b"\0\xffimage from spill service");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn image_resources_resolve_readonly_on_demand_and_revoke_native_and_wire_identity() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            LOADS=0
            CLOSE_IMAGE=ui.image_resolver("resource-test",function(key)
                assert(key=="owner/generation/opaque")
                assert(not pcall(function() mare.exec.check_mutation("resource callback") end))
                assert(not pcall(CLOSE_IMAGE),"readonly loader revoked host resource")
                LOADS=LOADS+1
                return "real bytes"
            end)
            RESOURCE=ui.image_resource("resource-test","owner/generation/opaque",32,16)
            local image=ui.image({resource=RESOURCE})
            local wire=ui.node_to_wire(image)
            assert(wire.resource.resolver_id==nil and wire.data==nil)
            assert(ui.node_from_wire(wire).resource.resolver_id==RESOURCE.resolver_id)
            wire.resource.resolver_id=RESOURCE.resolver_id
            assert(not pcall(ui.node_from_wire,wire),"wire accepted native resource authority")
            assert(LOADS==0,"descriptor or wire did IO")
        "#,
        )
        .await;
        let resource: mlua::Table = h.lua().globals().get("RESOURCE").unwrap();
        let reference = mare_shared::image_resource::Reference::new(
            resource.get("resolver_id").unwrap(),
            resource.get("key").unwrap(),
            32,
            16,
        )
        .unwrap();
        let work = reference.clone();
        assert_eq!(&*tokio::task::spawn_blocking(move || work.load()).await.unwrap().unwrap(), b"real bytes");
        let work = reference.clone();
        assert_eq!(&*tokio::task::spawn_blocking(move || work.load()).await.unwrap().unwrap(), b"real bytes");
        exec(&h, "assert(LOADS==1); CLOSE_IMAGE(); assert(not pcall(ui.image_resource,'resource-test','old',32,16))")
            .await;
        assert!(reference.load().is_err());
        exec(
            &h,
            r#"
            SLOW_ENTERED_TX, SLOW_ENTERED_RX = mare.task.channel()
            SLOW_WAIT_TX, SLOW_WAIT_RX = mare.task.channel()
            SLOW_LATE = false
            CLOSE_SLOW=ui.image_resolver("resource-slow",function()
                SLOW_ENTERED_TX:send(true);SLOW_WAIT_RX:recv();SLOW_LATE=true;return "late"
            end)
            SLOW_RESOURCE=ui.image_resource("resource-slow","owner/generation/slow",32,16)
        "#,
        )
        .await;
        let resource: mlua::Table = h.lua().globals().get("SLOW_RESOURCE").unwrap();
        let slow = mare_shared::image_resource::Reference::new(
            resource.get("resolver_id").unwrap(),
            resource.get("key").unwrap(),
            32,
            16,
        )
        .unwrap();
        let pending = tokio::task::spawn_blocking(move || slow.load());
        exec(
            &h,
            r#"
            assert(SLOW_ENTERED_RX:recv())
            CLOSE_SLOW();SLOW_WAIT_TX:send(true);assert(not SLOW_LATE)
        "#,
        )
        .await;
        assert!(tokio::time::timeout(std::time::Duration::from_secs(1), pending).await.unwrap().unwrap().is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn output_resource_pressure_evicts_and_rematerializes_without_history_bytes() {
    local(async {
        let env=TestEnv::new().await;
        // the test provides its own `spill` reader below: with one service slot the built-in
        // spill plugin is turned off so the slot is free, and the provider is a user plugin (the root
        // ctx is kernel-held: its provide never reaches the cordis registry)
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("test-spill.lua"),
            r#"return { name = "test-spill", provide = { "spill" }, apply = function(ctx)
  -- a lightweight spill reader so artifact_image exercises its spill resolver
  require("mare_lib").service(ctx, "spill", {
    register = function() return function() end end,
    list_backends = function() return { data = {} } end,
    save = function() error("unused") end,
    read = function() error("unused") end,
    delete = function() return {} end,
    save_owned = function() error("unused") end,
    read_owned = function() error("unused") end,
    delete_owned = function() return false end,
    save_bytes = function() error("unused") end,
    read_bytes = function(_, _, session_id, spill_id, opts)
      local bytes = assert((mare._backing or {})[spill_id])
      return { bytes = bytes, size = #bytes }
    end,
  })
end }"#,
        )
        .unwrap();
        let h=env.host().user_plugins().config(serde_json::json!({ "plugins": { "spill": { "enabled": false } } })).no_session().build().await;
        expose(&h);
        exec(&h,r#"
            local owner=C.cordis:get("session"):info()
            REFS,READS,BACKING={}, {}, {}
            local png=mare.crypto.base64_decode("iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAJElEQVR4nGOQ23Lp/0BihlEHjDpg1AGjDhh1wKgDRh0w0A4AALXQRkwBCanJAAAAAElFTkSuQmCC")
            local content=png..string.rep("\0",6*1024*1024-#png)
            CLOSE_PRESSURE=ui.image_resolver("pressure",function(key)
                local ref=mare.json.decode(key)
                assert(C.cordis:get("session"):info().session_id==ref.session_id and C.cordis:get("session"):info().session_epoch==ref.session_epoch)
                READS[key]=(READS[key] or 0)+1
                local bytes=assert(BACKING[ref.spill_id],"missing backing bytes")
                assert(mare.crypto.sha256(bytes)==ref.sha256)
                return bytes
            end)
            for index=1,12 do
                local spill_id="h-"..index
                BACKING[spill_id]=content
                local key=mare.json.encode({session_id=owner.session_id,session_epoch=owner.session_epoch,spill_id=spill_id,sha256=mare.crypto.sha256(content)})
                REFS[index]=ui.image_resource("pressure",key,32,16)
                local wire=ui.node_to_wire(ui.image({resource=REFS[index]}))
                assert(wire.data==nil and wire.resource.resolver_id==nil)
            end
            content=nil
            assert(next(READS)==nil,"reference creation performed storage IO")
        "#).await;
        let refs:mlua::Table=h.lua().globals().get("REFS").unwrap();
        let mut references=vec![];
        for value in refs.sequence_values::<mlua::Table>() {
            let value=value.unwrap();
            references.push(mare_shared::image_resource::Reference::new(value.get("resolver_id").unwrap(),value.get("key").unwrap(),32,16).unwrap());
        }
        for reference in &references {
            let reference=reference.clone();
            assert_eq!(tokio::task::spawn_blocking(move||reference.load()).await.unwrap().unwrap().len(),6*1024*1024);
            exec(&h,"local info=ui.image_cache_info();assert(info.materialized_bytes<=info.budget_bytes and info.entries<=64)").await;
        }
        let first=references[0].clone();
        assert_eq!(tokio::task::spawn_blocking(move||first.load()).await.unwrap().unwrap().len(),6*1024*1024);
        exec(&h,"assert(READS[REFS[1].key]==2,'evicted bytes were not read again'); CLOSE_PRESSURE()").await;
        assert!(references[0].load().is_err());
        exec(&h,r#"
            -- Provide a lightweight spill reader so artifact_image exercises its spill resolver.
            -- the test's spill reader (plugin test-spill, below) reads these bytes
            mare._backing = BACKING
            local ref=mare.json.decode(REFS[1].key)
            DEFAULT_IMAGE=C.cordis:get("ui"):artifact_image({session_id=ref.session_id,spill_id=ref.spill_id,sha256=ref.sha256,width=32,height=16},
                {fallback=ui.Code("complete source","text")})
            DEFAULT_RESOURCE=DEFAULT_IMAGE.resource
            assert(not pcall(function() C.cordis:get("ui"):artifact_image({session_id="foreign",spill_id=ref.spill_id,sha256=ref.sha256,width=32,height=16}) end))
        "#).await;
        let value:mlua::Table=h.lua().globals().get("DEFAULT_RESOURCE").unwrap();
        let old=mare_shared::image_resource::Reference::new(value.get("resolver_id").unwrap(),value.get("key").unwrap(),32,16).unwrap();
        let work=old.clone();
        assert_eq!(tokio::task::spawn_blocking(move||work.load()).await.unwrap().unwrap().len(),6*1024*1024);
        exec(&h,r#"
            local ch_tx, ch_rx = mare.task.channel()
            local unsub = C.cordis:on("session/changed", function() ch_tx:send(true) end)
            C.cordis:get("session"):create()
            assert(ch_rx:recv())
            unsub()
        "#).await;
        assert!(old.load().is_err(),"cached bytes survived a storage owner generation change");
        h.shutdown().await;
    }).await
}
