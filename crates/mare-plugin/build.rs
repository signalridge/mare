//! Precompile the built-in Lua plugins to bytecode with the same vendored Lua 5.5 the binary
//! uses. The bytecode keeps debug info (line numbers in errors).
//!
//! In non-debug builds, also pack the human-readable preset sources (extracted to
//! `~/.local/share/mare/runtime/` on version change) into a single deflate-compressed archive so
//! `.rodata` does not carry ~3.8 MB of uncompressed source text alongside precompiled bytecode.

use mlua::LuaSerdeExt as _;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

include!("src/contract_files.rs");
include!("../mare-shared/src/config_env.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/contract_files.rs");
    println!("cargo:rerun-if-changed=../mare-shared/src/config_env.rs");
    let preset_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("preset");
    let out_dir = Path::new(&std::env::var("OUT_DIR").unwrap()).to_path_buf();
    // Single source of truth for public Lua modules (`---@meta <name>` under `preset/types/`),
    // generated in both debug and release builds so `Kernel::public_modules()` stays in sync.
    generate_public_modules(&preset_dir, &out_dir);
    println!("cargo:rustc-env=MARE_API_VERSION={}", mare_protocol::PROTOCOL_VERSION);
    let dir = preset_dir.join("plugins");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut plugin_files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
        .collect();
    plugin_files.sort();

    let lua = mlua::Lua::new();
    let is_debug = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let out = out_dir.join("bytecode");
    if !is_debug {
        std::fs::create_dir_all(&out).unwrap();
    }

    let mut archive_entries: Vec<(String, String)> = Vec::new();
    let mut config_meta_entries: Vec<String> = Vec::new();
    for p in plugin_files {
        println!("cargo:rerun-if-changed={}", p.display());
        let name = p.file_stem().unwrap().to_str().unwrap().to_string();
        let src = std::fs::read_to_string(&p).unwrap();
        let f = lua
            .load(&src)
            .set_name(format!("@builtin/{name}.lua"))
            .into_function()
            .unwrap_or_else(|e| panic!("compiling preset/plugins/{name}.lua: {e}"));
        config_meta_entries.push(extract_builtin_config_meta(&name, &f));
        if !is_debug {
            std::fs::write(out.join(format!("{name}.luac")), f.dump(false)).unwrap();
            archive_entries.push((format!("plugins/{name}.lua"), src));
        }
    }
    write_builtin_config_meta(&out_dir, &config_meta_entries);

    // Pre-evaluate `@builtin/service-contracts` in both debug and release builds so startup
    // parses the deflated JSON manifest directly without executing ~35 Lua contract chunks in the
    // host VM. Release builds also emit `service_contracts.luac` bytecode.
    compile_service_contracts(&lua, &preset_dir, &out_dir, !is_debug);

    // Content fingerprint covering every bundled runtime asset (plugins, lua modules, UI scripts,
    // NEWS.md, types, skills) so same-version upgrades refresh stale runtime directories.
    println!("cargo:rustc-env=MARE_RUNTIME_FINGERPRINT={}", compute_runtime_fingerprint(&preset_dir));

    // debug builds read the preset live from the source tree (no embedded bytecode or archive),
    // while sharing the exact same `BUILTIN_CONFIG_META` and pre-evaluated contract paths as release.
    if is_debug {
        return;
    }

    // Precompile core config engine scripts (`crates/mare-config/preset/*.lua`) so release
    // startup skips parsing ~110 KB of Lua source.
    compile_config_engine(&lua, &out);

    // Precompile preset runtime prelude/UI/keymap/test scripts so startup doesn't parse them.
    for file in ["ui_prelude.lua", "ui_components.lua", "ui_shell.lua", "keymap.lua", "test.lua"] {
        let p = preset_dir.join(file);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap();
        let f = lua
            .load(&src)
            .set_name(format!("@preset/{file}"))
            .into_function()
            .unwrap_or_else(|e| panic!("compiling preset/{file}: {e}"));
        let stem = file.trim_end_matches(".lua");
        std::fs::write(out.join(format!("preset_{stem}.luac")), f.dump(false)).unwrap();
        archive_entries.push((file.to_string(), src));
    }

    let news_path = preset_dir.join("NEWS.md");
    println!("cargo:rerun-if-changed={}", news_path.display());
    // Compress NEWS.md into its own tiny standalone blob so reading news never decompresses
    // the full ~3.85 MB runtime preset source archive.
    let news_src = std::fs::read_to_string(&news_path).unwrap();
    let mut news_enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    news_enc.write_all(news_src.as_bytes()).unwrap();
    std::fs::write(out_dir.join("news.deflate"), news_enc.finish().unwrap()).unwrap();

    // Collect `preset/types/**/*.lua` into the compressed runtime archive.
    let types_dir = preset_dir.join("types");
    println!("cargo:rerun-if-changed={}", types_dir.display());
    let mut type_files = Vec::new();
    collect_files(&types_dir, &types_dir, &mut type_files);
    type_files.sort();
    for rel in type_files {
        let p = types_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap();
        archive_entries.push((format!("types/{rel}"), src));
    }

    // Collect `preset/skills/**/*` (bundled skills) into the compressed runtime archive.
    let skills_dir = preset_dir.join("skills");
    println!("cargo:rerun-if-changed={}", skills_dir.display());
    let mut skill_files = Vec::new();
    collect_all_files(&skills_dir, &skills_dir, &mut skill_files);
    skill_files.sort();
    for rel in skill_files {
        let p = skills_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap();
        archive_entries.push((format!("skills/{rel}"), src));
    }

    // implementation modules of the two-layer built-ins: preset/plugins/lua/
    // <plugin>/<mod>.lua → module "<plugin>.<mod>", compiled and listed in builtin_modules.rs
    // (`(module name, bytecode)`, embedded by loader.rs); sources live in runtime_sources.deflate.
    let lua_dir = dir.join("lua");
    println!("cargo:rerun-if-changed={}", lua_dir.display());
    let mut mods = vec![];
    collect(&lua_dir, &lua_dir, &mut mods);
    mods.sort();
    let mut list = String::from("&[\n");
    for (module, rel) in mods {
        let p = lua_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap();
        let f = lua
            .load(&src)
            .set_name(format!("@builtin/lua/{rel}"))
            .into_function()
            .unwrap_or_else(|e| panic!("compiling preset/plugins/lua/{rel}: {e}"));
        let luac = out.join("lua").join(rel.replace(".lua", ".luac"));
        std::fs::create_dir_all(luac.parent().unwrap()).unwrap();
        std::fs::write(&luac, f.dump(false)).unwrap();
        let _ = writeln!(list, "    ({module:?}, include_bytes!({:?}) as &[u8]),", luac.display().to_string());
        archive_entries.push((format!("plugins/lua/{rel}"), src));
    }
    list.push(']');
    std::fs::write(out_dir.join("builtin_modules.rs"), list).unwrap();

    // Write the deflate-compressed archive of all runtime-extraction source files.
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    for (key, val) in &archive_entries {
        encoder.write_all(&(key.len() as u32).to_le_bytes()).unwrap();
        encoder.write_all(key.as_bytes()).unwrap();
        encoder.write_all(&(val.len() as u32).to_le_bytes()).unwrap();
        encoder.write_all(val.as_bytes()).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    std::fs::write(out_dir.join("runtime_sources.deflate"), compressed).unwrap();
}

