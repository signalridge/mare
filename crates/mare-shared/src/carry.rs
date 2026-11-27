//! Carrying config functions into another Lua VM (a sub-agent's). Config crosses to a child VM as
//! JSON, which cannot hold functions; for the parts of the config that must behave the same in
//! the child (the model-parameter rules), [`Carried::config_to_json`] puts the same sentinels
//! [`crate::lua::config_to_json`] uses in place of each function and captures the function itself:
//! its bytecode plus every upvalue — plain values copied (shared tables and cycles kept), the
//! private environment of the config chunk rebuilt, and values reachable from the globals
//! (`string`, `math.max`, `mare`, …) looked up by path in the receiving VM. [`Carried::install`]
//! registers the rebuilt functions in the receiving VM, where [`crate::lua::restore_config_fns`]
//! swaps the sentinels back.
//!
//! Not carried: userdata, threads and C functions the globals don't reach (the function holding
//! one is left out, reported in [`Carried::problems`]); two functions sharing one upvalue
//! variable each get a copy.

use std::collections::HashMap;

use mlua::{Function, Lua, LuaSerdeExt, Table, Value};

use crate::lua::{FN_SENTINEL, config_upvalue, fn_registry, isolated_env, lua_to_json};

/// At most this many tables and functions per carry (a captured value is not a whole VM).
const MAX_NODES: usize = 4096;
/// Values reachable from the globals by a path of at most this many keys travel by path.
const GLOBAL_DEPTH: usize = 3;

#[derive(Debug, Clone)]
enum Leaf {
    Nil,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(Vec<u8>),
    Null,
    Ref(usize),
    Global(Vec<Vec<u8>>),
}

#[derive(Debug)]
enum Node {
    Table {
        array: bool,
        entries: Vec<(Leaf, Leaf)>,
    },
    /// a config chunk's private environment (`isolated_env`): rebuilt over the receiver's globals
    Env {
        entries: Vec<(Leaf, Leaf)>,
    },
    Func {
        code: Vec<u8>,
        name: String,
        upvalues: Vec<Leaf>,
    },
}

/// Functions captured in one VM, to be rebuilt in another. `Send`: it crosses to the child's
/// thread.
#[derive(Debug, Default)]
pub struct Carried {
    /// `None` while being captured, or when capturing failed
    nodes: Vec<Option<Node>>,
    /// sentinel id → the function node
    roots: Vec<(String, usize)>,
    /// what could not be carried, with its definition site
    pub problems: Vec<String>,
    memo: HashMap<usize, Result<usize, String>>,
    globals: Option<HashMap<usize, Vec<Vec<u8>>>>,
}

type Captured = Result<Leaf, String>;

fn bytes(s: &mlua::LuaString) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn site(f: &Function) -> String {
    let info = f.info();
    format!("{}:{}", info.short_src.as_deref().unwrap_or("?"), info.line_defined.unwrap_or(0))
}

/// What of config value `v` would not reach another VM: "<function site>: <reason>" per
/// function left out (`mare.subagent.uncarried`, for `mare doctor`).
pub fn uncarried(lua: &Lua, v: Value) -> mlua::Result<Vec<String>> {
    let mut c = Carried::default();
    c.swap(lua, v, 0)?;
    Ok(c.problems)
}

impl Carried {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Lua → JSON for one config value: each function inside becomes a sentinel and is captured
    /// (one that can't be carried is left out and named in [`Self::problems`]).
    pub fn config_to_json(&mut self, lua: &Lua, v: Value) -> mlua::Result<serde_json::Value> {
        let v = self.swap(lua, v, 0)?;
        lua_to_json(lua, v)
    }

