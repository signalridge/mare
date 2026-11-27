use super::*;

#[tokio::test]
async fn revision_events_coalesce_and_close_or_cancel_wakes_receivers() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let wire = mare_shared::wire_projection::hook(&lua).unwrap();
    assert_eq!(wire.revision(), 0);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    lua.globals().set("cancelled", crate::task::Token(token)).unwrap();
    lua.load(
        r#"
            local p=mare.projection local events=p.subscribe()
            local a=p.register('FAKE_watch_value_one') local b=p.register('FAKE_watch_value_two')
            assert(events:recv()==p.revision())
            a:close() assert(events:recv()==p.revision())
            assert(events:recv(cancelled)==nil)
            events:close() assert(events:recv()==nil)
        "#,
    )
    .exec_async()
    .await
    .unwrap();
    assert_eq!(wire.revision(), lua.load("return mare.projection.revision()").eval::<u64>().unwrap());
}

#[test]
fn a_separate_vm_cannot_restore_parent_references() {
    let parent = Lua::new();
    let mare = parent.create_table().unwrap();
    install(&parent, &mare).unwrap();
    parent.globals().set("mare", mare).unwrap();
    let reference: String =
        parent.load("keep=mare.projection.register('FAKE_parent_secret'); return keep.reference").eval().unwrap();
    let child = Lua::new();
    let mare = child.create_table().unwrap();
    install(&child, &mare).unwrap();
    assert!(restore_json(&child, serde_json::json!({"value":reference}), 0).is_err());
}

#[test]
fn projection_preserves_schema_ordering_metadata() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    let source: Value = lua
        .load(
            r#"return {type='object', ['x-mare-order']={'z','a'}, properties={a={type='string'}, z={type='string'}}}"#,
        )
        .eval()
        .unwrap();
    mare_shared::lua::mark_tool_schema(&lua, &source).unwrap();
    let projected = project(&lua, source).unwrap();
    let json = mare_shared::lua::lua_to_json(&lua, projected).unwrap();
    let keys: Vec<_> = json["properties"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["z", "a"]);
    assert!(json.get("x-mare-order").is_none());
}

#[tokio::test]
async fn exposed_codemode_callback_cannot_restore_a_reference_outside_approved_arguments() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = Lua::new();
            let mare = lua.create_table().unwrap();
            install(&lua, &mare).unwrap();
            crate::sandbox::local::install(&lua, &mare).unwrap();
            lua.globals().set("mare", mare).unwrap();
            let revision: u64 = lua
                .load(
                    r#"
            lease = mare.projection.register('FAKE_unapproved_other_secret')
            reference = lease.reference
            return mare.projection.revision()
        "#,
                )
                .eval()
                .unwrap();
            let context = lua.create_table().unwrap();
            context.set("approval_snapshot", "ordinary-tool-without-hidden-input").unwrap();
            context.set("projection_version", revision).unwrap();
            let callback = lua
                .load(
                    r#"return function()
            -- Build marker fragments in model code; there is no marker in the tool argument.
            local code = "assert(mare == nil); return restore_hidden('[[mare-' .. 'value:' .. " ..
                string.format('%q', reference:sub(14, -3)) .. " .. ']]', revision())"
            assert(not mare.projection.contains(code))
            local sandbox = mare.sandbox.new({expose={
                restore_hidden=mare.projection.restore, revision=mare.projection.revision
            }})
            local result, err = sandbox:run(code)
            assert(result == nil and tostring(err):find('PROJECTION_AUTHORIZATION', 1, true),
                'unapproved codemode callback recovered another registered value')
        end"#,
                )
                .eval::<mlua::Function>()
                .unwrap();
            mare_shared::exec_ctx::run_in(&lua, context, callback, mlua::MultiValue::new()).await.unwrap();
        })
        .await;
}