fn compile_service_contracts(lua: &mlua::Lua, preset_dir: &Path, out_dir: &Path, write_luac: bool) {
    let contracts_dir = preset_dir.join("types/mare/services/contracts");
    println!("cargo:rerun-if-changed={}", contracts_dir.display());
    let schema_path = contracts_dir.join("schema.lua");
    println!("cargo:rerun-if-changed={}", schema_path.display());
    let schema = std::fs::read_to_string(&schema_path).unwrap();
    let mut components = String::new();
    write_json_as_lua(&mare_protocol::protocol_components(), &mut components);
    let mut chunk =
        format!("local S = (function(...)\n{schema}\nend)()\nlocal C = {components}\nlocal contracts = {{}}\n");
    for file in CONTRACT_FILES {
        let p = contracts_dir.join(file);
        println!("cargo:rerun-if-changed={}", p.display());
        let part = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()));
        let _ = writeln!(
            chunk,
            "for name, contract in pairs((function(...)\n{part}\nend)(S, C)) do\n  assert(contracts[name] == nil, 'contract ' .. name .. ' is declared twice')\n  contracts[name] = contract\nend"
        );
    }
    chunk.push_str("return contracts\n");
    let f = lua
        .load(&chunk)
        .set_name("@builtin/service-contracts")
        .into_function()
        .unwrap_or_else(|e| panic!("compiling @builtin/service-contracts: {e}"));
    if write_luac {
        std::fs::write(out_dir.join("bytecode/service_contracts.luac"), f.dump(false)).unwrap();
    }
    let evaluated: mlua::Value = f.call(()).unwrap_or_else(|e| panic!("evaluating @builtin/service-contracts: {e}"));
    let json_val = strict_lua_to_json_build(lua, evaluated)
        .unwrap_or_else(|e| panic!("serializing @builtin/service-contracts to JSON: {e}"));
    let parsed = mare_kernel::contracts::parse_contracts_json_owned(json_val)
        .unwrap_or_else(|e| panic!("validating @builtin/service-contracts at build time: {e}"));
    emit_static_contracts(&parsed, out_dir);
}