    fn swap(&mut self, lua: &Lua, v: Value, depth: usize) -> mlua::Result<Value> {
        Ok(match v {
            Value::Function(f) => match self.capture(lua, Value::Function(f.clone()))? {
                Ok(Leaf::Ref(idx)) => {
                    let id = match self.roots.iter().find(|(_, i)| *i == idx) {
                        Some((id, _)) => id.clone(),
                        None => {
                            let id = format!("carry#{}", self.roots.len() + 1);
                            self.roots.push((id.clone(), idx));
                            id
                        }
                    };
                    Value::String(lua.create_string(format!("{FN_SENTINEL}{id}"))?)
                }
                Ok(_) => Value::Nil,
                Err(e) => {
                    self.problems.push(format!("{}: {e}", site(&f)));
                    Value::Nil
                }
            },
            Value::Table(t) if depth < 32 => {
                let out = lua.create_table()?;
                for kv in t.pairs::<Value, Value>() {
                    let (k, val) = kv?;
                    out.raw_set(k, self.swap(lua, val, depth + 1)?)?;
                }
                if let Some(mt) = t.metatable() {
                    out.set_metatable(Some(mt))?;
                }
                Value::Table(out)
            }
            v => v,
        })
    }

    /// path of a table or function from the globals (the receiving VM has its own)
    fn global_path(&mut self, lua: &Lua, ptr: usize) -> mlua::Result<Option<Vec<Vec<u8>>>> {
        if self.globals.is_none() {
            let mut map: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
            let g = lua.globals();
            map.insert(g.to_pointer() as usize, vec![]);
            let mut queue = vec![(g, Vec::<Vec<u8>>::new())];
            while let Some((t, path)) = queue.pop() {
                if path.len() >= GLOBAL_DEPTH {
                    continue;
                }
                for kv in t.pairs::<Value, Value>() {
                    let (k, v) = kv?;
                    let Value::String(k) = k else { continue };
                    let k = bytes(&k);
                    // the config itself is data, never a path (its functions are what we carry)
                    if path.len() == 1 && path[0] == b"mare" && k == b"config" {
                        continue;
                    }
                    let p = match &v {
                        Value::Table(t) => t.to_pointer() as usize,
                        Value::Function(f) => f.to_pointer() as usize,
                        _ => continue,
                    };
                    if map.contains_key(&p) {
                        continue;
                    }
                    let mut np = path.clone();
                    np.push(k);
                    map.insert(p, np.clone());
                    if let Value::Table(t) = v {
                        queue.insert(0, (t, np));
                    }
                }
            }
            self.globals = Some(map);
        }
        Ok(self.globals.as_ref().and_then(|m| m.get(&ptr).cloned()))
    }

    fn reserve(&mut self, ptr: usize) -> mlua::Result<Result<usize, String>> {
        if self.nodes.len() >= MAX_NODES {
            return Ok(Err("captured values too large".into()));
        }
        self.nodes.push(None);
        let idx = self.nodes.len() - 1;
        self.memo.insert(ptr, Ok(idx));
        Ok(Ok(idx))
    }

    fn fail(&mut self, ptr: usize, e: String) -> Captured {
        self.memo.insert(ptr, Err(e.clone()));
        Err(e)
    }

    fn capture(&mut self, lua: &Lua, v: Value) -> mlua::Result<Captured> {
        let ptr = match &v {
            Value::Nil => return Ok(Ok(Leaf::Nil)),
            Value::Boolean(b) => return Ok(Ok(Leaf::Bool(*b))),
            Value::Integer(i) => return Ok(Ok(Leaf::Int(*i))),
            Value::Number(n) => return Ok(Ok(Leaf::Num(*n))),
            Value::String(s) => return Ok(Ok(Leaf::Str(bytes(s)))),
            Value::LightUserData(u) if u.0.is_null() => return Ok(Ok(Leaf::Null)),
            Value::Table(t) => t.to_pointer() as usize,
            Value::Function(f) => f.to_pointer() as usize,
            other => return Ok(Err(format!("a {} value", other.type_name()))),
        };
        if let Some(r) = self.memo.get(&ptr) {
            return Ok(r.clone().map(Leaf::Ref));
        }
        if let Some(path) = self.global_path(lua, ptr)? {
            return Ok(Ok(Leaf::Global(path)));
        }
        match v {
            Value::Table(t) => self.capture_table(lua, t, ptr),
            Value::Function(f) => self.capture_function(lua, f, ptr),
            _ => unreachable!(),
        }
    }