#[test]
fn projected_validation_uses_the_immutable_original_schema_and_rejects_foreign_markers() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    crate::codec::install(&lua, &mare).unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
            local p = mare.projection
            local raw = 'FAKE_hidden_"value\\with_escape'
            local lease = p.register(raw)
            local source = {type='object', required={'payload'}, properties={payload={type='string', enum={raw}}}}
            local schema = mare.schema.compile(source)
            local fingerprint = schema:fingerprint()
            source.properties.payload.enum[1] = 'tampered'
            local projected = p.project({payload=raw})
            assert(projected.payload == lease.reference)
            assert(schema:fingerprint() == fingerprint)
            assert(pcall(schema.validate_projected, schema, projected, p.revision()))
            assert(not pcall(schema.validate, schema, projected))
            assert(not pcall(schema.validate_projected, schema, {payload='tampered'}, p.revision()))
            assert(not pcall(schema.validate_projected, schema, {payload='[[mare-value:unknown]]'}, p.revision()))
            assert(not pcall(schema.validate_projected, schema, {payload='[[mare-value:unfinished'}, p.revision()))
            local previous = p.revision()
            lease:close()
            local ok, err = pcall(schema.validate_projected, schema, projected, previous)
            assert(not ok and tostring(err):find('PROJECTION_CHANGED', 1, true))
            assert(not pcall(schema.validate_projected, schema, projected, p.revision()))
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn native_authorization_is_exact_single_use_and_preserves_original_arguments() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let revision: u64 = lua
        .load(
            r#"
            lease = mare.projection.register('FAKE_approved_payload')
            safe = mare.projection.project({payload='FAKE_approved_payload'})
            other_lease = mare.projection.register('FAKE_other_unapproved_payload')
            other = mare.projection.project({payload='FAKE_other_unapproved_payload'})
            assert(not pcall(mare.projection.restore, safe, mare.projection.revision()))
            return mare.projection.revision()
        "#,
        )
        .eval()
        .unwrap();
    let context = lua.create_table().unwrap();
    context.set("approval_snapshot", "frozen-call-snapshot").unwrap();
    context.set("projection_version", revision).unwrap();
    let arguments = mare_shared::lua::strict_lua_to_json(&lua, lua.globals().get("safe").unwrap()).unwrap();
    context
        .set("projection_authorization", authorize(&lua, arguments, revision, "frozen-call-snapshot").unwrap())
        .unwrap();
    let callback = lua
        .load(
            r#"return function()
            local p = mare.projection
            local ok, err = pcall(p.restore, other, p.revision())
            assert(not ok and tostring(err):find('PROJECTION_AUTHORIZATION', 1, true))
            ok, err = pcall(p.restore, {payload=safe.payload, rewritten='extra'}, p.revision())
            assert(not ok and tostring(err):find('PROJECTION_AUTHORIZATION', 1, true))
            local restored = p.restore(safe, p.revision())
            assert(restored.payload == 'FAKE_approved_payload')
            assert(safe.payload == lease.reference)
            ok, err = pcall(p.restore, safe, p.revision())
            assert(not ok and tostring(err):find('already consumed', 1, true))
            assert(not pcall(p.restore, '[[mare-value:unknown]]', p.revision()))
            lease:close()
            local ok, err = pcall(p.restore, safe, p.revision())
            assert(not ok and tostring(err):find('PROJECTION_AUTHORIZATION', 1, true))
        end"#,
        )
        .eval::<mlua::Function>()
        .unwrap();
    let thread = lua.create_thread(callback).unwrap();
    mare_shared::exec_ctx::bind(&lua, &thread, Some(context)).unwrap();
    thread.resume::<()>(()).unwrap();
}