#[derive(Hash, PartialEq, Eq)]
enum JsonInternKey {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(u64),
    Str(String),
    Array(Vec<u32>),
    Object(Vec<(String, u32)>),
}

struct JsonPoolBuilder {
    index: std::collections::HashMap<JsonInternKey, u32>,
    entries: Vec<String>,
}

impl JsonPoolBuilder {
    fn new() -> Self {
        Self { index: std::collections::HashMap::new(), entries: Vec::new() }
    }

    fn intern(&mut self, val: &serde_json::Value) -> u32 {
        use serde_json::Value as J;
        let (key, code) = match val {
            J::Null => (JsonInternKey::Null, "StaticJson::Null".to_string()),
            J::Bool(b) => (JsonInternKey::Bool(*b), format!("StaticJson::Bool({b})")),
            J::Number(n) => {
                if let Some(i) = n.as_i64() {
                    (JsonInternKey::I64(i), format!("StaticJson::I64({i}_i64)"))
                } else if let Some(u) = n.as_u64() {
                    (JsonInternKey::U64(u), format!("StaticJson::U64({u}_u64)"))
                } else {
                    let bits = n.as_f64().expect("finite json number").to_bits();
                    (JsonInternKey::F64(bits), format!("StaticJson::F64({bits}_u64)"))
                }
            }
            J::String(s) => (JsonInternKey::Str(s.clone()), format!("StaticJson::Str({s:?})")),
            J::Array(items) => {
                let ids: Vec<u32> = items.iter().map(|it| self.intern(it)).collect();
                let mut s = String::from("StaticJson::Array(&[");
                for (i, id) in ids.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    let _ = write!(s, "{id}");
                }
                s.push_str("])");
                (JsonInternKey::Array(ids), s)
            }
            J::Object(map) => {
                let pairs: Vec<(String, u32)> = map.iter().map(|(k, v)| (k.clone(), self.intern(v))).collect();
                let mut s = String::from("StaticJson::Object(&[");
                for (i, (k, id)) in pairs.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    let _ = write!(s, "({k:?}, {id})");
                }
                s.push_str("])");
                (JsonInternKey::Object(pairs), s)
            }
        };
        if let Some(&id) = self.index.get(&key) {
            return id;
        }
        let id = self.entries.len() as u32;
        self.entries.push(code);
        self.index.insert(key, id);
        id
    }
}

fn fmt_str_slice(items: &[String]) -> String {
    let mut s = String::from("&[");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{it:?}");
    }
    s.push(']');
    s
}

fn fmt_opt_str(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("Some({s:?})"),
        None => "None".to_string(),
    }
}

fn fmt_opt_str_slice(v: &Option<Vec<String>>) -> String {
    match v {
        Some(items) => format!("Some({})", fmt_str_slice(items)),
        None => "None".to_string(),
    }
}

fn fmt_opt_u32(v: Option<u32>) -> String {
    match v {
        Some(id) => format!("Some({id})"),
        None => "None".to_string(),
    }
}