    fn capture_table(&mut self, lua: &Lua, t: Table, ptr: usize) -> mlua::Result<Captured> {
        let env = match t.metatable() {
            None => false,
            Some(mt) => {
                let only_index = mt.raw_get::<Value>("__index")? == Value::Table(lua.globals())
                    && mt.clone().pairs::<Value, Value>().count() == 1;
                if only_index && t.raw_get::<Value>("_G")? == Value::Table(t.clone()) {
                    true
                } else if mt == lua.array_metatable() {
                    false
                } else {
                    return Ok(self.fail(ptr, "a table with a metatable".into()));
                }
            }
        };
        let array = !env && t.metatable().is_some();
        let idx = match self.reserve(ptr)? {
            Ok(i) => i,
            Err(e) => return Ok(Err(e)),
        };
        let mut entries = vec![];
        for kv in t.pairs::<Value, Value>() {
            let (k, v) = kv?;
            if env {
                let name = match &k {
                    Value::String(s) => Some(bytes(s)),
                    _ => None,
                };
                // `_G` points at the env itself; `require` is an owner-bound closure; a config
                // view of `mare` follows the live one
                if name.as_deref() == Some(b"_G".as_slice()) {
                    continue;
                }
                if name.as_deref() == Some(b"require".as_slice()) && matches!(&v, Value::Function(_)) {
                    continue;
                }
                if name.as_deref() == Some(b"mare".as_slice())
                    && let Value::Table(m) = &v
                    && m.raw_get::<Option<bool>>("__mare_config_view").ok().flatten() == Some(true)
                {
                    continue;
                }
            }
            let (k, v) = (self.capture(lua, k)?, self.capture(lua, v)?);
            match (k, v) {
                (Ok(k), Ok(v)) => entries.push((k, v)),
                // the env holds every global the config chunk set: one that can't travel only
                // matters if a carried function reads it
                _ if env => {}
                (Err(e), _) | (_, Err(e)) => return Ok(self.fail(ptr, e)),
            }
        }
        self.nodes[idx] = Some(if env { Node::Env { entries } } else { Node::Table { array, entries } });
        Ok(Ok(Leaf::Ref(idx)))
    }

    fn capture_function(&mut self, lua: &Lua, f: Function, ptr: usize) -> mlua::Result<Captured> {
        let info = f.info();
        if info.what == "C" {
            return Ok(self.fail(ptr, "a built-in function not reachable from the globals".into()));
        }
        let idx = match self.reserve(ptr)? {
            Ok(i) => i,
            Err(e) => return Ok(Err(e)),
        };
        let mut upvalues = vec![];
        for i in 1..=info.num_upvalues as usize {
            let (v, name) = config_upvalue(lua, &f, i)?;
            match self.capture(lua, v)? {
                Ok(l) => upvalues.push(l),
                Err(e) => return Ok(self.fail(ptr, format!("upvalue `{name}` is {e}"))),
            }
        }
        let name = info.source.unwrap_or_else(|| "=?".into());
        self.nodes[idx] = Some(Node::Func { code: f.dump(false), name, upvalues });
        Ok(Ok(Leaf::Ref(idx)))
    }

