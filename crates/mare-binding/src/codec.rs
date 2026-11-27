//! `mare.json`, `mare.crypto`.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use mlua::{Lua, LuaSerdeExt, MultiValue, Table, UserData, UserDataMethods, Value};
use sha2::{Digest, Sha256};

use mare_shared::lua::{json_to_lua, json_to_lua_lossless, lua_to_json, nil_err, rt_err, strict_lua_to_json};

pub fn random_hex(n: usize) -> String {
    let mut v = vec![0u8; n];
    rand::fill(&mut v[..]);
    v.iter().map(|b| format!("{b:02x}")).collect()
}

// Parse JSON while rejecting duplicate object members (after JSON unescaping) at every depth.
// Objects and arrays are walked directly so `{"$serde_json::private::Number":"1"}` stays an
// object instead of triggering serde_json/arbitrary_precision's internal number-map token, while
// genuine JSON numbers still use serde_json's arbitrary-precision scalar parser.
// Match serde_json's default recursion limit: 127 nested containers pass, 128th fails.
const MAX_JSON_DEPTH: usize = 127;

fn skip_json_ws(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_scalar<T: serde::de::DeserializeOwned>(bytes: &[u8], pos: &mut usize) -> Result<T, String> {
    let mut stream = serde_json::Deserializer::from_slice(&bytes[*pos..]).into_iter::<T>();
    let value = match stream.next() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(e.to_string()),
        None => return Err("EOF while parsing a value".to_string()),
    };
    *pos += stream.byte_offset();
    Ok(value)
}

fn parse_unique_value(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<serde_json::Value, String> {
    skip_json_ws(bytes, pos);
    let Some(&byte) = bytes.get(*pos) else {
        return Err("EOF while parsing a value".to_string());
    };
    match byte {
        b'{' => {
            if depth >= MAX_JSON_DEPTH {
                return Err("recursion limit exceeded".to_string());
            }
            *pos += 1;
            skip_json_ws(bytes, pos);
            let mut values = serde_json::Map::new();
            if bytes.get(*pos) == Some(&b'}') {
                *pos += 1;
                return Ok(serde_json::Value::Object(values));
            }
            loop {
                skip_json_ws(bytes, pos);
                if bytes.get(*pos) != Some(&b'"') {
                    return Err("expected JSON object key string".to_string());
                }
                let key: String = parse_scalar(bytes, pos)?;
                if values.contains_key(&key) {
                    return Err(format!("duplicate JSON member: {key}"));
                }
                skip_json_ws(bytes, pos);
                if bytes.get(*pos) != Some(&b':') {
                    return Err("expected ':' after JSON object key".to_string());
                }
                *pos += 1;
                let value = parse_unique_value(bytes, pos, depth + 1)?;
                values.insert(key, value);
                skip_json_ws(bytes, pos);
                match bytes.get(*pos) {
                    Some(b',') => *pos += 1,
                    Some(b'}') => {
                        *pos += 1;
                        return Ok(serde_json::Value::Object(values));
                    }
                    _ => return Err("expected ',' or '}' in JSON object".to_string()),
                }
            }
        }
        b'[' => {
            if depth >= MAX_JSON_DEPTH {
                return Err("recursion limit exceeded".to_string());
            }
            *pos += 1;
            skip_json_ws(bytes, pos);
            let mut values = Vec::new();
            if bytes.get(*pos) == Some(&b']') {
                *pos += 1;
                return Ok(serde_json::Value::Array(values));
            }
            loop {
                values.push(parse_unique_value(bytes, pos, depth + 1)?);
                skip_json_ws(bytes, pos);
                match bytes.get(*pos) {
                    Some(b',') => *pos += 1,
                    Some(b']') => {
                        *pos += 1;
                        return Ok(serde_json::Value::Array(values));
                    }
                    _ => return Err("expected ',' or ']' in JSON array".to_string()),
                }
            }
        }
        _ => parse_scalar(bytes, pos),
    }
}

fn parse_unique_json(bytes: &[u8]) -> Result<serde_json::Value, String> {
    let mut pos = 0;
    let value = parse_unique_value(bytes, &mut pos, 0)?;
    skip_json_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err("trailing characters after JSON value".to_string());
    }
    Ok(value)
}

struct OutputSchema {
    schema: mare_shared::schema::OutputSchema,
    wire: serde_json::Value,
}