fn emit_static_contracts(parsed: &std::collections::BTreeMap<String, mare_kernel::ServiceContract>, out_dir: &Path) {
    use mare_kernel::contracts::{
        Access, EventDelivery, EventMode, EventOnError, EventReach, EventRetain, EventScope, Expose, MethodProvider,
        ReleaseOn, Scope,
    };

    let mut pool = JsonPoolBuilder::new();
    let mut contracts_code = String::new();
    contracts_code.push_str("pub(super) const BUILTIN_CONTRACTS: &[(&str, StaticServiceContract)] = &[\n");
    for (name, c) in parsed {
        let _ = writeln!(contracts_code, "    ({name:?}, StaticServiceContract {{");
        let _ = writeln!(contracts_code, "        required: {},", fmt_str_slice(&c.required));
        let _ = writeln!(contracts_code, "        optional: {},", fmt_str_slice(&c.optional));
        let _ = writeln!(contracts_code, "        doc: {},", fmt_opt_str(&c.doc));
        contracts_code.push_str("        methods: &[\n");
        for (mname, m) in &c.methods {
            let params_id = pool.intern(&m.params);
            let result_id = pool.intern(&m.result);
            let expose = match m.expose {
                Expose::Lua => "Expose::Lua",
                Expose::Both => "Expose::Both",
            };
            let provider = match m.provider {
                MethodProvider::Plugin => "MethodProvider::Plugin",
                MethodProvider::Host => "MethodProvider::Host",
            };
            let access = match m.access {
                Access::Primary => "Access::Primary",
                Access::Peer => "Access::Peer",
                Access::PeerDefault => "Access::PeerDefault",
            };
            let scope = match m.scope {
                Scope::Host => "Scope::Host",
                Scope::Workspace => "Scope::Workspace",
                Scope::Session => "Scope::Session",
            };
            let release_on = match m.release_on {
                ReleaseOn::Callbacks => "ReleaseOn::Callbacks",
                ReleaseOn::UiDetach => "ReleaseOn::UiDetach",
                ReleaseOn::Never => "ReleaseOn::Never",
            };
            let _ = writeln!(contracts_code, "            ({mname:?}, StaticMethodSpec {{");
            let _ = writeln!(contracts_code, "                doc: {:?},", m.doc);
            let _ = writeln!(contracts_code, "                expose: {expose},");
            let _ = writeln!(contracts_code, "                provider: {provider},");
            let _ = writeln!(contracts_code, "                access: {access},");
            let _ = writeln!(contracts_code, "                admin: {},", m.admin);
            let _ = writeln!(contracts_code, "                access_declared: {},", m.access_declared);
            let _ = writeln!(contracts_code, "                scope: {scope},");
            let _ = writeln!(contracts_code, "                params: {params_id},");
            let _ = writeln!(contracts_code, "                order: {},", fmt_str_slice(&m.order));
            let _ = writeln!(contracts_code, "                rest: {},", m.rest);
            let _ = writeln!(contracts_code, "                result: {result_id},");
            let _ = writeln!(contracts_code, "                lua_result: {},", fmt_opt_str_slice(&m.lua_result));
            let _ = writeln!(contracts_code, "                registration: {},", m.registration);
            let _ = writeln!(contracts_code, "                optional: {},", m.optional);
            contracts_code.push_str("                callbacks: &[");
            for (i, (ck, cv)) in m.callbacks.iter().enumerate() {
                if i > 0 {
                    contracts_code.push_str(", ");
                }
                let _ = write!(contracts_code, "({ck:?}, {cv:?})");
            }
            contracts_code.push_str("],\n");
            contracts_code.push_str("                remote_callbacks: &[\n");
            for (rk, rc) in &m.remote_callbacks {
                let rc_params = pool.intern(&rc.params);
                let rc_result = pool.intern(&rc.result);
                let rc_update = rc.update.as_ref().map(|u| pool.intern(u));
                let _ = writeln!(
                    contracts_code,
                    "                    ({rk:?}, StaticCallbackSpec {{ luals: {:?}, params: {rc_params}, order: {}, result: {rc_result}, update: {}, optional: {} }}),",
                    rc.luals,
                    fmt_str_slice(&rc.order),
                    fmt_opt_u32(rc_update),
                    rc.optional,
                );
            }
            contracts_code.push_str("                ],\n");
            let _ = writeln!(contracts_code, "                errors: {},", fmt_opt_str_slice(&m.errors));
            let _ = writeln!(contracts_code, "                poisoned: {:?},", m.poisoned);
            let _ = writeln!(contracts_code, "                confined: {},", m.confined);
            let _ = writeln!(contracts_code, "                baseline: {},", m.baseline);
            let _ = writeln!(contracts_code, "                host_local: {},", m.host_local);
            let _ = writeln!(contracts_code, "                host_anchor: {},", m.host_anchor);
            let _ = writeln!(contracts_code, "                replace_key: {},", fmt_opt_str(&m.replace_key));
            let _ = writeln!(contracts_code, "                release_on: {release_on},");
            let _ = writeln!(contracts_code, "                require_explicit_scope: {},", m.require_explicit_scope);
            let _ = writeln!(
                contracts_code,
                "                requires_capabilities: {},",
                fmt_str_slice(&m.requires_capabilities)
            );
            let _ = writeln!(contracts_code, "                project_scoped: {},", m.project_scoped);
            let _ = writeln!(contracts_code, "                lua_unchecked: {},", m.lua_unchecked);
            let _ = writeln!(contracts_code, "                history_result: {},", m.history_result);
            let _ = writeln!(contracts_code, "                lua_kind_errors: {},", m.lua_kind_errors);
            contracts_code.push_str("            }),\n");
        }
        contracts_code.push_str("        ],\n");
        contracts_code.push_str("        errors: &[\n");
        for (ename, e) in &c.errors {
            let data_id = e.data.as_ref().map(|d| pool.intern(d));
            let _ = writeln!(
                contracts_code,
                "            ({ename:?}, StaticErrorSpec {{ doc: {:?}, data: {} }}),",
                e.doc,
                fmt_opt_u32(data_id),
            );
        }
        contracts_code.push_str("        ],\n");
        let _ = writeln!(contracts_code, "        essential: {},", c.essential);
        match &c.poisoned_allow {
            Some(set) => {
                let items: Vec<String> = set.iter().cloned().collect();
                let _ = writeln!(contracts_code, "        poisoned_allow: Some({}),", fmt_str_slice(&items));
            }
            None => contracts_code.push_str("        poisoned_allow: None,\n"),
        }
        let _ = writeln!(contracts_code, "        owner: {},", fmt_opt_str(&c.owner));
        contracts_code.push_str("        events: &[\n");
        for (evk, ev) in &c.events {
            let data_id = pool.intern(&ev.data);
            let example_id = ev.example.as_ref().map(|v| pool.intern(v));
            let result_id = ev.result.as_ref().map(|v| pool.intern(v));
            let mode = match ev.mode {
                EventMode::Emit => "EventMode::Emit",
                EventMode::Parallel => "EventMode::Parallel",
                EventMode::Serial => "EventMode::Serial",
                EventMode::Bail => "EventMode::Bail",
                EventMode::Waterfall => "EventMode::Waterfall",
                EventMode::Policy => "EventMode::Policy",
                EventMode::Rewrite => "EventMode::Rewrite",
            };
            let expose = match ev.expose {
                EventReach::Process => "EventReach::Process",
                EventReach::Lua => "EventReach::Lua",
                EventReach::Both => "EventReach::Both",
            };
            let scope = match ev.scope {
                EventScope::Host => "EventScope::Host",
                EventScope::Session => "EventScope::Session",
                EventScope::Turn => "EventScope::Turn",
            };
            let delivery = match ev.delivery {
                EventDelivery::Required => "EventDelivery::Required",
                EventDelivery::Lossy => "EventDelivery::Lossy",
            };
            let on_error = match ev.on_error {
                EventOnError::Continue => "EventOnError::Continue",
                EventOnError::Abort => "EventOnError::Abort",
            };
            let retain = match &ev.retain {
                None => "None".to_string(),
                Some(EventRetain::Last) => "Some(StaticEventRetain::Last)".to_string(),
                Some(EventRetain::Key { key, cleared_by }) => {
                    format!("Some(StaticEventRetain::Key {{ key: {key:?}, cleared_by: {cleared_by:?} }})")
                }
            };
            let _ = writeln!(contracts_code, "            ({evk:?}, StaticEventSpec {{");
            let _ = writeln!(contracts_code, "                name: {:?},", ev.name);
            let _ = writeln!(contracts_code, "                short_name: {:?},", ev.short_name);
            let _ = writeln!(contracts_code, "                root: {:?},", ev.root);
            let _ = writeln!(contracts_code, "                owner: {:?},", ev.owner);
            let _ = writeln!(contracts_code, "                doc: {:?},", ev.doc);
            let _ = writeln!(contracts_code, "                data: {data_id},");
            let _ = writeln!(contracts_code, "                has_data_schema: {},", ev.has_data_schema);
            let _ = writeln!(contracts_code, "                mode: {mode},");
            let _ = writeln!(contracts_code, "                expose: {expose},");
            let _ = writeln!(contracts_code, "                scope: {scope},");
            let _ = writeln!(contracts_code, "                delivery: {delivery},");
            let _ = writeln!(contracts_code, "                ignorable: {},", ev.ignorable);
            let _ = writeln!(contracts_code, "                retain: {retain},");
            let _ = writeln!(contracts_code, "                on_error: {on_error},");
            let _ = writeln!(contracts_code, "                affirmative: {},", ev.affirmative);
            let _ = writeln!(contracts_code, "                join_ask_reasons: {},", ev.join_ask_reasons);
            let _ =
                writeln!(contracts_code, "                authority_service: {},", fmt_opt_str(&ev.authority_service));
            let _ = writeln!(contracts_code, "                example: {},", fmt_opt_u32(example_id));
            let _ = writeln!(contracts_code, "                result: {},", fmt_opt_u32(result_id));
            let _ = writeln!(contracts_code, "                emitters: {},", fmt_str_slice(&ev.emitters));
            contracts_code.push_str("            }),\n");
        }
        contracts_code.push_str("        ],\n");
        contracts_code.push_str("        capabilities: &[\n");
        for (capk, cap) in &c.capabilities {
            let _ = writeln!(
                contracts_code,
                "            ({capk:?}, StaticCapabilitySpec {{ method: {:?}, doc: {:?} }}),",
                cap.method, cap.doc,
            );
        }
        contracts_code.push_str("        ],\n");
        contracts_code.push_str("    }),\n");
    }
    contracts_code.push_str("];\n");

    let mut out = String::with_capacity(contracts_code.len() + pool.entries.len() * 40);
    out.push_str("pub(super) const JSON_POOL: &[StaticJson] = &[\n");
    for entry in &pool.entries {
        let _ = writeln!(out, "    {entry},");
    }
    out.push_str("];\n");
    out.push_str(&contracts_code);
    std::fs::write(out_dir.join("builtin_contracts.rs"), out).unwrap();
}

