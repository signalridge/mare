use mlua::serde::DeserializeOptions;
use mlua::{Lua, LuaSerdeExt, Value};

use super::rt_err;
use super::shape::{apply_marked_schemas, sort_json};

fn de_opts() -> DeserializeOptions {
    DeserializeOptions::new().deny_unsupported_types(false).deny_recursive_tables(false)
}

/// JSON → Lua. `null` becomes nil (inside objects the key is simply absent); arrays keep
/// the array metatable so they re-encode as arrays even when empty.
pub fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    json_to_lua_value(lua, v, false)
}

struct LossyJson(serde_json::Value);

struct StrictJsonKey(String);

thread_local! {
    static LOSSY_REPLACEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct LossyScope(usize);

impl LossyScope {
    fn enter() -> Self {
        let prev = LOSSY_REPLACEMENTS.with(|c| c.replace(0));
        Self(prev)
    }

    fn count(&self) -> usize {
        LOSSY_REPLACEMENTS.with(|c| c.get())
    }
}

impl Drop for LossyScope {
    fn drop(&mut self) {
        LOSSY_REPLACEMENTS.with(|c| c.set(self.0));
    }
}

fn lossy_utf8_value(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            LOSSY_REPLACEMENTS.with(|c| c.set(c.get().saturating_add(1)));
            tracing::warn!(byte_len = bytes.len(), "lua_to_json: lossily replacing non-UTF-8 bytes in string value");
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

impl<'de> serde::Deserialize<'de> for StrictJsonKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> serde::de::Visitor<'de> for KeyVisitor {
            type Value = StrictJsonKey;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a valid UTF-8 string key")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictJsonKey(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictJsonKey(v))
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                std::str::from_utf8(v)
                    .map(|s| StrictJsonKey(s.to_owned()))
                    .map_err(|_| E::custom("object key must be valid UTF-8"))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                String::from_utf8(v).map(StrictJsonKey).map_err(|_| E::custom("object key must be valid UTF-8"))
            }
        }
        deserializer.deserialize_any(KeyVisitor)
    }
}

impl<'de> serde::Deserialize<'de> for LossyJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> serde::de::Visitor<'de> for ValueVisitor {
            type Value = LossyJson;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any valid JSON value")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::Bool(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::Number(v.into())))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::Number(v.into())))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                // Non-finite Lua numbers (NaN/Inf) are converted to JSON null.
                Ok(LossyJson(
                    serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, serde_json::Value::Number),
                ))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::String(v.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::String(v)))
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::String(lossy_utf8_value(v))))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                let s = String::from_utf8(v).unwrap_or_else(|e| lossy_utf8_value(e.as_bytes()));
                Ok(LossyJson(serde_json::Value::String(s)))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(LossyJson(serde_json::Value::Null))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(self, deserializer: D2) -> Result<Self::Value, D2::Error> {
                deserializer.deserialize_any(self)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut vec = Vec::new();
                while let Some(LossyJson(elem)) = seq.next_element()? {
                    vec.push(elem);
                }
                Ok(LossyJson(serde_json::Value::Array(vec)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut obj = serde_json::Map::new();
                while let Some((StrictJsonKey(k), LossyJson(v))) = map.next_entry()? {
                    obj.insert(k, v);
                }
                Ok(LossyJson(serde_json::Value::Object(obj)))
            }
        }
        deserializer.deserialize_any(ValueVisitor)
    }
}

/// Lua → JSON. Functions/userdata are skipped, non-UTF-8 string values are replaced lossily
/// with a warning, and non-UTF-8 object keys are rejected. Object keys come out sorted;
/// explicitly marked tool schemas apply their ordering hint without changing ordinary JSON data.
pub fn lua_to_json(lua: &Lua, v: Value) -> mlua::Result<serde_json::Value> {
    let source = v.clone();
    let mut j = match v {
        Value::Nil => return Ok(serde_json::Value::Null),
        Value::LightUserData(ud) if ud == lua.null().as_light_userdata().unwrap_or(ud) && ud.0.is_null() => {
            return Ok(serde_json::Value::Null);
        }
        v => lua.from_value_with::<LossyJson>(v, de_opts())?.0,
    };
    if sort_json(&mut j) {
        apply_marked_schemas(lua, source, &mut j, 0, &mut Vec::new())?;
    }
    Ok(j)
}