#[test]
fn lease_projection_scope_and_revision() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
            local p = mare.projection
            local a = p.register('FAKE_secret_one')
            local b = p.register('FAKE_secret_one_long')
            local rev = p.revision()
            local original = {text='FAKE_secret_one_long FAKE_secret_one', n=12}
            local mt={} setmetatable(original,mt)
            local safe = p.project(original)
            assert(getmetatable(safe)==mt)
            assert(safe.text == b.reference .. ' ' .. a.reference)
            assert(original.text == 'FAKE_secret_one_long FAKE_secret_one')
            assert(p.project(safe).text == safe.text)
            assert(not pcall(p.restore,safe,rev)) -- no plaintext getter outside approved execution
            assert(not tostring(a):find('FAKE'))
            assert(not pcall(p.project, {FAKE_secret_one='key collision'}))
            local cycle={} cycle.self=cycle assert(not pcall(p.project,cycle))
            assert(not pcall(p.register,''))
            local short=p.register('ZZZ'); assert(p.project('ZZZ')==short.reference); short:close()
            assert(not pcall(p.register,'mare-value'))
            assert(not pcall(p.register,a.reference:sub(15,25)))
            assert(not p.project('[[mare-value:FAKE_secret_one]]'):find('FAKE_secret_one',1,true))
            assert(not pcall(p.register,'FAKE_secret_one'))
            assert(not pcall(p.restore, '[[mare-value:unknown]]', rev))
            a:close() a:close()
            assert(not pcall(p.restore,safe,rev))
            assert(not pcall(p.restore,a.reference,p.revision()))
            local d=p.register('FAKE_gc_secret') local old=d.reference d=nil collectgarbage('collect')
            assert(not pcall(p.restore,old,p.revision()))
            local deep={} local tail=deep for i=1,66 do tail.child={} tail=tail.child end
            assert(not pcall(p.project,deep))
            assert(not pcall(p.contains,deep))
            assert(p.project('FAKE_secret_one') == a.reference)
            local c=p.register('FAKE_secret_one')
            assert(c.reference ~= a.reference)
            assert(p.project('FAKE_secret_one') == c.reference)
            assert(not pcall(p.restore,a.reference,p.revision()))
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn project_lossy_bounded_stops_copying_at_node_budget_and_replaces_non_utf8_values() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let big = lua.create_table().unwrap();
    for i in 1..=200_000 {
        big.raw_set(i, i).unwrap();
    }
    lua.gc_collect().unwrap();
    lua.gc_stop();
    let before = lua.used_memory();
    let err = project_lossy_bounded(&lua, Value::Table(big), 1_000).unwrap_err().to_string();
    let grown = lua.used_memory().saturating_sub(before);
    assert!(err.contains("exceeds resource limit"), "{err}");
    assert!(grown < 64 * 1024, "projection copied {grown} bytes before enforcing node budget");
    lua.gc_restart();

    let (projected_ok, lossy_ok): (Table, bool) = lua
        .load(
            r#"
                local lease = mare.projection.register("SECRET_VAL")
                local tbl = { msg = "prefix SECRET_VAL \xff suffix" }
                local out, lossy = mare.projection.project(tbl, { lossy = true })
                assert(out.msg:find(lease.reference, 1, true), "secret must still be projected: " .. out.msg)
                assert(not out.msg:find("SECRET_VAL", 1, true), "secret leaked: " .. out.msg)
                assert(out.msg:find("\u{FFFD}", 1, true), "expected U+FFFD replacement: " .. out.msg)
                return out, lossy
                "#,
        )
        .eval()
        .unwrap();
    assert!(lossy_ok);
    assert!(projected_ok.get::<String>("msg").unwrap().contains('\u{FFFD}'));

    let bad_key = lua.create_table().unwrap();
    bad_key.set(lua.create_string(b"bad\xffkey").unwrap(), "v").unwrap();
    let key_err = project_lossy_bounded(&lua, Value::Table(bad_key), 100).unwrap_err().to_string();
    assert!(key_err.contains("expected UTF-8 key"), "{key_err}");
}

// A registered value is still hidden when it sits inside a non-UTF-8 string whose bytes are
// kept (session memory), and an unchanged table still comes back as a fresh copy.
#[test]
fn preserve_invalid_keeps_bytes_but_projects_registered_values_and_always_copies() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
            local plain = { nested = { v = "x\xff" } }
            local copy = mare.projection.project(plain, { preserve_invalid = true })
            assert(copy ~= plain and copy.nested ~= plain.nested, "must be a detached copy")
            assert(copy.nested.v == "x\xff", "bytes kept without registrations")
            local lease = mare.projection.register("SECRET_VAL")
            local out = mare.projection.project({ msg = "a\xffSECRET_VAL\xe4\xb8" }, { preserve_invalid = true })
            assert(out.msg == "a\xff" .. lease.reference .. "\xe4\xb8", "got " .. out.msg)
            assert(not pcall(mare.projection.project, {}, { lossy = true, preserve_invalid = true }))
            "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn byte_stream_handles_every_split_including_utf8_and_invalid_bytes() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
            KEEP_A = mare.projection.register("FAKE_秘密_secret")
            KEEP_B = mare.projection.register("FAKE_秘密_secret_long")
            "#,
    )
    .exec()
    .unwrap();
    let proj = mare_shared::wire_projection::hook(&lua).unwrap();
    let input: Vec<u8> =
        b"prefix FAKE_\xe7\xa7\x98\xe5\xaf\x86_secret_long \xff mid FAKE_\xe7\xa7\x98\xe5\xaf\x86_secret tail".to_vec();
    let expected = proj.project_bytes(&input);
    assert!(!String::from_utf8_lossy(&expected).contains("FAKE_"));
    assert!(expected.contains(&0xff));

    for split in 0..=input.len() {
        let mut stream = proj.byte_stream();
        let mut out = stream.push(&input[..split]);
        out.extend_from_slice(&stream.push(&input[split..]));
        out.extend_from_slice(&stream.finish());
        assert_eq!(out, expected, "split={split}");
        assert!(stream.finish().is_empty());
    }

    // Byte-by-byte streaming across every UTF-8 byte boundary also matches whole-buffer output.
    let mut per_byte = proj.byte_stream();
    let mut out = Vec::new();
    for b in &input {
        out.extend_from_slice(&per_byte.push(std::slice::from_ref(b)));
    }
    out.extend_from_slice(&per_byte.finish());
    assert_eq!(out, expected);
}