fn strict_lua_to_json_build(lua: &mlua::Lua, value: mlua::Value) -> Result<serde_json::Value, String> {
    use mlua::Value as V;
    use serde_json::{Map, Number, Value as J};
    fn walk(lua: &mlua::Lua, v: V, depth: usize, stack: &mut Vec<usize>) -> Result<J, String> {
        if depth > 64 {
            return Err("exceeded JSON nesting depth".into());
        }
        Ok(match v {
            V::Nil => return Err("nil is not a JSON value".into()),
            V::LightUserData(ud) if ud.0.is_null() => J::Null,
            V::Boolean(b) => J::Bool(b),
            V::Integer(i) => J::Number(i.into()),
            V::Number(n) => J::Number(Number::from_f64(n).ok_or_else(|| "non-finite JSON number".to_string())?),
            V::String(s) => J::String(s.to_str().map_err(|_| "non-UTF-8 string".to_string())?.to_string()),
            V::Table(t) => {
                let marked_array = match t.metatable() {
                    None => false,
                    Some(mt) if mt == lua.array_metatable() => true,
                    Some(_) => return Err("unsupported metatable".into()),
                };
                let ptr = t.to_pointer() as usize;
                if stack.contains(&ptr) {
                    return Err("recursive table".into());
                }
                stack.push(ptr);
                let mut entries = Vec::new();
                for pair in t.pairs::<V, V>() {
                    entries.push(pair.map_err(|e| e.to_string())?);
                }
                let has_integer = entries.iter().any(|(k, _)| matches!(k, V::Integer(_)));
                let result = if marked_array || has_integer {
                    let mut values = vec![None; entries.len()];
                    for (k, v) in entries {
                        let i = match k {
                            V::Integer(i) if i > 0 && (i as usize) <= values.len() => i as usize - 1,
                            _ => return Err("array must have dense positive integer keys".into()),
                        };
                        values[i] = Some(walk(lua, v, depth + 1, stack)?);
                    }
                    J::Array(values.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| "sparse array".to_string())?)
                } else {
                    let mut values = Map::new();
                    for (k, v) in entries {
                        let key = match k {
                            V::String(s) => s.to_str().map_err(|_| "non-UTF-8 key".to_string())?.to_string(),
                            _ => return Err("object keys must be strings".into()),
                        };
                        values.insert(key, walk(lua, v, depth + 1, stack)?);
                    }
                    values.sort_keys();
                    J::Object(values)
                };
                stack.pop();
                result
            }
            _ => return Err("unsupported Lua value in service contract".into()),
        })
    }
    walk(lua, value, 0, &mut Vec::new())
}