/// Like [`lua_to_json`], also returning `true` when any non-UTF-8 string value was lossily replaced.
pub fn lua_to_json_lossy(lua: &Lua, v: Value) -> mlua::Result<(serde_json::Value, bool)> {
    let scope = LossyScope::enter();
    let j = lua_to_json(lua, v)?;
    Ok((j, scope.count() > 0))
}

/// Bounded variant of [`lua_to_json_lossy`] enforcing a maximum node budget and depth limit
/// before allocating large output structures. Non-UTF-8 object keys fail, non-UTF-8 string values
/// are replaced lossily with `U+FFFD` (`lossy = true`), and non-finite numbers (`NaN`/`Inf`)
/// become JSON `null`.
pub fn lua_to_json_lossy_bounded(lua: &Lua, value: Value, max_nodes: usize) -> mlua::Result<(serde_json::Value, bool)> {
    use serde_json::{Map, Number, Value as J};
    fn walk(
        lua: &Lua,
        v: Value,
        path: &str,
        depth: usize,
        budget: &mut usize,
        stack: &mut Vec<usize>,
        lossy: &mut bool,
    ) -> mlua::Result<J> {
        let err = |message: &str| rt_err(format!("{path}: {message}"));
        if depth > 64 || *budget == 0 {
            return Err(err("JSON value exceeds resource limit"));
        }
        *budget -= 1;
        Ok(match v {
            Value::Nil => J::Null,
            Value::LightUserData(ud) if ud.0.is_null() => J::Null,
            Value::Boolean(v) => J::Bool(v),
            Value::Integer(v) => J::Number(v.into()),
            // Non-finite Lua numbers (NaN/Inf) are converted to JSON null.
            Value::Number(v) => Number::from_f64(v).map_or(J::Null, J::Number),
            Value::String(s) => match s.to_str() {
                Ok(text) => J::String(text.to_string()),
                Err(_) => {
                    let bytes = s.as_bytes();
                    *lossy = true;
                    tracing::warn!(
                        byte_len = bytes.len(),
                        "lua_to_json_lossy_bounded: lossily replacing non-UTF-8 bytes in string value"
                    );
                    J::String(String::from_utf8_lossy(&bytes).into_owned())
                }
            },
            Value::Table(t) => {
                let marked_array = t.metatable().is_some_and(|mt| mt == lua.array_metatable());
                let pointer = t.to_pointer() as usize;
                if stack.contains(&pointer) {
                    return Err(err("recursive JSON table"));
                }
                stack.push(pointer);
                let mut entries = Vec::new();
                for pair in t.pairs::<Value, Value>() {
                    if entries.len() > *budget {
                        return Err(err("JSON value exceeds resource limit"));
                    }
                    entries.push(pair?);
                }
                let is_dense_array = !entries.is_empty()
                    && entries
                        .iter()
                        .all(|(k, _)| matches!(k, Value::Integer(i) if *i > 0 && (*i as usize) <= entries.len()));
                let result = if marked_array || is_dense_array {
                    let mut values = vec![None; entries.len()];
                    for (k, v) in entries {
                        let i = match k {
                            Value::Integer(i) if i > 0 && (i as usize) <= values.len() => i as usize - 1,
                            _ => return Err(err("JSON array must have dense positive integer keys")),
                        };
                        values[i] = Some(walk(lua, v, &format!("{path}/{i}"), depth + 1, budget, stack, lossy)?);
                    }
                    J::Array(values.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| err("sparse JSON array"))?)
                } else {
                    let mut values = Map::new();
                    for (k, v) in entries {
                        let key = match k {
                            Value::String(s) => {
                                s.to_str().map_err(|_| err("object key must be valid UTF-8"))?.to_string()
                            }
                            _ => return Err(err("JSON object keys must be strings")),
                        };
                        let child_path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                        values.insert(key, walk(lua, v, &child_path, depth + 1, budget, stack, lossy)?);
                    }
                    values.sort_keys();
                    J::Object(values)
                };
                stack.pop();
                result
            }
            // Match de_opts().deny_unsupported_types(false): functions/userdata/threads serialize as null.
            _ => J::Null,
        })
    }
    let mut budget = max_nodes;
    let mut lossy = false;
    let j = walk(lua, value, "$", 0, &mut budget, &mut Vec::new(), &mut lossy)?;
    Ok((j, lossy))
}