    /// Rebuild the captured functions in `lua` and register them for
    /// [`crate::lua::restore_config_fns`]. Returns what the receiving VM lacks (a global path).
    pub fn install(self, lua: &Lua) -> mlua::Result<Vec<String>> {
        let mut problems = vec![];
        let mut objs: Vec<Value> = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            objs.push(match n {
                None => Value::Nil,
                Some(Node::Table { array, .. }) => {
                    let t = lua.create_table()?;
                    if *array {
                        t.set_metatable(Some(lua.array_metatable()))?;
                    }
                    Value::Table(t)
                }
                Some(Node::Env { .. }) => Value::Table(isolated_env(lua)?),
                Some(Node::Func { code, name, .. }) => Value::Function(
                    lua.load(&code[..])
                        .set_mode(mlua::chunk::ChunkMode::Binary)
                        .set_name(name.clone())
                        .into_function()?,
                ),
            });
        }
        let leaf = |l: &Leaf, problems: &mut Vec<String>| -> mlua::Result<Value> {
            Ok(match l {
                Leaf::Nil => Value::Nil,
                Leaf::Bool(b) => Value::Boolean(*b),
                Leaf::Int(i) => Value::Integer(*i),
                Leaf::Num(n) => Value::Number(*n),
                Leaf::Str(s) => Value::String(lua.create_string(s)?),
                Leaf::Null => lua.null(),
                Leaf::Ref(i) => objs.get(*i).cloned().unwrap_or(Value::Nil),
                Leaf::Global(path) => {
                    let mut v = Value::Table(lua.globals());
                    for k in path {
                        v = match v {
                            Value::Table(t) => t.raw_get::<Value>(lua.create_string(k)?)?,
                            _ => Value::Nil,
                        };
                    }
                    if v.is_nil() {
                        let p: Vec<_> = path.iter().map(|k| String::from_utf8_lossy(k).into_owned()).collect();
                        problems.push(format!("`{}` is missing here", p.join(".")));
                    }
                    v
                }
            })
        };
        for (n, obj) in self.nodes.iter().zip(&objs) {
            match (n, obj) {
                (Some(Node::Table { entries, .. } | Node::Env { entries }), Value::Table(t)) => {
                    for (k, v) in entries {
                        let k = leaf(k, &mut problems)?;
                        if !k.is_nil() {
                            t.raw_set(k, leaf(v, &mut problems)?)?;
                        }
                    }
                }
                (Some(Node::Func { upvalues, .. }), Value::Function(f)) => {
                    for (i, l) in upvalues.iter().enumerate() {
                        let v = leaf(l, &mut problems)?;
                        // SAFETY: `exec_raw` runs on a protected stack holding the three
                        // arguments (function, index, value); `lua_setupvalue` pops the value
                        // pushed on top and the stack is emptied before returning.
                        unsafe {
                            lua.exec_raw::<()>((f.clone(), (i + 1) as i64, v), |state| {
                                let n = mlua::ffi::lua_tointeger(state, 2) as i32;
                                mlua::ffi::lua_pushvalue(state, 3);
                                mlua::ffi::lua_setupvalue(state, 1, n);
                                mlua::ffi::lua_settop(state, 0);
                            })?;
                        }
                    }
                }
                _ => {}
            }
        }
        let reg = fn_registry(lua)?;
        for (id, idx) in &self.roots {
            if let Some(f @ Value::Function(_)) = objs.get(*idx) {
                reg.raw_set(id.as_str(), f.clone())?;
            }
        }
        problems.dedup();
        Ok(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_and_install_roundtrip_scalars_arrays_and_reject_custom_metatable() {
        let lua = Lua::new();
        let arr_arg = lua.create_table_from([(1, 1)]).unwrap();
        arr_arg.set_metatable(Some(lua.array_metatable())).unwrap();
        let f: mlua::Function = lua
            .load(
                r#"
                local a, b, c, d, n = nil, true, 2.5, ...
                return function() return a, b, c, d[1], n end
                "#,
            )
            .call((arr_arg, lua.null()))
            .unwrap();
        let mut carried = Carried::default();
        let json = carried.config_to_json(&lua, Value::Function(f)).unwrap();
        assert!(json.as_str().unwrap().starts_with(FN_SENTINEL));
        assert!(carried.problems.is_empty(), "{:?}", carried.problems);

        let lua2 = Lua::new();
        let install_probs = carried.install(&lua2).unwrap();
        assert!(install_probs.is_empty(), "{install_probs:?}");

        // Table with a custom metatable is refused with a diagnostic.
        let lua3 = Lua::new();
        let bad_f: mlua::Function = lua3
            .load(
                r#"
                local t = setmetatable({ x = 1 }, { __index = {} })
                return function() return t.x end
                "#,
            )
            .eval()
            .unwrap();
        let probs3 = uncarried(&lua3, Value::Function(bad_f)).unwrap();
        assert!(probs3.iter().any(|p| p.contains("metatable")), "{probs3:?}");
    }
}