fn write_json_as_lua(val: &serde_json::Value, out: &mut String) {
    match val {
        serde_json::Value::Null => out.push_str("nil"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        serde_json::Value::String(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        serde_json::Value::Array(items) => {
            out.push('{');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_as_lua(item, out);
            }
            out.push('}');
        }
        serde_json::Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('[');
                write_json_as_lua(&serde_json::Value::String(k.clone()), out);
                out.push_str("] = ");
                write_json_as_lua(v, out);
            }
            out.push('}');
        }
    }
}

/// `(module name, path relative to lua/)` for every `.lua` below `dir` (`a/b.lua` → `a.b`,
/// `a/init.lua` → `a`).
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            println!("cargo:rerun-if-changed={}", p.display());
            collect(root, &p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("lua") {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let module = rel.trim_end_matches(".lua").trim_end_matches("/init").replace('/', ".");
            out.push((module, rel));
        }
    }
}

/// Relative paths of every `.lua` file below `dir`.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            println!("cargo:rerun-if-changed={}", p.display());
            collect_files(root, &p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("lua") {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push(rel);
        }
    }
}

fn collect_all_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            println!("cargo:rerun-if-changed={}", p.display());
            collect_all_files(root, &p, out);
        } else if p.is_file() {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push(rel);
        }
    }
}