/// Default node budget for strict Lua ↔ JSON conversions.
pub const STRICT_JSON_MAX_NODES: usize = 10_000;

/// Bounded node budget for history-bearing lossless Lua ↔ JSON conversions.
pub const HISTORY_LOSSLESS_MAX_NODES: usize = 1_000_000;

/// Lossless conversion of Lua values to JSON for tool schemas and arguments. Tables must be
/// dense arrays or string-keyed objects; opaque metatables and all non-JSON values fail.
pub fn strict_lua_to_json(lua: &Lua, value: Value) -> mlua::Result<serde_json::Value> {
    strict_lua_to_json_bounded(lua, value, STRICT_JSON_MAX_NODES)
}

/// [`strict_lua_to_json`] with an explicit node budget.
pub fn strict_lua_to_json_bounded(lua: &Lua, value: Value, max_nodes: usize) -> mlua::Result<serde_json::Value> {
    use serde_json::{Map, Number, Value as J};
    enum PathSeg<'a> {
        Root,
        Index(&'a Self, usize),
        Key(&'a Self, &'a str),
    }
    impl PathSeg<'_> {
        fn format(&self) -> String {
            let mut parts = Vec::new();
            let mut cur = self;
            loop {
                match cur {
                    PathSeg::Root => break,
                    PathSeg::Index(parent, i) => {
                        parts.push(i.to_string());
                        cur = parent;
                    }
                    PathSeg::Key(parent, k) => {
                        parts.push(k.replace('~', "~0").replace('/', "~1"));
                        cur = parent;
                    }
                }
            }
            let mut out = String::from("$");
            for part in parts.into_iter().rev() {
                out.push('/');
                out.push_str(&part);
            }
            out
        }
    }
    fn walk(
        array_mt: &mlua::Table,
        v: Value,
        path: &PathSeg<'_>,
        depth: usize,
        budget: &mut usize,
        stack: &mut Vec<usize>,
    ) -> mlua::Result<J> {
        let err = |message: &str| rt_err(format!("{}: {message}", path.format()));
        if depth > 64 || *budget == 0 {
            return Err(err("JSON value exceeds resource limit"));
        }
        *budget -= 1;
        Ok(match v {
            Value::Nil => return Err(err("nil is not a JSON value; use mare.json.null")),
            Value::LightUserData(ud) if ud.0.is_null() => J::Null,
            Value::Boolean(v) => J::Bool(v),
            Value::Integer(v) => J::Number(v.into()),
            Value::Number(v) => J::Number(Number::from_f64(v).ok_or_else(|| err("non-finite JSON number"))?),
            Value::String(v) => J::String(v.to_str().map_err(|_| err("JSON strings must be UTF-8"))?.to_string()),
            Value::Table(t) => {
                let marked_array = match t.metatable() {
                    None => false,
                    Some(mt) if mt == *array_mt => true,
                    Some(_) => return Err(err("JSON table has an unsupported metatable")),
                };
                let pointer = t.to_pointer() as usize;
                if stack.contains(&pointer) {
                    return Err(err("recursive JSON table"));
                }
                stack.push(pointer);
                let mut entries = Vec::new();
                for pair in t.pairs::<Value, Value>() {
                    if entries.len() > *budget {
                        return Err(err("JSON value exceeds resource limit"));
                    }
                    entries.push(pair?);
                }
                let has_integer = entries.iter().any(|(k, _)| matches!(k, Value::Integer(_)));
                let result = if marked_array || has_integer {
                    let mut values = vec![None; entries.len()];
                    for (k, v) in entries {
                        let i = match k {
                            Value::Integer(i) if i > 0 && (i as usize) <= values.len() => i as usize - 1,
                            _ => return Err(err("JSON array must have dense positive integer keys")),
                        };
                        let child_path = PathSeg::Index(path, i);
                        values[i] = Some(walk(array_mt, v, &child_path, depth + 1, budget, stack)?);
                    }
                    J::Array(values.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| err("sparse JSON array"))?)
                } else {
                    let mut values = Map::with_capacity(entries.len());
                    for (k, v) in entries {
                        let key = match k {
                            Value::String(s) => {
                                s.to_str().map_err(|_| err("JSON object keys must be UTF-8"))?.to_string()
                            }
                            _ => return Err(err("JSON object keys must be strings")),
                        };
                        let child_path = PathSeg::Key(path, &key);
                        let child = walk(array_mt, v, &child_path, depth + 1, budget, stack)?;
                        values.insert(key, child);
                    }
                    values.sort_keys();
                    J::Object(values)
                };
                stack.pop();
                result
            }
            _ => return Err(err("unsupported JSON value")),
        })
    }
    let array_mt = lua.array_metatable();
    let mut budget = max_nodes;
    walk(&array_mt, value, &PathSeg::Root, 0, &mut budget, &mut Vec::new())
}