#[test]
fn wire_projector_checks_sensitive_keys_and_preserves_envelope_keys_in_json_fallback() {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let reference: String =
        lua.load(r#"K_LEASE = mare.projection.register("turn_id"); return K_LEASE.reference"#).eval().unwrap();
    let proj = mare_shared::wire_projection::active_projector(&lua).unwrap();
    assert!(proj.check_json_keys(&serde_json::json!({ "safe": "turn_id" })).is_ok());
    assert!(proj.check_json_keys(&serde_json::json!({ "k-turn_id": "v" })).is_err());

    let mut obj = serde_json::json!({ "turn_id": "val-turn_id" });
    proj.project_json(&mut obj);
    assert_eq!(obj["turn_id"], format!("val-{reference}"));
}

#[test]
fn json_projector_and_byte_stream_cover_edge_branches() {
    use mare_shared::wire_projection::ByteStreamProjector as _;

    let bare = Lua::new();
    let empty_proj = JsonProjector::from_lua(&bare).unwrap();
    let mut noop = serde_json::json!({ "a": [1, "x"] });
    empty_proj.project_json_checked(&mut noop).unwrap();
    assert_eq!(noop["a"][1], "x");
    assert!(restore_json(&bare, noop.clone(), 0).is_err());

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    assert!(JsonProjector::from_lua(&lua).unwrap().check_json_keys(&noop).is_ok());

    let mut raw_stream = stream::ByteStream::new(Arc::clone(&state(&lua).0));
    assert_eq!(raw_stream.push(b"plain"), b"plain");
    assert!(raw_stream.finish().is_empty());

    let reference: String = lua
        .load("KEEP_LEASE = mare.projection.register('SECRET_EDGE_VALUE_123'); return KEEP_LEASE.reference")
        .eval()
        .unwrap();
    let active_proj = JsonProjector::from_lua(&lua).unwrap();
    let mut nested = serde_json::json!({ "arr": [{ "s": "hi SECRET_EDGE_VALUE_123" }, 42] });
    active_proj.project_json_checked(&mut nested).unwrap();
    assert_eq!(nested["arr"][0]["s"], format!("hi {reference}"));
    let mut bad = serde_json::json!({ "arr": [{ "bad-SECRET_EDGE_VALUE_123": "v" }] });
    assert!(active_proj.project_json_checked(&mut bad).is_err());

    let restored = restore_json(&lua, nested, 1).unwrap();
    assert_eq!(restored["arr"][0]["s"], "hi SECRET_EDGE_VALUE_123");
    assert!(restore_json(&lua, serde_json::json!({ "k-SECRET_EDGE_VALUE_123": 1 }), 1).is_err());
    let mut deep = serde_json::json!(1);
    for _ in 0..66 {
        deep = serde_json::json!([deep]);
    }
    assert!(restore_json(&lua, deep, 1).is_err());

    let mut bs = stream::ByteStream::new(Arc::clone(&state(&lua).0));
    // Split 3-byte UTF-8 character across chunks so `truncated_utf8` holds the prefix until completion,
    // and exercise `local_cut` backing up off a multi-byte character boundary plus trailing `pos >= raw_limit`.
    let mut stream_out = bs.push(b"ok-\xe4\xb8");
    stream_out.extend_from_slice(&bs.push(b"\xad-tail"));
    stream_out.extend_from_slice(&bs.finish());
    assert_eq!(String::from_utf8(stream_out).unwrap(), "ok-中-tail");

    let mut bs_boundary = stream::ByteStream::new(Arc::clone(&state(&lua).0));
    let mut part = bs_boundary.push(b"\xff\xe4\xb8\xad12345678901234567890");
    part.extend_from_slice(&bs_boundary.finish());
    assert_eq!(part, b"\xff\xe4\xb8\xad12345678901234567890");
}