/// Extract a declared public module name from a `---@meta <name>` line (matching
/// `^---@meta[ \t]+([\w.-]+)` in `scripts/lua_api_rules.py`, excluding `_`).
fn parse_meta_module(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("---@meta")?;
    if !rest.chars().next()?.is_whitespace() {
        return None;
    }
    let trimmed = rest.trim_start();
    let len = trimmed.bytes().take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')).count();
    if len == 0 {
        return None;
    }
    let name = &trimmed[..len];
    (name != "_").then_some(name)
}

fn generate_public_modules(preset_dir: &Path, out_dir: &Path) {
    let types_dir = preset_dir.join("types");
    println!("cargo:rerun-if-changed={}", types_dir.display());
    let mut files = Vec::new();
    collect_files(&types_dir, &types_dir, &mut files);
    files.sort();
    let mut modules = std::collections::BTreeSet::new();
    for rel in files {
        let p = types_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()));
        for line in src.lines() {
            if let Some(name) = parse_meta_module(line) {
                modules.insert(name.to_string());
            }
        }
    }
    let mut out = String::from("&[\n");
    for name in modules {
        let _ = writeln!(out, "    {name:?},");
    }
    out.push(']');
    std::fs::write(out_dir.join("public_modules.rs"), out).unwrap();
}

fn compile_config_engine(lua: &mlua::Lua, out: &Path) {
    let config_preset =
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates dir").join("mare-config").join("preset");
    for (file, chunk_name) in [
        ("config_api.lua", "@core/config_api"),
        ("config_api_ui_layout.lua", "@core/config_api_ui_layout"),
        ("config_api_validate.lua", "@core/config_api_validate"),
        ("config_packages.lua", "@core/config_packages"),
        ("config_introspect.lua", "@core/config_introspect"),
        ("config_txn.lua", "@core/config_txn"),
        ("plugin_config_api.lua", "@core/plugin_config_api"),
    ] {
        let p = config_preset.join(file);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()));
        let f = lua
            .load(&src)
            .set_name(chunk_name)
            .into_function()
            .unwrap_or_else(|e| panic!("compiling {}: {e}", p.display()));
        let stem = file.trim_end_matches(".lua");
        std::fs::write(out.join(format!("{stem}.luac")), f.dump(false)).unwrap();
    }
}