fn json_to_lua_value(lua: &Lua, value: &serde_json::Value, preserve_null: bool) -> mlua::Result<Value> {
    Ok(match value {
        serde_json::Value::Null => {
            if preserve_null {
                Value::NULL
            } else {
                Value::Nil
            }
        }
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                // u64 above i64::MAX and decimals: nearest f64 (lossless callers re-check the roundtrip).
                Value::Number(f)
            } else {
                return Err(rt_err("JSON number cannot be represented in Lua"));
            }
        }
        serde_json::Value::String(s) => Value::String(lua.create_string(s)?),
        serde_json::Value::Array(arr) => {
            let t = lua.create_table_with_capacity(arr.len(), 0)?;
            t.set_metatable(Some(lua.array_metatable()))?;
            for (i, v) in arr.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua_value(lua, v, preserve_null)?)?;
            }
            Value::Table(t)
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table_with_capacity(0, map.len())?;
            for (k, v) in map {
                if !preserve_null && v.is_null() {
                    continue;
                }
                t.raw_set(k.as_str(), json_to_lua_value(lua, v, preserve_null)?)?;
            }
            Value::Table(t)
        }
    })
}

/// Convert a `serde_json::Value` to a Lua value, preserving `null` as `Value::NULL`.
pub fn json_to_lua_with_null(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    json_to_lua_value(lua, value, true)
}

/// Convert an already parsed JSON value to Lua without further loss. This cannot recover
/// duplicate keys lost during the initial serde_json parse.
pub fn json_to_lua_lossless(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    json_to_lua_lossless_bounded(lua, value, STRICT_JSON_MAX_NODES)
}

/// [`json_to_lua_lossless`] with an explicit node budget for the lossless roundtrip check.
/// The node budget is checked on the parsed JSON before any Lua value is allocated, with the
/// same node and depth accounting as [`strict_lua_to_json_bounded`].
pub fn json_to_lua_lossless_bounded(lua: &Lua, value: &serde_json::Value, max_nodes: usize) -> mlua::Result<Value> {
    fn within_budget(v: &serde_json::Value, depth: usize, budget: &mut usize) -> bool {
        if depth > 64 || *budget == 0 {
            return false;
        }
        *budget -= 1;
        match v {
            serde_json::Value::Array(items) => items.iter().all(|v| within_budget(v, depth + 1, budget)),
            serde_json::Value::Object(map) => map.values().all(|v| within_budget(v, depth + 1, budget)),
            _ => true,
        }
    }
    if !within_budget(value, 0, &mut { max_nodes }) {
        return Err(rt_err("$: JSON value exceeds resource limit"));
    }
    let result = json_to_lua_value(lua, value, true)?;
    let roundtrip = strict_lua_to_json_bounded(lua, result.clone(), max_nodes)?;
    if !crate::schema::try_json_equal(value, &roundtrip).map_err(rt_err)? {
        fn difference(a: &serde_json::Value, b: &serde_json::Value, path: &str) -> String {
            match (a, b) {
                (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
                    for (key, value) in a {
                        let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                        match b.get(key) {
                            Some(other) if !crate::schema::json_equal(value, other) => {
                                return difference(value, other, &child);
                            }
                            None => return child,
                            _ => {}
                        }
                    }
                }
                (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
                    for (index, value) in a.iter().enumerate() {
                        let child = format!("{path}/{index}");
                        match b.get(index) {
                            Some(other) if !crate::schema::json_equal(value, other) => {
                                return difference(value, other, &child);
                            }
                            None => return child,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            path.to_string()
        }
        return Err(rt_err(format!(
            "{}: JSON value cannot be represented losslessly in Lua",
            difference(value, &roundtrip, "$")
        )));
    }
    Ok(result)
}