impl UserData for OutputSchema {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("value", |lua, this, ()| {
            let value = json_to_lua_lossless(lua, &this.wire)?;
            mare_shared::lua::mark_tool_schema(lua, &value)?;
            Ok(value)
        });
        m.add_method("validate", |lua, this, value: Value| {
            let value = strict_lua_to_json(lua, value)?;
            this.schema.validate(&value).map_err(rt_err)
        });
        m.add_method("validate_text", |lua, this, text: String| {
            let parsed = this.schema.validate_json_text(&text).map_err(rt_err)?;
            json_to_lua_lossless(lua, &parsed)
        });
    }
}

struct ToolSchema {
    schema: mare_shared::schema::Schema,
    wire: serde_json::Value,
}

impl UserData for ToolSchema {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("value", |lua, this, ()| {
            let value = json_to_lua_lossless(lua, &this.wire)?;
            mare_shared::lua::mark_tool_schema(lua, &value)?;
            Ok(value)
        });
        m.add_method("validate", |lua, this, value: Value| {
            let value = strict_lua_to_json(lua, value)?;
            this.schema.validate(&value).map_err(rt_err)
        });
        m.add_method("validate_projected", |lua, this, (value, revision): (Value, u64)| {
            let value = strict_lua_to_json(lua, value)?;
            let value = crate::projection::restore_json(lua, value, revision)?;
            this.schema.validate(&value).map_err(rt_err)
        });
        m.add_method("fingerprint", |_, this, ()| {
            fn sort(v: &mut serde_json::Value) {
                match v {
                    serde_json::Value::Object(m) => {
                        m.sort_keys();
                        for value in m.values_mut() {
                            sort(value);
                        }
                    }
                    serde_json::Value::Array(a) => {
                        for value in a {
                            sort(value);
                        }
                    }
                    _ => {}
                }
            }
            let mut value = this.wire.clone();
            sort(&mut value);
            serde_json::to_string(&value).map_err(rt_err)
        });
    }
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(|lua, (v, opts): (Value, Option<Table>)| {
            let j = lua_to_json(lua, v)?;
            let pretty = opts.and_then(|o| o.get::<Option<bool>>("pretty").ok().flatten()).unwrap_or(false);
            if pretty { serde_json::to_string_pretty(&j) } else { serde_json::to_string(&j) }.map_err(rt_err)
        })?,
    )?;
    // decode(s) -> value | nil, err
    json.set(
        "decode",
        lua.create_function(|lua, s: mlua::LuaString| {
            match serde_json::from_slice::<serde_json::Value>(&s.as_bytes()) {
                Ok(v) => Ok(MultiValue::from_vec(vec![json_to_lua(lua, &v)?])),
                Err(e) => nil_err(lua, e),
            }
        })?,
    )?;
    json.set(
        "decode_unique",
        lua.create_function(|lua, s: mlua::LuaString| match parse_unique_json(&s.as_bytes()) {
            Ok(v) => Ok(MultiValue::from_vec(vec![json_to_lua(lua, &v)?])),
            Err(e) => nil_err(lua, e),
        })?,
    )?;
    json.set(
        "decode_lossless",
        lua.create_function(|lua, s: mlua::LuaString| {
            match serde_json::from_slice::<serde_json::Value>(&s.as_bytes()) {
                Ok(v) => Ok(MultiValue::from_vec(vec![json_to_lua_lossless(lua, &v)?])),
                Err(e) => nil_err(lua, e),
            }
        })?,
    )?;
    json.set(
        "encode_strict",
        lua.create_function(|lua, v: Value| serde_json::to_string(&strict_lua_to_json(lua, v)?).map_err(rt_err))?,
    )?;
    json.set(
        "clone",
        lua.create_function(|lua, v: Value| {
            let json = strict_lua_to_json(lua, v)?;
            json_to_lua_lossless(lua, &json)
        })?,
    )?;
    json.set(
        "copy_metadata",
        lua.create_function(|lua, (source, target): (Value, Value)| {
            mare_shared::lua::copy_json_metadata(lua, &source, &target)
        })?,
    )?;
    // mark a table to encode as a JSON array (even when empty)
    json.set(
        "array",
        lua.create_function(|lua, t: Option<Table>| {
            let t = match t {
                Some(t) => t,
                None => lua.create_table()?,
            };
            t.set_metatable(Some(lua.array_metatable()))?;
            Ok(t)
        })?,
    )?;
    json.set(
        "is_array",
        lua.create_function(|lua, t: Table| Ok(t.metatable().is_some_and(|mt| mt == lua.array_metatable())))?,
    )?;
    json.set("null", lua.null())?;
    mare.set("json", json)?;

    let schemas = lua.create_table()?;
    schemas.set(
        "compile",
        lua.create_function(|lua, value: Value| {
            let wire = strict_lua_to_json(lua, value)?;
            let schema = mare_shared::schema::Schema::compile(wire.clone()).map_err(rt_err)?;
            lua.create_userdata(ToolSchema { schema, wire })
        })?,
    )?;
    schemas.set(
        "compile_output",
        lua.create_function(|lua, value: Value| {
            if let Value::UserData(ud) = &value
                && let Ok(existing) = ud.borrow::<OutputSchema>()
            {
                return lua
                    .create_userdata(OutputSchema { schema: existing.schema.clone(), wire: existing.wire.clone() });
            }
            let wire = strict_lua_to_json(lua, value)?;
            let schema = mare_shared::schema::OutputSchema::compile(wire.clone()).map_err(rt_err)?;
            lua.create_userdata(OutputSchema { schema, wire })
        })?,
    )?;
    schemas.set(
        "compile_value",
        lua.create_function(|lua, value: Value| {
            let wire = strict_lua_to_json(lua, value)?;
            let schema = mare_shared::schema::Schema::compile_value(wire.clone()).map_err(rt_err)?;
            lua.create_userdata(ToolSchema { schema, wire })
        })?,
    )?;
    schemas.set(
        "redact",
        lua.create_function(|lua, (schema, value): (Value, Value)| {
            let schema = strict_lua_to_json(lua, schema)?;
            let value = if value.is_nil() { None } else { Some(strict_lua_to_json(lua, value)?) };
            let result = mare_shared::schema_redaction::redact(&schema, value).map_err(rt_err)?;
            json_to_lua_lossless(lua, &result.into_json())
        })?,
    )?;
    mare.set("schema", schemas)?;

    let c = lua.create_table()?;
    c.set(
        "sha256",
        lua.create_function(|_, s: mlua::LuaString| {
            let d = Sha256::digest(&*s.as_bytes());
            Ok(d.iter().map(|b| format!("{b:02x}")).collect::<String>())
        })?,
    )?;
    c.set(
        "random_bytes",
        lua.create_function(|lua, n: usize| {
            let mut v = vec![0u8; n];
            rand::fill(&mut v[..]);
            lua.create_string(&v)
        })?,
    )?;
    c.set("random_hex", lua.create_function(|_, n: usize| Ok(random_hex(n)))?)?;
    c.set("uuid", lua.create_function(|_, ()| Ok(uuid::Uuid::now_v7().to_string()))?)?;
    c.set("base64_encode", lua.create_function(|_, s: mlua::LuaString| Ok(STANDARD.encode(&*s.as_bytes())))?)?;
    c.set(
        "base64_decode",
        lua.create_function(|lua, s: String| match STANDARD.decode(s.trim()) {
            Ok(b) => Ok(Some(lua.create_string(&b)?)),
            Err(_) => Ok(None),
        })?,
    )?;
    c.set(
        "base64url_encode",
        lua.create_function(|_, s: mlua::LuaString| Ok(URL_SAFE_NO_PAD.encode(&*s.as_bytes())))?,
    )?;
    c.set(
        "base64url_decode",
        lua.create_function(|lua, s: String| match URL_SAFE_NO_PAD.decode(s.trim().trim_end_matches('=')) {
            Ok(b) => Ok(Some(lua.create_string(&b)?)),
            Err(_) => Ok(None),
        })?,
    )?;
    // pkce() -> {verifier, challenge}
    c.set(
        "pkce",
        lua.create_function(|lua, ()| {
            let mut v = [0u8; 32];
            rand::fill(&mut v[..]);
            let verifier = URL_SAFE_NO_PAD.encode(v);
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let t = lua.create_table()?;
            t.set("verifier", verifier)?;
            t.set("challenge", challenge)?;
            Ok(t)
        })?,
    )?;
    mare.set("crypto", c)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_unique_json;

    #[test]
    fn unique_json_rejects_literal_escaped_and_nested_duplicates() {
        for source in [
            r#"{"decision":"deny","decision":"allow"}"#,
            r#"{"decision":"deny","\u0064ecision":"allow"}"#,
            r#"{"nested":[{"reason":"one","reason":"two"}]}"#,
            r#"{"a":1,"a":1}"#,
            r#"{"$serde_json::private::Number":"1","\u0024serde_json::private::Number":"2"}"#,
        ] {
            assert!(parse_unique_json(source.as_bytes()).is_err(), "accepted {source}");
        }
        let source = r#"{"decision":"deny","nested":[null,true,-1,1.5,"string",{}]}"#;
        assert_eq!(
            parse_unique_json(source.as_bytes()).unwrap(),
            serde_json::from_str::<serde_json::Value>(source).unwrap()
        );
        for num in ["0.5", "-123.456", "9007199254740993", "200000000000000000000000000000000000004"] {
            assert_eq!(
                parse_unique_json(num.as_bytes()).unwrap(),
                serde_json::from_str::<serde_json::Value>(num).unwrap(),
                "number mismatch for {num}"
            );
        }
        let marker_obj = parse_unique_json(br#"{"$serde_json::private::Number":"1"}"#).unwrap();
        assert_eq!(
            marker_obj.as_object().and_then(|m| m.get("$serde_json::private::Number")).and_then(|v| v.as_str()),
            Some("1")
        );
    }

    #[test]
    fn lua_decode_unique_handles_decimals_large_ints_nested_and_duplicates() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let ok: bool = lua
            .load(
                r#"
                local dec = mare.json.decode_unique("0.5")
                assert(type(dec) == "number" and dec == 0.5, "expected 0.5 number, got " .. type(dec))
                local big = mare.json.decode_unique("9007199254740993")
                assert(type(big) == "number" and big == 9007199254740993, "expected exact i64")
                local nested = mare.json.decode_unique('{"a":0.5,"b":[1.25,9007199254740993]}')
                assert(type(nested) == "table" and nested.a == 0.5 and nested.b[1] == 1.25 and nested.b[2] == 9007199254740993)
                local dup, err = mare.json.decode_unique('{"a":1,"a":2}')
                assert(dup == nil and tostring(err):find("duplicate JSON member", 1, true))
                return true
            "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn lua_decode_unique_keeps_serde_private_number_object_as_table() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let ok: bool = lua
            .load(
                r#"
                local obj = mare.json.decode_unique('{"$serde_json::private::Number":"1"}')
                assert(type(obj) == "table", "expected table, got " .. type(obj))
                assert(obj["$serde_json::private::Number"] == "1", "expected string '1'")

                local multi = mare.json.decode_unique('{"$serde_json::private::Number":"1","a":2}')
                assert(type(multi) == "table" and multi["$serde_json::private::Number"] == "1" and multi.a == 2)

                local non_num = mare.json.decode_unique('{"$serde_json::private::Number":"not-a-number"}')
                assert(type(non_num) == "table" and non_num["$serde_json::private::Number"] == "not-a-number")

                local int_val = mare.json.decode_unique('{"$serde_json::private::Number":123}')
                assert(type(int_val) == "table" and int_val["$serde_json::private::Number"] == 123)

                local nested = mare.json.decode_unique('{"x":{"$serde_json::private::Number":"1.5"},"y":1.5}')
                assert(type(nested.x) == "table" and nested.x["$serde_json::private::Number"] == "1.5")
                assert(type(nested.y) == "number" and nested.y == 1.5)

                local dup, err = mare.json.decode_unique('{"$serde_json::private::Number":"1","$serde_json::private::Number":"2"}')
                assert(dup == nil and tostring(err):find("duplicate JSON member", 1, true))
                return true
            "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    fn nested_json_array(depth: usize) -> String {
        format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
    }

    fn nested_json_object(depth: usize) -> String {
        format!("{}0{}", r#"{"a":"#.repeat(depth), "}".repeat(depth))
    }

    fn nested_json_mixed(depth: usize) -> String {
        let mut prefix = String::new();
        let mut closers = Vec::with_capacity(depth);
        for i in 0..depth {
            if i % 2 == 0 {
                prefix.push_str(r#"{"a":"#);
                closers.push('}');
            } else {
                prefix.push('[');
                closers.push(']');
            }
        }
        let suffix: String = closers.into_iter().rev().collect();
        format!("{prefix}0{suffix}")
    }

    #[test]
    fn unique_json_matches_serde_json_depth_limit_127_ok_128_err() {
        assert_eq!(nested_json_mixed(2), r#"{"a":[0]}"#);
        assert_eq!(nested_json_mixed(3), r#"{"a":[{"a":0}]}"#);

        for (kind, make) in [
            ("array", nested_json_array as fn(usize) -> String),
            ("object", nested_json_object as fn(usize) -> String),
            ("mixed", nested_json_mixed as fn(usize) -> String),
        ] {
            let ok_127 = make(127);
            assert!(
                serde_json::from_str::<serde_json::Value>(&ok_127).is_ok(),
                "serde_json should accept 127-deep {kind}"
            );
            assert!(parse_unique_json(ok_127.as_bytes()).is_ok(), "parse_unique_json should accept 127-deep {kind}");

            let err_128 = make(128);
            let serde_err = serde_json::from_str::<serde_json::Value>(&err_128)
                .expect_err(&format!("serde_json should reject 128-deep {kind}"));
            assert!(
                serde_err.to_string().contains("recursion limit exceeded"),
                "serde_json 128-deep {kind} must fail due to recursion limit, got: {serde_err}"
            );
            let unique_err = parse_unique_json(err_128.as_bytes())
                .expect_err(&format!("parse_unique_json should reject 128-deep {kind}"));
            assert!(
                unique_err.contains("recursion limit exceeded"),
                "parse_unique_json 128-deep {kind} must fail due to recursion limit, got: {unique_err}"
            );
        }

        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let decode_fn: mlua::Function = lua.load("return mare.json.decode_unique").eval().unwrap();
        for (kind, make) in [
            ("array", nested_json_array as fn(usize) -> String),
            ("object", nested_json_object as fn(usize) -> String),
            ("mixed", nested_json_mixed as fn(usize) -> String),
        ] {
            let (val, err): (mlua::Value, Option<String>) = decode_fn.call(make(127)).unwrap();
            assert!(!matches!(val, mlua::Value::Nil), "Lua decode_unique should accept 127-deep {kind}: {err:?}");
            let (val, err): (mlua::Value, Option<String>) = decode_fn.call(make(128)).unwrap();
            assert!(
                matches!(val, mlua::Value::Nil) && err.as_deref().is_some_and(|e| e.contains("recursion limit")),
                "Lua decode_unique should reject 128-deep {kind}, got val={val:?}, err={err:?}"
            );
        }
    }

    #[test]
    fn clone_lossless_preserves_fidelity_isolation_and_budgets() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            local fixture = assert(mare.json.decode_lossless('{"n":null,"arr":[],"obj":{},"num":0.5,"big":9007199254740993,"nested":{"a":[1,2]}}'))
            local cloned = mare.json.clone(fixture)
            assert(mare.json.encode_strict(cloned) == mare.json.encode_strict(fixture))
            assert(cloned.n == mare.json.null)
            assert(mare.json.is_array(cloned.arr) and not mare.json.is_array(cloned.obj))
            assert(cloned.num == 0.5 and cloned.big == 9007199254740993)

            -- Independent mutation isolation.
            cloned.nested.a[1] = 99
            cloned.arr[1] = "added"
            assert(fixture.nested.a[1] == 1 and #fixture.arr == 0)

            -- Reject nil, functions, non-finite numbers, cycles, depth > 64, and > 10000 nodes.
            assert(not pcall(mare.json.clone, nil))
            assert(not pcall(mare.json.clone, { fn = function() end }))
            assert(not pcall(mare.json.clone, 0 / 0))
            assert(not pcall(mare.json.clone, math.huge))
            local cyc = {}; cyc.self = cyc
            assert(not pcall(mare.json.clone, cyc))
            local deep = 0
            for _ = 1, 65 do deep = { a = deep } end
            assert(not pcall(mare.json.clone, deep))
            local wide = {}
            for i = 1, 10001 do wide[i] = 1 end
            assert(not pcall(mare.json.clone, wide))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn results_lua_mutation_isolation_and_approval_hash_invariance() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        crate::approval::install(&lua, &mare).unwrap();
        crate::text::install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let results_src = include_str!("../../mare-plugin/preset/plugins/lua/tools/results.lua");
        let loader: mlua::Function = lua.load(results_src).set_name("@preset/tools/results.lua").eval().unwrap();
        lua.globals().set("LOAD_RESULTS", loader).unwrap();

        lua.load(
            r#"
            table.copy = function(t)
                if type(t) ~= "table" then return t end
                local out = {}
                for k, v in pairs(t) do out[k] = v end
                return out
            end
            package.loaded.mare_lib = {
                truncate_utf8 = function(s) return s, false end,
            }

            local seen_calls = {}
            local bus = {
                waterfall = function(_, name, ev)
                    seen_calls[name] = mare.json.encode_strict(ev.call)
                    ev.call.arguments.nested.mutated_by = name
                    if name == "tool/program-result" then
                        ev.result.value.nested.from_prog = true
                    elseif name == "tool/after" then
                        ev.result.details.nested.from_after = true
                    end
                    return ev
                end,
                waterfall_checked = function(self, name, ev)
                    return self.waterfall(self, name, ev)
                end,
            }
            local R = LOAD_RESULTS({
                ctx = { bus = bus },
                cfg = { max_lines = 200, max_bytes = 65536 },
                svc = {},
            })

            local call = {
                id = "call-1",
                name = "custom_tool",
                arguments = { path = "a.txt", nested = { keep = 1, tags = mare.json.array({}) } },
            }
            local before_hash = mare.crypto.sha256(mare.json.encode_strict(call))
            local raw = {
                content = "raw",
                details = { nested = { orig = 2 }, nil_marker = mare.json.null },
                value = { nested = { count = 3 } },
                sandbox_denials = mare.json.array({ { kind = "fs", path = "/secret" } }),
            }
            local def = {
                render_value = function(args, val)
                    args.nested.from_render = true
                    val.nested.from_render = true
                    return mare.json.encode_strict({ args_keep = args.nested.keep, count = val.nested.count })
                end,
            }
            local display, program = R.finish(raw, nil, def, call, { audience = "model" }, { tool = "custom_tool" })
            local after_hash = mare.crypto.sha256(mare.json.encode_strict(call))

            assert(before_hash == after_hash, "approval/call hash must remain invariant")
            assert(call.arguments.nested.mutated_by == nil and call.arguments.nested.from_render == nil)
            -- Listeners on program-result, after, and result each saw unmodified original call.
            assert(seen_calls["tool/program-result"] == seen_calls["tool/after"])
            assert(seen_calls["tool/after"] == seen_calls["tool/result"])
            -- Canonical program must not carry mutations from tool/after or render_value.
            assert(program.value.nested.from_prog == true and program.value.nested.from_render == nil)
            assert(program.details.nested.from_after == nil)
            assert(program.details.nil_marker == mare.json.null)
            assert(display.details.nested.from_after == true and display.value == nil)
            assert(display.content == '{"args_keep":1,"count":3}')
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn json_clone_and_copy_metadata_are_public_without_private_plants() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            assert(mare.json._clone_lossless == nil, "_clone_lossless must be removed")
            assert(package.loaded["llm._schema_meta"] == nil, "llm._schema_meta package.loaded plant must be removed")
            assert(type(mare.json.clone) == "function", "mare.json.clone must be public")
            assert(type(mare.json.copy_metadata) == "function", "mare.json.copy_metadata must be public")

            local fixture = assert(mare.json.decode_lossless('{"n":null,"arr":[],"obj":{},"num":0.5,"big":9007199254740993,"nested":{"a":[1,2]}}'))
            local cloned = mare.json.clone(fixture)
            assert(mare.json.encode_strict(cloned) == mare.json.encode_strict(fixture))
            assert(cloned.n == mare.json.null)
            assert(mare.json.is_array(cloned.arr) and not mare.json.is_array(cloned.obj))
            cloned.nested.a[1] = 99
            assert(fixture.nested.a[1] == 1)

            local compiled = mare.schema.compile({
                type = "object",
                ["x-mare-order"] = { "z", "a" },
                properties = { z = { type = "string" }, a = { type = "string" } },
            })
            local src = compiled:value()
            local dst = {
                type = "object",
                ["x-mare-order"] = { "z", "a" },
                properties = { z = { type = "string" }, a = { type = "string" } },
            }
            assert(mare.json.encode(dst):find('"x-mare-order"', 1, true) ~= nil, "unmarked table keeps x-mare-order")
            mare.json.copy_metadata(src, dst)
            assert(mare.json.encode(dst) == mare.json.encode(src))
            assert(mare.json.encode(dst):find('"x-mare-order"', 1, true) == nil, "marked table strips x-mare-order")

            local cyc = {}
            cyc.self = cyc
            local dst_cyc = {}
            dst_cyc.self = dst_cyc
            assert(not pcall(mare.json.copy_metadata, cyc, dst_cyc))
            "#,
        )
        .exec()
        .unwrap();
    }
}