fn extract_builtin_config_meta(name_hint: &str, chunk: &mlua::Function) -> String {
    let def: mlua::Table = chunk
        .call(())
        .unwrap_or_else(|e| panic!("evaluating built-in plugin preset/plugins/{name_hint}.lua at build time: {e}"));
    let name = def.get::<Option<String>>("name").ok().flatten().unwrap_or_else(|| name_hint.to_string());
    let overrides = def.get::<Option<bool>>("override").ok().flatten().unwrap_or(false);
    let tags: Vec<String> = match def.get::<mlua::Value>("tags") {
        Ok(mlua::Value::Table(t)) => t.sequence_values::<String>().filter_map(Result::ok).collect(),
        _ => vec![],
    };
    let has_config_api = matches!(def.get::<mlua::Value>("config_api"), Ok(mlua::Value::Table(_)));
    let schema = match def.get::<mlua::Value>("config_schema") {
        Ok(mlua::Value::Table(t)) => Some(t),
        _ => None,
    };
    let has_config_schema = schema.is_some();
    let mut env_bindings = Vec::new();
    if let Some(ref s) = schema {
        collect_schema_env_bindings(s, &name, &format!("plugins.{name}"), None, &mut env_bindings)
            .unwrap_or_else(|e| panic!("extracting env bindings for preset/plugins/{name_hint}.lua: {e}"));
    }
    let has_env_bindings = !env_bindings.is_empty();
    let mut entry = String::new();
    let _ = writeln!(entry, "    BuiltinConfigMeta {{");
    let _ = writeln!(entry, "        name_hint: {name_hint:?},");
    let _ = writeln!(entry, "        name: {name:?},");
    let _ = writeln!(entry, "        overrides: {overrides},");
    let _ = write!(entry, "        tags: &[");
    for (i, t) in tags.iter().enumerate() {
        if i > 0 {
            entry.push_str(", ");
        }
        let _ = write!(entry, "{t:?}");
    }
    entry.push_str("],\n");
    let _ = writeln!(entry, "        has_config_api: {has_config_api},");
    let _ = writeln!(entry, "        has_config_schema: {has_config_schema},");
    let _ = writeln!(entry, "        has_env_bindings: {has_env_bindings},");
    entry.push_str("    },");
    entry
}

fn write_builtin_config_meta(out_dir: &Path, entries: &[String]) {
    let mut out = String::from("&[\n");
    for e in entries {
        out.push_str(e);
        out.push('\n');
    }
    out.push(']');
    std::fs::write(out_dir.join("builtin_config_meta.rs"), out).unwrap();
}

fn fnv1a_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn compute_runtime_fingerprint(preset_dir: &Path) -> String {
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    let plugins_dir = preset_dir.join("plugins");
    if let Ok(rd) = std::fs::read_dir(&plugins_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("lua")
                && let Some(name) = p.file_name().and_then(|s| s.to_str())
            {
                entries.push((format!("plugins/{name}"), p));
            }
        }
    }
    let lua_dir = plugins_dir.join("lua");
    println!("cargo:rerun-if-changed={}", lua_dir.display());
    let mut mods = Vec::new();
    collect(&lua_dir, &lua_dir, &mut mods);
    for (_, rel) in mods {
        let p = lua_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        entries.push((format!("plugins/lua/{rel}"), p));
    }
    for file in ["ui_prelude.lua", "ui_components.lua", "ui_shell.lua", "keymap.lua", "test.lua", "NEWS.md"] {
        let p = preset_dir.join(file);
        println!("cargo:rerun-if-changed={}", p.display());
        entries.push((file.to_string(), p));
    }
    let types_dir = preset_dir.join("types");
    let mut type_files = Vec::new();
    collect_files(&types_dir, &types_dir, &mut type_files);
    for rel in type_files {
        let p = types_dir.join(&rel);
        entries.push((format!("types/{rel}"), p));
    }
    let skills_dir = preset_dir.join("skills");
    println!("cargo:rerun-if-changed={}", skills_dir.display());
    let mut skill_files = Vec::new();
    collect_all_files(&skills_dir, &skills_dir, &mut skill_files);
    for rel in skill_files {
        let p = skills_dir.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        entries.push((format!("skills/{rel}"), p));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0xaf63_dc4c_8601_ec8c;
    for (key, path) in entries {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for part in [key.as_bytes(), b"\0", &bytes, b"\xff"] {
            h1 = fnv1a_update(h1, part);
            h2 = fnv1a_update(h2.rotate_left(7), part);
        }
    }
    format!("{h1:016x}{h2:016x}")
}
