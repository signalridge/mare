//! Lua runtime setup (Yazi `yazi-plugin` style): builds a VM with every `mare.*` primitive from
//! mare-binding, the UI prelude/components and config API from the preset, then freezes the
//! standard library. `preset/` holds the built-in Lua plugins, UI components and `types/`.

pub mod config_load;
mod config_origin;
mod contract_files;
pub mod engines;
pub mod flags;
pub mod loader;
mod lua_cov;
pub mod pkg_binding;
pub mod runtime;
#[cfg(not(debug_assertions))]
pub(crate) mod runtime_archive;
mod test_host;

use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Table};

pub use mare_shared::ui::Mode;

/// Built-in subagent global table and service namespace (`mare.subagent` / `contracts/subagent.lua`).
pub const SUBAGENT_GLOBAL_NAME: &str = "subagent";
/// Built-in webserver global table and service namespace (`mare.webserver` / `contracts/webserver.lua`).
pub const WEBSERVER_GLOBAL_NAME: &str = "webserver";
/// Format version for `$XDG_STATE_HOME/mare/news.json` (`preset/plugins/lua/plugins/news.lua`).
pub const NEWS_VERSION: u32 = 1;

fn preset_source(rel: &'static str) -> std::borrow::Cow<'static, str> {
    #[cfg(debug_assertions)]
    {
        let p = std::path::Path::new(mare_macro::preset_dir!()).join(rel);
        std::borrow::Cow::Owned(std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading preset/{rel}: {e}")))
    }
    #[cfg(not(debug_assertions))]
    {
        std::borrow::Cow::Owned(
            runtime_archive::get(rel).unwrap_or_else(|| panic!("missing preset/{rel} in runtime archive")),
        )
    }
}

pub fn ui_prelude() -> std::borrow::Cow<'static, str> {
    preset_source("ui_prelude.lua")
}
pub fn ui_components() -> std::borrow::Cow<'static, str> {
    preset_source("ui_components.lua")
}
pub fn ui_shell() -> std::borrow::Cow<'static, str> {
    preset_source("ui_shell.lua")
}
pub fn keymap_lua() -> std::borrow::Cow<'static, str> {
    preset_source("keymap.lua")
}
pub fn test_lua() -> std::borrow::Cow<'static, str> {
    preset_source("test.lua")
}

macro_rules! preset_chunk {
    ($fn_name:ident, $stem:literal, $text_fn:ident) => {
        pub(crate) fn $fn_name() -> std::borrow::Cow<'static, [u8]> {
            #[cfg(not(debug_assertions))]
            {
                std::borrow::Cow::Borrowed(include_bytes!(concat!(
                    env!("OUT_DIR"),
                    "/bytecode/preset_",
                    $stem,
                    ".luac"
                )))
            }
            #[cfg(debug_assertions)]
            {
                std::borrow::Cow::Owned($text_fn().into_owned().into_bytes())
            }
        }
    };
}

preset_chunk!(ui_prelude_chunk, "ui_prelude", ui_prelude);
preset_chunk!(ui_components_chunk, "ui_components", ui_components);
preset_chunk!(ui_shell_chunk, "ui_shell", ui_shell);
preset_chunk!(keymap_chunk, "keymap", keymap_lua);
preset_chunk!(test_chunk, "test", test_lua);
pub fn news_text() -> std::borrow::Cow<'static, str> {
    #[cfg(debug_assertions)]
    {
        preset_source("NEWS.md")
    }
    #[cfg(not(debug_assertions))]
    {
        use std::io::Read as _;
        static NEWS_DEFLATE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/news.deflate"));
        let mut decoder = flate2::read::DeflateDecoder::new(NEWS_DEFLATE);
        let mut out = String::new();
        decoder.read_to_string(&mut out).expect("invalid embedded news.deflate");
        std::borrow::Cow::Owned(out)
    }
}
/// LuaLS definitions, `(path under types/, text)`: the entry `mare.lua` (`---@class mare` and
/// its scalar fields) and one `mare/<namespace>.lua` per namespace. A test keeps the list
/// equal to `preset/types/` (`tests::types_list_matches_the_directory`).
pub fn types_texts() -> Vec<(&'static str, std::borrow::Cow<'static, str>)> {
    const TYPE_FILES: &[&str] = &[
        "mare.lua",
        "cordis.lua",
        "mare_lib.lua",
        "mare_lib/baseline_page.lua",
        "mare_lib/schema_sanitize.lua",
        "mare_lib/table_revision.lua",
        "mare/approvals.lua",
        "mare/subagent.lua",
        "mare/config.lua",
        "mare/plugin_options.gen.lua",
        "mare/events.lua",
        "mare/events.gen.lua",
        "mare/flags.lua",
        "mare/fs.lua",
        "mare/host/prims.lua",
        "mare/keymap.lua",
        "mare/loader.lua",
        "mare/host/loader.lua",
        "mare/host/kernel.lua",
        "mare/mcp.lua",
        "mare/net.lua",
        "mare/plugins.lua",
        "mare/presentations.lua",
        "mare/proc.lua",
        "mare/profile.lua",
        "mare/content.lua",
        "mare/sandbox.lua",
        "mare/schema.lua",
        "mare/services/agent.gen.lua",
        "mare/services/approvals.gen.lua",
        "mare/services/changes.gen.lua",
        "mare/services/commands.gen.lua",
        "mare/services/compact.gen.lua",
        "mare/services/context.gen.lua",
        "mare/services/conversations.gen.lua",
        "mare/services/copy.gen.lua",
        "mare/services/ctx.lua",
        "mare/services/images.gen.lua",
        "mare/services/models.lua",
        "mare/services/permissions.gen.lua",
        "mare/services/review.gen.lua",
        "mare/services/update-check.gen.lua",
        "mare/services/workspace.gen.lua",
        "mare/services/contracts/schema.lua",
        "mare/services/contracts/events.lua",
        "mare/services/contracts/agent.lua",
        "mare/services/contracts/compact.lua",
        "mare/services/contracts/context.lua",
        "mare/services/contracts/commands.lua",
        "mare/services/contracts/fs.lua",
        "mare/services/contracts/hooks.lua",
        "mare/services/contracts/conversations.lua",
        "mare/services/contracts/jobs.lua",
        "mare/services/contracts/kernel.lua",
        "mare/services/contracts/cells.lua",
        "mare/services/contracts/llm.lua",
        "mare/services/contracts/options.lua",
        "mare/services/contracts/mcp.lua",
        "mare/services/contracts/message.lua",
        "mare/services/contracts/approvals.lua",
        "mare/services/contracts/changes.lua",
        "mare/services/contracts/copy.lua",
        "mare/services/contracts/images.lua",
        "mare/services/contracts/permissions.lua",
        "mare/services/contracts/review.lua",
        "mare/services/contracts/update_check.lua",
        "mare/services/contracts/tools.lua",
        "mare/services/contracts/status.lua",
        "mare/services/contracts/query.lua",
        "mare/services/contracts/session.lua",
        "mare/services/contracts/subagent.lua",
        "mare/services/contracts/terminal.lua",
        "mare/services/contracts/plugins.lua",
        "mare/services/contracts/spill.lua",
        "mare/services/contracts/proc.lua",
        "mare/services/contracts/theme.lua",
        "mare/services/contracts/ui.lua",
        "mare/services/contracts/usage.lua",
        "mare/services/contracts/web.lua",
        "mare/services/contracts/workspace.lua",
        "mare/services/contracts/webserver.lua",
        "mare/services/fs.gen.lua",
        "mare/services/hooks.gen.lua",
        "mare/services/jobs.gen.lua",
        "mare/services/kernel.gen.lua",
        "mare/services/mcp.gen.lua",
        "mare/host/contracts.lua",
        "mare/host/bus.lua",
        "mare/host/policy.lua",
        "mare/host/service.lua",
        "mare/services/llm.gen.lua",
        "mare/services/options.gen.lua",
        "mare/services/query.gen.lua",
        "mare/services/plugins.gen.lua",
        "mare/services/spill.gen.lua",
        "mare/services/proc.gen.lua",
        "mare/services/message.gen.lua",
        "mare/services/cells.gen.lua",
        "mare/services/session.gen.lua",
        "mare/services/status.gen.lua",
        "mare/services/subagent.gen.lua",
        "mare/services/terminal.gen.lua",
        "mare/services/theme.gen.lua",
        "mare/services/tools.gen.lua",
        "mare/services/ui.gen.lua",
        "mare/services/usage.gen.lua",
        "mare/services/web.gen.lua",
        "mare/services/webserver.gen.lua",
        "mare/webserver.lua",
        "mare/stream.lua",
        "mare/std.lua",
        "mare/store.lua",
        "mare/spill.lua",
        "mare/state.lua",
        "mare/task.lua",
        "mare/test.lua",
        "mare/time.lua",
        "mare/text.lua",
        "mare/ui.lua",
        "mare/workspace.lua",
    ];
    #[cfg(not(debug_assertions))]
    {
        let mut archive = runtime_archive::extract_all();
        TYPE_FILES
            .iter()
            .map(|&rel| {
                let text = archive
                    .remove(&format!("types/{rel}"))
                    .unwrap_or_else(|| panic!("missing preset/types/{rel} in runtime archive"));
                (rel, std::borrow::Cow::Owned(text))
            })
            .collect()
    }
    #[cfg(debug_assertions)]
    {
        let root = std::path::Path::new(mare_macro::preset_dir!()).join("types");
        TYPE_FILES
            .iter()
            .map(|&rel| {
                let text = std::fs::read_to_string(root.join(rel))
                    .unwrap_or_else(|e| panic!("reading preset/types/{rel}: {e}"));
                (rel, std::borrow::Cow::Owned(text))
            })
            .collect()
    }
}

/// Bundled skill files, `(path under skills/, text)`: every file shipped under `preset/skills/`.
pub fn skill_texts() -> Vec<(String, std::borrow::Cow<'static, str>)> {
    #[cfg(not(debug_assertions))]
    {
        let mut entries: Vec<_> = runtime_archive::list_prefix("skills/")
            .into_iter()
            .map(|(rel, text)| (rel, std::borrow::Cow::Owned(text)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
    #[cfg(debug_assertions)]
    {
        let root = std::path::Path::new(mare_macro::preset_dir!()).join("skills");
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    let rel = p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                    let text =
                        std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("reading preset/skills/{rel}: {err}"));
                    out.push((rel, std::borrow::Cow::Owned(text)));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Build-time validated static built-in service contracts cached once per process behind shared
/// `Arc`s so startup, reloads, and child hosts materialize contracts directly from compile-time
/// data without runtime deflate decompression, JSON parsing, or schema re-validation.
pub fn builtin_service_contracts()
-> mlua::Result<Arc<std::collections::BTreeMap<String, Arc<mare_kernel::ServiceContract>>>> {
    type SharedContracts = Arc<std::collections::BTreeMap<String, Arc<mare_kernel::ServiceContract>>>;
    static CACHED: std::sync::OnceLock<SharedContracts> = std::sync::OnceLock::new();
    Ok(Arc::clone(CACHED.get_or_init(|| Arc::new(static_contracts::materialize()))))
}

mod static_contracts {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use mare_kernel::ServiceContract;
    use mare_kernel::contracts::{
        Access, CallbackSpec, CapabilitySpec, ErrorSpec, EventDelivery, EventMode, EventOnError, EventReach,
        EventRetain, EventScope, EventSpec, Expose, MethodProvider, MethodSpec, ReleaseOn, Scope,
    };
    use serde_json::{Map, Number, Value as Json};

    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) enum StaticJson {
        Null,
        Bool(bool),
        I64(i64),
        U64(u64),
        F64(u64),
        Str(&'static str),
        Array(&'static [u32]),
        Object(&'static [(&'static str, u32)]),
    }

    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) enum StaticEventRetain {
        Last,
        Key { key: &'static str, cleared_by: &'static str },
    }

    pub(super) struct StaticCallbackSpec {
        pub luals: &'static str,
        pub params: u32,
        pub order: &'static [&'static str],
        pub result: u32,
        pub update: Option<u32>,
        pub optional: bool,
    }

    pub(super) struct StaticErrorSpec {
        pub doc: &'static str,
        pub data: Option<u32>,
    }

    pub(super) struct StaticCapabilitySpec {
        pub method: &'static str,
        pub doc: &'static str,
    }

    pub(super) struct StaticMethodSpec {
        pub doc: &'static str,
        pub expose: Expose,
        pub provider: MethodProvider,
        pub access: Access,
        pub admin: bool,
        pub access_declared: bool,
        pub scope: Scope,
        pub params: u32,
        pub order: &'static [&'static str],
        pub rest: bool,
        pub result: u32,
        pub lua_result: Option<&'static [&'static str]>,
        pub registration: bool,
        pub optional: bool,
        pub callbacks: &'static [(&'static str, &'static str)],
        pub remote_callbacks: &'static [(&'static str, StaticCallbackSpec)],
        pub errors: Option<&'static [&'static str]>,
        pub poisoned: Option<bool>,
        pub confined: bool,
        pub baseline: bool,
        pub host_local: bool,
        pub host_anchor: bool,
        pub replace_key: Option<&'static str>,
        pub release_on: ReleaseOn,
        pub require_explicit_scope: bool,
        pub requires_capabilities: &'static [&'static str],
        pub project_scoped: bool,
        pub lua_unchecked: bool,
        pub history_result: bool,
        pub lua_kind_errors: bool,
    }

    pub(super) struct StaticEventSpec {
        pub name: &'static str,
        pub short_name: &'static str,
        pub root: &'static str,
        pub owner: &'static str,
        pub doc: &'static str,
        pub data: u32,
        pub has_data_schema: bool,
        pub mode: EventMode,
        pub expose: EventReach,
        pub scope: EventScope,
        pub delivery: EventDelivery,
        pub ignorable: bool,
        pub retain: Option<StaticEventRetain>,
        pub on_error: EventOnError,
        pub affirmative: bool,
        pub join_ask_reasons: bool,
        pub authority_service: Option<&'static str>,
        pub example: Option<u32>,
        pub result: Option<u32>,
        pub emitters: &'static [&'static str],
    }

    pub(super) struct StaticServiceContract {
        pub required: &'static [&'static str],
        pub optional: &'static [&'static str],
        pub doc: Option<&'static str>,
        pub methods: &'static [(&'static str, StaticMethodSpec)],
        pub errors: &'static [(&'static str, StaticErrorSpec)],
        pub essential: bool,
        pub poisoned_allow: Option<&'static [&'static str]>,
        pub owner: Option<&'static str>,
        pub events: &'static [(&'static str, StaticEventSpec)],
        pub capabilities: &'static [(&'static str, StaticCapabilitySpec)],
    }

    include!(concat!(env!("OUT_DIR"), "/builtin_contracts.rs"));

    fn to_json(id: u32) -> Json {
        match JSON_POOL[id as usize] {
            StaticJson::Null => Json::Null,
            StaticJson::Bool(b) => Json::Bool(b),
            StaticJson::I64(i) => Json::Number(i.into()),
            StaticJson::U64(u) => Json::Number(u.into()),
            StaticJson::F64(bits) => Json::Number(Number::from_f64(f64::from_bits(bits)).expect("finite static json")),
            StaticJson::Str(s) => Json::String(s.to_owned()),
            StaticJson::Array(ids) => {
                let mut out = Vec::with_capacity(ids.len());
                for &cid in ids {
                    out.push(to_json(cid));
                }
                Json::Array(out)
            }
            StaticJson::Object(entries) => {
                let mut out = Map::with_capacity(entries.len());
                for &(k, cid) in entries {
                    out.insert(k.to_owned(), to_json(cid));
                }
                Json::Object(out)
            }
        }
    }

    fn str_vec(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    pub(super) fn materialize() -> BTreeMap<String, Arc<ServiceContract>> {
        let mut out = BTreeMap::new();
        for &(name, ref c) in BUILTIN_CONTRACTS {
            let methods = c
                .methods
                .iter()
                .map(|&(mname, ref m)| {
                    let callbacks = m.callbacks.iter().map(|&(k, v)| (k.to_owned(), v.to_owned())).collect();
                    let remote_callbacks = m
                        .remote_callbacks
                        .iter()
                        .map(|&(k, ref rc)| {
                            (
                                k.to_owned(),
                                CallbackSpec {
                                    luals: rc.luals.to_owned(),
                                    params: to_json(rc.params),
                                    order: str_vec(rc.order),
                                    result: to_json(rc.result),
                                    update: rc.update.map(to_json),
                                    optional: rc.optional,
                                },
                            )
                        })
                        .collect();
                    (
                        mname.to_owned(),
                        MethodSpec {
                            doc: m.doc.to_owned(),
                            expose: m.expose,
                            provider: m.provider,
                            access: m.access,
                            admin: m.admin,
                            access_declared: m.access_declared,
                            scope: m.scope,
                            params: to_json(m.params),
                            order: str_vec(m.order),
                            rest: m.rest,
                            result: to_json(m.result),
                            lua_result: m.lua_result.map(str_vec),
                            registration: m.registration,
                            optional: m.optional,
                            callbacks,
                            remote_callbacks,
                            errors: m.errors.map(str_vec),
                            poisoned: m.poisoned,
                            confined: m.confined,
                            baseline: m.baseline,
                            host_local: m.host_local,
                            host_anchor: m.host_anchor,
                            replace_key: m.replace_key.map(str::to_owned),
                            release_on: m.release_on,
                            require_explicit_scope: m.require_explicit_scope,
                            requires_capabilities: str_vec(m.requires_capabilities),
                            project_scoped: m.project_scoped,
                            lua_unchecked: m.lua_unchecked,
                            history_result: m.history_result,
                            lua_kind_errors: m.lua_kind_errors,
                        },
                    )
                })
                .collect();
            let errors = c
                .errors
                .iter()
                .map(|&(k, ref e)| (k.to_owned(), ErrorSpec { doc: e.doc.to_owned(), data: e.data.map(to_json) }))
                .collect();
            let events = c
                .events
                .iter()
                .map(|&(k, ref ev)| {
                    (
                        k.to_owned(),
                        EventSpec {
                            name: ev.name.to_owned(),
                            short_name: ev.short_name.to_owned(),
                            root: ev.root.to_owned(),
                            owner: ev.owner.to_owned(),
                            doc: ev.doc.to_owned(),
                            data: to_json(ev.data),
                            has_data_schema: ev.has_data_schema,
                            mode: ev.mode,
                            expose: ev.expose,
                            scope: ev.scope,
                            delivery: ev.delivery,
                            ignorable: ev.ignorable,
                            retain: ev.retain.map(|r| match r {
                                StaticEventRetain::Last => EventRetain::Last,
                                StaticEventRetain::Key { key, cleared_by } => {
                                    EventRetain::Key { key: key.to_owned(), cleared_by: cleared_by.to_owned() }
                                }
                            }),
                            on_error: ev.on_error,
                            affirmative: ev.affirmative,
                            join_ask_reasons: ev.join_ask_reasons,
                            authority_service: ev.authority_service.map(str::to_owned),
                            example: ev.example.map(to_json),
                            result: ev.result.map(to_json),
                            emitters: str_vec(ev.emitters),
                        },
                    )
                })
                .collect();
            let capabilities = c
                .capabilities
                .iter()
                .map(|&(k, ref cap)| {
                    (k.to_owned(), CapabilitySpec { method: cap.method.to_owned(), doc: cap.doc.to_owned() })
                })
                .collect();
            out.insert(
                name.to_owned(),
                Arc::new(ServiceContract {
                    required: str_vec(c.required),
                    optional: str_vec(c.optional),
                    doc: c.doc.map(str::to_owned),
                    methods,
                    errors,
                    essential: c.essential,
                    poisoned_allow: c.poisoned_allow.map(|s| s.iter().map(|&v| v.to_owned()).collect::<BTreeSet<_>>()),
                    owner: c.owner.map(str::to_owned),
                    events,
                    capabilities,
                }),
            );
        }
        out
    }
}

/// Built-in service method and event contracts as a loadable Lua chunk: precompiled Lua 5.5
/// bytecode in release builds (compiled at build time in `build.rs`), live source text in debug
/// builds.
pub fn service_contracts_chunk() -> std::borrow::Cow<'static, [u8]> {
    #[cfg(not(debug_assertions))]
    {
        std::borrow::Cow::Borrowed(include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/service_contracts.luac")))
    }
    #[cfg(debug_assertions)]
    {
        std::borrow::Cow::Owned(service_contracts_lua().into_owned().into_bytes())
    }
}

/// Built-in service method and event contracts, loaded by the host before plugin activation.
pub fn service_contracts_lua() -> std::borrow::Cow<'static, str> {
    #[cfg(not(debug_assertions))]
    {
        static CACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        return std::borrow::Cow::Borrowed(CACHED.get_or_init(build_service_contracts_lua).as_str());
    }
    #[cfg(debug_assertions)]
    {
        std::borrow::Cow::Owned(build_service_contracts_lua())
    }
}

fn build_service_contracts_lua() -> String {
    use std::fmt::Write as _;
    // one manifest from the per-service files: schema.lua's helpers and expanded protocol
    // components are passed to each contract file as `(S, C)`
    let components = mare_kernel::contracts::protocol_components_lua();
    #[cfg(not(debug_assertions))]
    let (schema, parts) = {
        let mut archive = runtime_archive::extract_all();
        let schema = archive
            .remove("types/mare/services/contracts/schema.lua")
            .expect("missing contracts/schema.lua in runtime archive");
        let parts: Vec<String> = contract_files::CONTRACT_FILES
            .iter()
            .map(|file| {
                archive
                    .remove(&format!("types/mare/services/contracts/{file}"))
                    .unwrap_or_else(|| panic!("missing contracts/{file} in runtime archive"))
            })
            .collect();
        (schema, parts)
    };
    #[cfg(debug_assertions)]
    let (schema, parts) = {
        let root = std::path::Path::new(mare_macro::preset_dir!()).join("types/mare/services/contracts");
        let schema = std::fs::read_to_string(root.join("schema.lua")).expect("reading contracts/schema.lua");
        let parts: Vec<String> = contract_files::CONTRACT_FILES
            .iter()
            .map(|file| {
                std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("reading contracts/{file}: {e}"))
            })
            .collect();
        (schema, parts)
    };
    let mut chunk =
        format!("local S = (function(...)\n{schema}\nend)()\nlocal C = {components}\nlocal contracts = {{}}\n");
    for part in parts {
        let _ = writeln!(
            chunk,
            "for name, contract in pairs((function(...)\n{part}\nend)(S, C)) do\n  assert(contracts[name] == nil, 'contract ' .. name .. ' is declared twice')\n  contracts[name] = contract\nend"
        );
    }
    chunk.push_str("return contracts\n");
    chunk
}

pub struct Env {
    pub cwd: PathBuf,
    pub mode: Mode,
    pub ui: Arc<mare_shared::ui::UiShared>,
    pub config: serde_json::Value,
    pub args: serde_json::Value,
}

/// Default Lua VM memory limit in MiB (0 disables the limit).
pub const DEFAULT_LUA_MEMORY_MB: usize = 512;

/// Configure the Lua VM memory limit in MiB (0 disables the limit).
pub fn set_memory_limit_mb(lua: &Lua, mb: usize) -> mlua::Result<usize> {
    lua.set_memory_limit(mb.saturating_mul(1024 * 1024))
}

/// The generational GC's minor multiplier (`collectgarbage("param", "minormul")`, Lua default 20):
/// a minor collection runs once the heap has grown this many percent since the last major one.
/// A full `/reload` restarts every plugin, building fibers that survive the young generation, and
/// at 20 it spent ~10% of its time in minor steps. Measured A/B on one binary (RELOAD-TIME,
/// 2026-11-18, debug, load 25-30): reload ~300 -> ~270 ms with peak heap 19.6 -> 29.6 MB; a long
/// streaming turn, the first frame and `-p` to its first request unchanged; no visible pause; a
/// 20-reload session flat. 200 made reload faster still but streaming turns 3-13% slower. The
/// root fix is fewer allocations per plugin fiber; this only stops collecting too eagerly.
pub const GC_MINOR_MULTIPLIER: i32 = 100;

pub fn new_lua() -> mlua::Result<Lua> {
    let lua = Lua::new();
    // Lua 5.5 generational GC: short-lived tables (events, stream chunks) die young
    lua.gc_set_mode(mlua::state::GcMode::Generational(
        mlua::state::GcGenParams::default().minor_multiplier(GC_MINOR_MULTIPLIER),
    ));
    set_memory_limit_mb(&lua, DEFAULT_LUA_MEMORY_MB)?;
    lua_cov::maybe_attach(&lua)?;
    Ok(lua)
}

/// Flush any recorded Lua line coverage for `lua` to `MARE_LUA_COV_DIR`.
pub fn flush_lua_cov(lua: &Lua) {
    lua_cov::flush(lua);
}

pub use mare_shared::lua::isolated_env;

/// Redirect Lua `print`, `io.stdout`, `io.output()`, and `io.write` away from protocol stdout
/// to stderr in structured stdio modes (`--json`, `--rpc`, `acp`) before freezing stdlib.
fn redirect_lua_stdout(lua: &Lua) -> mlua::Result<()> {
    let tostring: mlua::Function = lua.globals().get("tostring")?;
    let print_fn = lua.create_function(move |_, args: mlua::MultiValue| {
        use std::io::Write;
        let mut out = String::new();
        for (idx, val) in args.into_iter().enumerate() {
            if idx > 0 {
                out.push('\t');
            }
            let s: mlua::LuaString = tostring.call(val)?;
            out.push_str(&s.to_string_lossy());
        }
        let _ = writeln!(std::io::stderr(), "{out}");
        Ok(())
    })?;
    lua.globals().set("print", print_fn)?;
    lua.load("io.output(io.stderr); io.stdout = io.stderr").set_name("@core/stdout_guard").exec()
}

/// Make the standard library tables read-only for new keys and lock their metatables.
fn freeze_stdlib(lua: &Lua) -> mlua::Result<()> {
    lua.load(
        r#"
        local function deny(_, k) error("attempt to modify standard library field '" .. tostring(k) .. "'", 2) end
        for _, name in ipairs({ "string", "table", "math", "utf8", "coroutine", "os", "io", "debug", "package" }) do
            local lib = _G[name]
            if type(lib) == "table" and getmetatable(lib) == nil then
                setmetatable(lib, { __newindex = deny, __metatable = false })
            end
        end
        "#,
    )
    .set_name("@core/freeze")
    .exec()
}

/// Install every primitive into `lua` as the global `mare`.
pub fn install(lua: &Lua, env: &Env) -> mlua::Result<Table> {
    let mare = lua.create_table()?;
    use mare_binding::*;
    task::install(lua, &mare)?;
    mare_binding::time::install(lua, &mare)?;
    proc::install(lua, &mare, env.cwd.to_string_lossy().into_owned())?;
    job::install(lua, &mare, env.cwd.to_string_lossy().into_owned())?;
    fs::install(lua, &mare, env.cwd.clone())?;
    http::install(lua, &mare)?;
    mare_binding::crashguard::install(lua, &mare)?;
    mare_binding::snapshot::install(lua, &mare)?;
    mare_binding::activity::install(lua, &mare)?;
    flags::install(lua, &mare, env.args.get("flags"))?;
    pkg_binding::install(lua, &mare)?;
    mare_binding::mcp::install(lua, &mare)?;
    mare_binding::filelock::install(lua, &mare)?;
    mare_binding::statefile::install(lua, &mare)?;
    mare_binding::spill::install(lua, &mare, env.cwd.clone())?;
    net_unix::install(lua, &mare)?;
    peers::install(lua, &mare)?;
    codec::install(lua, &mare)?;
    approval::install(lua, &mare)?;
    streaming::install(lua, &mare)?;
    text::install(lua, &mare, env.cwd.clone())?;
    mare_binding::projection::install(lua, &mare)?;
    store::install(lua, &mare)?;
    let core = mare_shared::lua::core::ensure(lua)?;
    let ui_table = engines::ui::install(lua, &mare, env.ui.clone())?;
    engines::keymap::install(lua, &mare, &ui_table)?;
    let snapshot_io = build_snapshot_io(lua)?;
    let test_host_factory = test_host::build_factory(lua)?;
    let (test_table, test_runner): (Table, mlua::Function) =
        lua.load(test_chunk().as_ref()).set_name("@preset/test.lua").call((snapshot_io, core, test_host_factory))?;
    mare.set("test", test_table.clone())?;
    lua.globals().get::<Table>("package")?.get::<Table>("loaded")?.set("mare.test", test_table)?;
    lua.set_named_registry_value("mare_test_runner", test_runner)?;
    image::install(lua, &mare, env.cwd.clone())?;
    mare_binding::clipboard::install(lua, &mare, env.ui.clone())?;
    pty::install(lua, &mare, env.cwd.to_string_lossy().into_owned())?;
    sandbox::install(lua, &mare)?;
    workspace::install(lua, &mare, env.cwd.clone())?;
    exec_ctx::install(lua, &mare)?;

    mare.set("version", env!("CARGO_PKG_VERSION"))?;
    mare.set("api_version", env!("MARE_API_VERSION"))?;
    // development builds (tests, `cargo run`) — e.g. no automatic update check from them
    mare.set("debug_build", cfg!(debug_assertions))?;
    mare.set("cwd", env.cwd.to_string_lossy().into_owned())?;
    mare.set("mode", env.mode.as_str())?;
    mare.set("pid", std::process::id())?;
    mare.set("os", std::env::consts::OS)?;
    mare.set("arch", std::env::consts::ARCH)?;
    let config = match mare_shared::lua::json_to_lua(lua, &env.config)? {
        mlua::Value::Table(t) => t,
        _ => lua.create_table()?,
    };
    mare.set("config", config.clone())?;
    mare.set("args", mare_shared::lua::json_to_lua(lua, &env.args)?)?;
    let paths = lua.create_table()?;
    paths.set("config", mare_shared::paths::config_dir().to_string_lossy().into_owned())?;
    paths.set("state", mare_shared::paths::state_dir().to_string_lossy().into_owned())?;
    paths.set("data", mare_shared::paths::data_dir().to_string_lossy().into_owned())?;
    paths.set("auth", mare_shared::paths::auth_file().to_string_lossy().into_owned())?;
    paths.set("sessions", mare_shared::paths::sessions_dir().to_string_lossy().into_owned())?;
    paths.set("packages", mare_shared::paths::packages_dir().to_string_lossy().into_owned())?;
    paths.set("runtime", runtime::dir().to_string_lossy().into_owned())?;
    paths.set("log", mare_shared::paths::log_dir().to_string_lossy().into_owned())?;
    paths.set("crashes", mare_shared::paths::crash_dir().to_string_lossy().into_owned())?;
    mare.set("paths", paths)?;
    let rr = mare_shared::project::resource_root(&env.cwd);
    let project = lua.create_table()?;
    project.set("root", rr.root.as_ref().map(|p| p.to_string_lossy().into_owned()))?;
    project.set("start", rr.start.to_string_lossy().into_owned())?;
    project.set("ignored", rr.ignored.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())?;
    project.set("rejected", rr.rejected.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())?;
    mare.set("project", project)?;
    // mare.env: callable (`mare.env("HOME")`) plus helpers (`mare.env.hostname()`, ...)
    let envt = lua.create_table()?;
    envt.set("get", lua.create_function(|_, k: String| Ok(std::env::var(k).ok()))?)?;
    envt.set(
        "hostname",
        lua.create_function(|_, ()| {
            os_hostname().map_err(|e| mlua::Error::external(format!("mare.env.hostname failed: {e}")))
        })?,
    )?;
    envt.set("os", std::env::consts::OS)?;
    envt.set("arch", std::env::consts::ARCH)?;
    envt.set("home", dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))?;
    let envmeta = lua.create_table()?;
    envmeta.set("__call", lua.create_function(|_, (_, k): (mlua::Value, String)| Ok(std::env::var(k).ok()))?)?;
    envt.set_metatable(Some(envmeta))?;
    mare.set("env", envt)?;
    mare.set(
        "log",
        lua.create_function(|_, (level, msg): (String, String)| {
            match level.as_str() {
                "error" => tracing::error!(target: "lua", "{msg}"),
                "warn" => tracing::warn!(target: "lua", "{msg}"),
                "debug" => tracing::debug!(target: "lua", "{msg}"),
                _ => tracing::info!(target: "lua", "{msg}"),
            }
            Ok(())
        })?,
    )?;
    // date(fmt?) -> local time string (strftime), e.g. "%Y-%m-%d"
    mare.set(
        "date",
        lua.create_function(|_, (fmt, ms): (Option<String>, Option<f64>)| {
            let ts = match ms {
                Some(ms) => jiff::Timestamp::from_millisecond(ms as i64).map_err(mare_shared::lua::rt_err)?,
                None => jiff::Timestamp::now(),
            };
            let z = ts.to_zoned(jiff::tz::TimeZone::system());
            Ok(z.strftime(fmt.as_deref().unwrap_or("%Y-%m-%d %H:%M:%S")).to_string())
        })?,
    )?;
    config_origin::install(lua, &mare)?;
    // small std additions used by plugins (documented in types/mare/std.lua)
    lua.load(
        "function table.copy(t) local o = {} for k, v in pairs(t) do o[k] = v end return o end\n\
         function table.find(t, x) for i, v in ipairs(t) do if v == x then return i end end return nil end",
    )
    .set_name("@core/table")
    .exec()?;
    // mare.caller_site(): "file:line" of the nearest caller outside the core prelude (for `/config`
    // sources, Neovim `:verbose set` style). The debug library isn't loaded, so ask mlua.
    // `mare.caller_site(skip)` skips that many more frames first: a plugin's `config_api` function
    // names its caller's line, not its own.
    mare.set(
        "caller_site",
        lua.create_function(|lua, skip: Option<usize>| {
            for level in 1 + skip.unwrap_or(0).min(16)..32 {
                let found = lua.inspect_stack(level, |d| {
                    let src = d.source();
                    let s = src.source.as_deref().unwrap_or("");
                    if s.starts_with('@') && !s.starts_with("@core/") && !s.starts_with("@preset/") {
                        Some(format!("{}:{}", &s[1..], d.current_line().unwrap_or(0)))
                    } else {
                        None
                    }
                });
                match found {
                    Some(Some(w)) => return Ok(Some(w)),
                    Some(None) => continue,
                    None => break,
                }
            }
            Ok(None)
        })?,
    )?;
    engines::config::install(lua, &mare, &config)?;
    lua.globals().set("mare", mare.clone())?;
    lua.globals().set("ui", mare.get::<Table>("ui")?)?;
    text_only_loaders(lua)?;
    if matches!(env.mode, Mode::Json | Mode::Rpc | Mode::Acp) {
        redirect_lua_stdout(lua)?;
    }
    freeze_stdlib(lua)?;
    Ok(mare)
}

pub use engines::config::load_config_api;

/// `load`, `loadfile` and `dofile` take source text only, whatever mode the caller names: a
/// binary chunk from a string or a file never loads from Lua code. Binary chunks load only on
/// host paths that hold bytecode this process compiled itself (built-in plugins, the module
/// cache's compiled sources, functions carried to a sub-agent: `mare_shared::carry`). The raw
/// loaders live in Rust closures, out of reach of `debug.getupvalue`.
fn text_only_loaders(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();
    // the mode is argument 3 of `load`, 2 of `loadfile`; arguments after it (env) pass unchanged
    for (name, mode_at) in [("load", 3usize), ("loadfile", 2)] {
        let raw: mlua::Function = g.get(name)?;
        g.set(
            name,
            lua.create_function(move |lua, args: mlua::MultiValue| {
                let mut a = args.into_vec();
                if name == "loadfile"
                    && let Some(arg) = a.first().filter(|v| !matches!(v, mlua::Value::Nil))
                    && let Some(path) = lua.coerce_string(arg.clone())?
                {
                    let path_str = path.to_str()?;
                    mare_binding::exec_ctx::check_process_path(lua, "read", &path_str)?;
                }
                if a.len() < mode_at {
                    a.resize(mode_at, mlua::Value::Nil);
                }
                a[mode_at - 1] = mlua::Value::String(lua.create_string("t")?);
                raw.call::<mlua::MultiValue>(mlua::MultiValue::from_vec(a))
            })?,
        )?;
    }
    // in Lua so a yielding file still yields; it reaches `loadfile` through the globals
    lua.load(
        "dofile = function(f) local chunk, err = loadfile(f) if not chunk then error(err, 2) end return chunk() end",
    )
    .set_name("@core/dofile")
    .exec()
}

/// A single failed case in a `mare.test` spec file.
#[derive(Debug, Clone)]
pub struct SpecFailure {
    pub name: String,
    pub error: String,
}

/// Options controlling `mare.test` spec execution.
#[derive(Debug, Clone, Default)]
pub struct SpecOptions {
    /// Only run test cases whose full name contains this substring.
    pub filter: Option<String>,
    /// Update `.snap` files directly instead of failing or writing `.snap.new`.
    pub update_snapshots: bool,
    /// File path of the spec being executed (used to locate sibling `snapshots/`).
    pub spec_path: Option<std::path::PathBuf>,
}

/// Execution summary of one `*_spec.lua` file via [`run_spec`] or [`run_spec_with_options`].
#[derive(Debug, Clone)]
pub struct SpecReport {
    pub total: usize,
    pub passed: usize,
    pub filtered: usize,
    pub load_error: bool,
    pub failures: Vec<SpecFailure>,
}

fn resolve_snapshot_update_mode(override_update: bool) -> &'static str {
    if override_update {
        return "always";
    }
    if let Ok(val) = std::env::var("MARE_UPDATE_SNAPSHOTS") {
        let lower = val.trim().to_ascii_lowercase();
        match lower.as_str() {
            "always" => return "always",
            "unseen" => return "unseen",
            "new" => return "new",
            "no" => return "no",
            _ => {}
        }
    }
    if std::env::var_os("CI").is_some_and(|v| !v.is_empty() && v != "0" && v != "false") { "no" } else { "new" }
}

fn build_snapshot_io(lua: &Lua) -> mlua::Result<Table> {
    let io_tbl = lua.create_table()?;
    io_tbl.set("read_file", lua.create_function(|_, path: String| Ok(std::fs::read_to_string(&path).ok()))?)?;
    io_tbl.set(
        "write_file",
        lua.create_function(|_, (path, content): (String, String)| {
            let p = std::path::Path::new(&path);
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return Ok((false, Some(e.to_string())));
            }
            match std::fs::write(p, content) {
                Ok(()) => Ok((true, None::<String>)),
                Err(e) => Ok((false, Some(e.to_string()))),
            }
        })?,
    )?;
    io_tbl.set("remove_file", lua.create_function(|_, path: String| Ok(std::fs::remove_file(&path).is_ok()))?)?;
    io_tbl.set(
        "update_mode",
        lua.create_function(|_, override_update: bool| Ok(resolve_snapshot_update_mode(override_update)))?,
    )?;
    Ok(io_tbl)
}

/// Execute a single `mare.test` spec chunk in an isolated environment inside `lua`.
///
/// `name` is used as the Lua chunk name (`@{name}`), so syntax and runtime errors include the
/// spec file path and line number.
pub async fn run_spec(lua: &Lua, name: &str, source: &str) -> mlua::Result<SpecReport> {
    let opts = SpecOptions { spec_path: Some(std::path::PathBuf::from(name)), ..Default::default() };
    run_spec_with_options(lua, name, source, &opts).await
}

/// Execute a single `mare.test` spec chunk with explicit [`SpecOptions`].
pub async fn run_spec_with_options(
    lua: &Lua,
    name: &str,
    source: &str,
    opts: &SpecOptions,
) -> mlua::Result<SpecReport> {
    let env = isolated_env(lua)?;
    let chunk = match lua.load(source).set_name(format!("@{name}")).set_environment(env).into_function() {
        Ok(f) => f,
        Err(err) => {
            return Ok(SpecReport {
                total: 0,
                passed: 0,
                filtered: 0,
                load_error: true,
                failures: vec![SpecFailure { name: "<syntax>".into(), error: err.to_string() }],
            });
        }
    };
    let runner: mlua::Function = lua.named_registry_value("mare_test_runner")?;
    let lua_opts = lua.create_table()?;
    if let Some(filter) = &opts.filter {
        lua_opts.set("filter", filter.as_str())?;
    }
    lua_opts.set("update_snapshots", opts.update_snapshots)?;
    if let Some(spec_path) = &opts.spec_path {
        lua_opts.set("spec_path", spec_path.to_string_lossy().into_owned())?;
    }
    let result: Table = runner.call_async((chunk, lua_opts)).await?;
    let total: usize = result.get("total")?;
    let passed: usize = result.get("passed")?;
    let filtered: usize = result.get("filtered").unwrap_or(0);
    let load_error: bool = result.get("load_error").unwrap_or(false);
    let failures_tbl: Table = result.get("failures")?;
    let mut failures = Vec::new();
    for entry in failures_tbl.sequence_values::<Table>() {
        let entry = entry?;
        failures.push(SpecFailure { name: entry.get("name")?, error: entry.get("error")? });
    }
    Ok(SpecReport { total, passed, filtered, load_error, failures })
}

#[cfg(test)]
mod tool_constraints_tests;

#[cfg(unix)]
fn hostname_from_raw(mut fill: impl FnMut(&mut [u8]) -> std::io::Result<()>) -> std::io::Result<String> {
    let mut size = 256usize;
    loop {
        let mut buf = vec![0u8; size];
        match fill(&mut buf) {
            Ok(()) => {
                if let Some(nul) = buf.iter().position(|&b| b == 0) {
                    let raw = &buf[..nul];
                    if raw.is_empty() {
                        return Err(std::io::Error::other("system hostname is empty"));
                    }
                    let s = std::str::from_utf8(raw).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("hostname is not valid UTF-8: {e}"),
                        )
                    })?;
                    return Ok(s.to_string());
                }
                if size >= 4096 {
                    return Err(std::io::Error::other("system hostname exceeds 4096 bytes"));
                }
                size *= 2;
            }
            Err(e) if e.raw_os_error() == Some(libc::ENAMETOOLONG) && size < 4096 => {
                size *= 2;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn os_hostname() -> std::io::Result<String> {
    hostname_from_raw(|buf| {
        // SAFETY: `buf` is valid for `buf.len()` writable bytes.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
        if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    })
}

#[cfg(windows)]
fn os_hostname() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, GetLastError};
    use windows_sys::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};
    let mut size = 0u32;
    // SAFETY: Querying required UTF-16 length with a null pointer is documented by Win32.
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, std::ptr::null_mut(), &mut size) };
    if ok == 0 {
        // SAFETY: `GetLastError` reads the calling thread's Win32 error code.
        let err = unsafe { GetLastError() };
        if err != ERROR_MORE_DATA || size == 0 {
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }
    }
    let mut buf = vec![0u16; size as usize];
    // SAFETY: `buf` has space for `size` UTF-16 code units.
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut size) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = (size as usize).min(buf.len());
    let s = String::from_utf16(&buf[..len]).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("hostname is not valid UTF-16: {e}"))
    })?;
    if s.is_empty() {
        return Err(std::io::Error::other("system hostname is empty"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    /// A slot's `enabled`: an explicit value wins either way; otherwise
    /// `plugins.preset = "minimal"` turns off a built-in tagged neither `minimal` nor `provider`
    /// (by tag, never by name) and never a slot whose effective definition is not a built-in.
    #[test]
    fn a_slot_is_on_unless_set_off_or_left_out_by_the_minimal_preset() {
        use crate::loader::slot_enabled;
        let minimal = serde_json::json!({ "plugins": { "preset": "minimal" } });
        let (provider, core) = (["provider".to_string()], ["minimal".to_string()]);
        assert!(slot_enabled("bedrock", true, &provider, &minimal), "tagged provider, any name");
        assert!(slot_enabled("llm", true, &core, &minimal), "tagged minimal");
        assert!(!slot_enabled("provider-x", true, &[], &minimal), "the name alone doesn't count");
        assert!(slot_enabled("review", false, &[], &minimal), "a replacement is not a built-in");
        assert!(slot_enabled("review", true, &[], &serde_json::json!({})), "no preset: every slot");
        let unknown = serde_json::json!({ "plugins": { "preset": "tiny" } });
        assert!(slot_enabled("review", true, &[], &unknown), "an unknown preset applies nothing");
        let on = serde_json::json!({ "plugins": { "preset": "minimal", "review": { "enabled": true } } });
        assert!(slot_enabled("review", true, &[], &on), "explicit true beats the preset");
        let off = serde_json::json!({ "plugins": { "preset": "minimal", "bedrock": { "enabled": false } } });
        assert!(!slot_enabled("bedrock", true, &provider, &off), "explicit false beats the tag");
    }

    /// `types_texts()` ships exactly the files in `preset/types/` (a new namespace file must be
    /// listed, a removed one dropped), and each listed text is that file.
    #[test]
    fn types_list_matches_the_directory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/types");
        let mut on_disk = vec![];
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "lua") {
                    on_disk.push(p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let listed = super::types_texts();
        let mut names: Vec<String> = listed.iter().map(|(n, _)| n.to_string()).collect();
        names.sort();
        assert_eq!(names, on_disk);
        for (rel, text) in &listed {
            assert_eq!(text.as_ref(), std::fs::read_to_string(root.join(rel)).unwrap(), "{rel}");
        }
        // Verify runtime extraction sources (deflate-archived in release builds) also match disk.
        let preset_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset");
        assert_eq!(
            super::loader::builtin_text("agent").unwrap().as_ref(),
            std::fs::read_to_string(preset_root.join("plugins/agent.lua")).unwrap()
        );
        let mod_texts = super::loader::builtin_module_texts();
        assert!(!mod_texts.is_empty());
        for (rel, text) in mod_texts.iter().take(5) {
            assert_eq!(
                text.as_ref(),
                std::fs::read_to_string(preset_root.join("plugins/lua").join(rel)).unwrap(),
                "{rel}"
            );
        }
        assert_eq!(super::news_text().as_ref(), std::fs::read_to_string(preset_root.join("NEWS.md")).unwrap());
    }

    #[test]
    fn bundled_skills_match_preset_directory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/skills");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    on_disk.push(p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let listed = super::skill_texts();
        let names: Vec<String> = listed.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, on_disk);
        assert!(names.iter().any(|n| n == "plugin-authoring/SKILL.md"), "missing bundled plugin-authoring skill");
        for (rel, text) in &listed {
            assert_eq!(text.as_ref(), std::fs::read_to_string(root.join(rel)).unwrap(), "{rel}");
        }
    }

    #[test]
    fn new_lua_runs_the_generational_gc_with_its_minor_multiplier() {
        let lua = super::new_lua().unwrap();
        // `param` returns the previous value: read it, then put it back
        let (minor, mode): (i64, String) = lua
            .load("local v = collectgarbage('param', 'minormul', 0) collectgarbage('param', 'minormul', v) return v, collectgarbage('generational')")
            .eval()
            .unwrap();
        assert_eq!(minor, i64::from(super::GC_MINOR_MULTIPLIER));
        assert_eq!(mode, "generational", "the VM runs the generational collector");
    }

    #[test]
    fn new_lua_sets_default_memory_limit_and_enforces_configured_limit() {
        let lua = super::new_lua().unwrap();
        let prev = super::set_memory_limit_mb(&lua, 1).unwrap();
        assert_eq!(prev, super::DEFAULT_LUA_MEMORY_MB * 1024 * 1024);

        let err = lua
            .load(
                r#"
                local t = {}
                for i = 1, 200000 do
                    t[i] = string.rep("x", 64)
                end
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(
            matches!(err, mlua::Error::MemoryError(_)),
            "expected MemoryError when exceeding lua_memory_mb, got: {err:?}"
        );
    }

    #[test]
    fn generated_config_artifacts_match_registry() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        let cfg = lua.create_table().unwrap();
        super::load_config_api(&lua, &mare, &cfg).unwrap();

        let options: mlua::Table = mare.get("options").unwrap();
        let mut rows: Vec<(String, String, String)> = vec![];
        for pair in options.pairs::<String, mlua::Table>() {
            let (name, o) = pair.unwrap();
            let luals = match o.get::<mlua::Value>("luals_type").unwrap() {
                mlua::Value::String(s) => s.to_str().unwrap().to_string(),
                _ => match o.get::<mlua::Value>("type").unwrap() {
                    mlua::Value::String(s) => s.to_str().unwrap().to_string(),
                    mlua::Value::Table(t) => {
                        t.sequence_values::<String>().map(|r| r.unwrap()).collect::<Vec<_>>().join("|")
                    }
                    other => panic!("unexpected option type for {name}: {other:?}"),
                },
            };
            let raw_desc = o.get::<Option<String>>("desc").unwrap().unwrap_or_default();
            let desc = raw_desc.split(['\t', '\r', '\n']).collect::<Vec<_>>().join(" ");
            let desc = desc.trim().to_string();
            rows.push((name, luals, desc));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        // 1. CORE_OPTIONS matches the registry.
        let expected_core: Vec<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(mare_config::CORE_OPTIONS, expected_core.as_slice(), "CORE_OPTIONS drifted from config_api.lua");

        // 2. Checked-in SetupOpts in preset/types/mare/config.lua matches registry
        use std::fmt::Write as _;
        let mut expected_block =
            String::from("-- BEGIN GENERATED SetupOpts (scripts/gen_options.py)\n---@class mare.SetupOpts\n");
        for (name, luals, desc) in &rows {
            if desc.is_empty() {
                let _ = writeln!(expected_block, "---@field {name}? {luals}");
            } else {
                let _ = writeln!(expected_block, "---@field {name}? {luals} {desc}");
            }
        }
        expected_block.push_str("-- END GENERATED SetupOpts");

        let config_lua_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/types/mare/config.lua");
        let config_lua = std::fs::read_to_string(&config_lua_path).unwrap();
        let begin = "-- BEGIN GENERATED SetupOpts (scripts/gen_options.py)";
        let end = "-- END GENERATED SetupOpts";
        let start_idx = config_lua.find(begin).expect("SetupOpts begin marker");
        let end_idx = config_lua.find(end).expect("SetupOpts end marker") + end.len();
        assert_eq!(
            &config_lua[start_idx..end_idx],
            expected_block,
            "SetupOpts in preset/types/mare/config.lua drifted from config_api.lua; run `python3 scripts/gen_options.py`"
        );
    }

    #[test]
    fn profile_plugin_renders_callback_timing_table_sorted_and_capped() {
        let lua = super::new_lua().unwrap();
        for (plugin, event, micros) in [
            ("p_slow_one", "ev/b", 80_000),
            ("p_discarded", "ev/c", 20_000),
            ("p_fast_many", "ev/a", 30_000),
            ("p_fast_many", "ev/a", 30_000),
            ("p_fast_many", "ev/a", 30_000),
            ("p_fast_many", "ev/a", 30_000),
            ("p_fast_many", "ev/a", 500),
        ] {
            mare_shared::profile::record_callback_timing(std::time::Duration::from_micros(micros), plugin, event);
        }
        let timings = lua
            .create_function(|lua, limit: usize| {
                let result = lua.create_table()?;
                for (i, row) in mare_shared::profile::callback_timings(limit).into_iter().enumerate() {
                    let value = lua.create_table()?;
                    value.set("plugin", row.plugin)?;
                    value.set("event", row.event)?;
                    value.set("count", row.count)?;
                    value.set("total_ms", row.total_ms)?;
                    value.set("max_ms", row.max_ms)?;
                    result.raw_set(i + 1, value)?;
                }
                Ok(result)
            })
            .unwrap();
        lua.globals().set("__real_callback_timings", timings).unwrap();
        let profile_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/profile.lua"),
        )
        .unwrap();
        let rendered: String = lua
            .load(format!(
                r#"
                mare = {{
                    o = {{ slow_callback_ms = 5, slow_callback_record_ms = 500, slow_callback_table_rows = 2 }},
                    task = {{ now = function() return 0 end }},
                    date = function(fmt) return fmt end,
                    -- the native plugin reads the profile through mare.profile (3c2d3be7)
                    profile = {{
                        report = function()
                            return {{ {{ at = 1.0, took = 2.0, label = "plugin profile" }} }}
                        end,
                        slow_callbacks = function() return {{}} end,
                        callback_timings = __real_callback_timings,
                    }},
                    crypto = {{ sha256 = function(s) return s end }},
                    json = {{ encode = function() return "{{}}" end }},
                }}
                local overlay_title, overlay_lines
                local function make_obj(text)
                    local o = {{ text = text or "" }}
                    function o:hl() return self end
                    return o
                end
                ui = {{
                    Span = function(s) return make_obj(s) end,
                    Line = function(spans)
                        local parts = {{}}
                        for _, sp in ipairs(spans) do parts[#parts + 1] = sp.text end
                        return table.concat(parts)
                    end,
                    Gap = function() return "" end,
                    Lines = function(lines) return lines end,
                    overlay = function(title, lines)
                        overlay_title = title
                        overlay_lines = lines
                    end,
                }}
                local registered_run
                -- the native plugin's ctx: bus injected, commands followed (`ctx:inject({{ "commands" }}, fn)`)
                local ctx = {{
                    bus = {{ emit = function() end }},
                    on = function() end,
                    inject = function(_, names, fn)
                        assert(names[1] == "commands" and #names == 1, "only commands is followed")
                        fn({{
                            commands = {{
                                register = function(_, name, def)
                                    if name == "profile" then registered_run = def.run end
                                end,
                            }},
                        }})
                    end,
                }}
                local mod = (function() {profile_src} end)()
                mod.apply(ctx)
                registered_run()
                assert(overlay_title == "Profile")
                return table.concat(overlay_lines, "\n")
                "#
            ))
            .eval()
            .unwrap();

        assert!(rendered.contains("callback timing (> 2ms): 2"), "{rendered}");
        assert!(rendered.contains("count") && rendered.contains("total") && rendered.contains("max"), "{rendered}");
        assert!(rendered.contains("5    120.5ms     30.0ms  [p_fast_many] ev/a"), "{rendered}");
        assert!(rendered.contains("1     80.0ms     80.0ms  [p_slow_one] ev/b"), "{rendered}");
        let pos_a = rendered.find("[p_fast_many] ev/a").unwrap();
        let pos_b = rendered.find("[p_slow_one] ev/b").unwrap();
        assert!(pos_a < pos_b, "expected higher total_ms row first:\n{rendered}");
        assert!(!rendered.contains("p_discarded"), "row limit must be applied to real timings:\n{rendered}");
    }

    #[test]
    fn lua_stdout_is_redirected_in_protocol_modes() {
        use crate::{Env, Mode, install};
        use mlua::Lua;
        for mode in [Mode::Json, Mode::Rpc, Mode::Acp] {
            let lua = Lua::new();
            let env = Env {
                cwd: std::env::current_dir().unwrap(),
                mode,
                ui: mare_shared::ui::UiShared::new(),
                config: serde_json::json!({}),
                args: serde_json::json!({}),
            };
            install(&lua, &env).unwrap();
            let same: bool = lua.load("return io.stdout == io.stderr and io.output() == io.stderr").eval().unwrap();
            assert!(same, "io.stdout and io.output() must redirect to io.stderr in {mode:?}");
        }
    }

    #[tokio::test]
    async fn mare_env_hostname_uses_syscall_without_path() {
        use crate::{Env, Mode, install};
        use mlua::Lua;
        let prev_path = std::env::var_os("PATH");
        // SAFETY: nextest runs each unit test in its own process; restore PATH before returning.
        unsafe {
            std::env::set_var("PATH", "");
        }
        let res: mlua::Result<String> = async {
            let lua = Lua::new();
            let env = Env {
                cwd: std::env::current_dir().unwrap(),
                mode: Mode::Print,
                ui: mare_shared::ui::UiShared::new(),
                config: serde_json::json!({}),
                args: serde_json::json!({}),
            };
            install(&lua, &env)?;
            lua.load("return mare.env.hostname()").eval_async::<String>().await
        }
        .await;
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        let host = res.expect("mare.env.hostname() must succeed via OS syscall even with empty PATH");
        assert!(!host.trim().is_empty(), "mare.env.hostname() returned empty string with empty PATH");
    }

    #[cfg(unix)]
    #[test]
    fn hostname_from_raw_propagates_errors_and_grows_buffer() {
        use super::hostname_from_raw;

        let err = hostname_from_raw(|_| Err(std::io::Error::from_raw_os_error(libc::EFAULT))).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EFAULT));

        let empty = hostname_from_raw(|buf| {
            buf[0] = 0;
            Ok(())
        })
        .unwrap_err();
        assert!(empty.to_string().contains("empty"), "{empty}");

        let long = "a".repeat(300);
        let got = hostname_from_raw(|buf| {
            if buf.len() <= long.len() {
                return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
            }
            buf[..long.len()].copy_from_slice(long.as_bytes());
            buf[long.len()] = 0;
            Ok(())
        })
        .unwrap();
        assert_eq!(got, long);
    }

    #[test]
    fn service_contracts_chunk_matches_source_and_is_bytecode_in_release() {
        // Keep `contract_files::CONTRACT_FILES` in sync with `preset/types/mare/services/contracts/`.
        let contracts_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/types/mare/services/contracts");
        let mut on_disk: Vec<String> = std::fs::read_dir(&contracts_dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (name.ends_with(".lua") && name != "schema.lua").then_some(name)
            })
            .collect();
        on_disk.sort();
        let mut listed: Vec<String> = crate::contract_files::CONTRACT_FILES.iter().map(|s| (*s).to_string()).collect();
        listed.sort();
        assert_eq!(listed, on_disk, "CONTRACT_FILES must match preset/types/mare/services/contracts/");

        let chunk = super::service_contracts_chunk();
        #[cfg(not(debug_assertions))]
        assert!(
            chunk.starts_with(b"\x1bLua"),
            "release builds must embed precompiled Lua bytecode for @builtin/service-contracts"
        );

        let lua = mlua::Lua::new();
        let from_chunk: mlua::Value = lua.load(chunk.as_ref()).set_name("@builtin/service-contracts").eval().unwrap();
        let from_src: mlua::Value =
            lua.load(super::service_contracts_lua().as_ref()).set_name("@builtin/service-contracts").eval().unwrap();

        let parsed_chunk = mare_kernel::contracts::parse_contracts(&lua, from_chunk).unwrap();
        let parsed_src = mare_kernel::contracts::parse_contracts(&lua, from_src).unwrap();
        let parsed_builtin = super::builtin_service_contracts().unwrap();
        let parsed_builtin_plain: std::collections::BTreeMap<String, mare_kernel::ServiceContract> =
            parsed_builtin.iter().map(|(k, v)| (k.clone(), (**v).clone())).collect();
        assert_eq!(
            parsed_builtin_plain, parsed_src,
            "build-time pre-evaluated service contracts must match live Lua evaluation exactly"
        );
        let chunk_names: Vec<&str> = parsed_chunk.keys().map(String::as_str).collect();
        let src_names: Vec<&str> = parsed_src.keys().map(String::as_str).collect();
        assert_eq!(chunk_names, src_names);
        for (name, c_spec) in &parsed_chunk {
            let s_spec = &parsed_src[name];
            assert_eq!(c_spec.methods.len(), s_spec.methods.len(), "{name} methods len");
            assert_eq!(c_spec.events.len(), s_spec.events.len(), "{name} events len");
            for m in c_spec.methods.keys() {
                assert!(s_spec.methods.contains_key(m), "{name}.{m} missing in source");
            }
            for e in c_spec.events.keys() {
                assert!(s_spec.events.contains_key(e), "{name} event {e} missing in source");
            }
        }
    }

    #[test]
    fn news_check_does_not_call_mare_news_when_version_already_seen() {
        let lua = mlua::Lua::new();
        let news_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/lua/plugins/news.lua"),
        )
        .unwrap();
        let (seen_calls, upgrade_calls, upgrade_shown): (usize, usize, usize) = lua
            .load(format!(
                r###"
                local function run_case(seen_version)
                    local calls, shown = 0, 0
                    mare = {{
                        version = "1.0.3",
                        paths = {{ state = "/tmp" }},
                        lock = {{
                            file = function() return {{ release = function() end }} end,
                        }},
                        fs = {{
                            read = function() return seen_version end,
                            write = function() return true end,
                        }},
                        json = {{
                            is_array = function() return false end,
                            decode_lossless = function(raw)
                                return {{ version = 1, seen = {{ mare = raw }} }}
                            end,
                            encode = function() return "{{}}" end,
                        }},
                        plugins = {{
                            news_version = 1,
                            mare_news = function()
                                calls = calls + 1
                                return "## 1.0.3\n\n- News item\n"
                            end,
                            catalog = function() return {{}} end,
                        }},
                        ui = {{
                            print = function() shown = shown + 1 end,
                        }},
                        log = function() end,
                    }}
                    ui = {{
                        Span = function(s) return {{ text = s, hl = function(self) return self end }} end,
                        Line = function(xs) return xs end,
                        Lines = function(xs) return xs end,
                    }}
                    local news = (function() {news_src} end)()
                    news.news()
                    return calls, shown
                end
                local c1 = run_case("1.0.3")
                local c2, s2 = run_case("1.0.2")
                return c1, c2, s2
                "###
            ))
            .eval()
            .unwrap();
        assert_eq!(
            seen_calls, 0,
            "news.news() must not eagerly call mare.plugins.mare_news() when mare.version is already seen"
        );
        assert_eq!(upgrade_calls, 1, "news.news() must still read mare_news() when mare.version upgraded");
        assert_eq!(upgrade_shown, 1, "news.news() must display the changelog section on version upgrade");
    }

    #[test]
    fn news_text_is_isolated_from_full_runtime_source_archive() {
        let on_disk =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/NEWS.md")).unwrap();
        assert_eq!(super::news_text().as_ref(), on_disk);
        #[cfg(not(debug_assertions))]
        assert!(
            crate::runtime_archive::get("NEWS.md").is_none(),
            "NEWS.md must be stored in its own standalone archive, not inside the ~3.85 MB runtime_sources.deflate"
        );
    }

    #[test]
    fn plugins_watch_setup_does_not_block_apply_with_sync_fs_reads() {
        let lua = mlua::Lua::new();
        let watch_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/lua/plugins/watch.lua"),
        )
        .unwrap();
        let (sync_stats, sync_reads, notified): (usize, usize, usize) = lua
            .load(format!(
                r#"
                STAT_CALLS, READ_CALLS, NOTIFIES = 0, 0, 0
                local mtime, body = 100, "initial"
                mare = {{
                    mode = "tui",
                    fs = {{
                        stat = function()
                            STAT_CALLS = STAT_CALLS + 1
                            return {{ mtime = mtime, size = #body }}
                        end,
                        read = function()
                            READ_CALLS = READ_CALLS + 1
                            return body
                        end,
                    }},
                    plugins = {{
                        watched = function() return {{ "/tmp/init.lua" }} end,
                    }},
                }}
                ui = {{
                    notify = function() NOTIFIES = NOTIFIES + 1 end,
                }}
                local spawned, tick_fn = {{}}, nil
                local ctx = {{
                    spawn = function(_, fn) spawned[#spawned + 1] = fn end,
                    every = function(_, _, fn) tick_fn = fn end,
                }}
                local watch = (function() {watch_src} end)()
                watch.setup(ctx, {{ watch_ms = 2000 }})
                local sync_s, sync_r = STAT_CALLS, READ_CALLS
                for _, fn in ipairs(spawned) do fn() end
                mtime, body = 200, "edited"
                tick_fn()
                return sync_s, sync_r, NOTIFIES
                "#
            ))
            .eval()
            .unwrap();
        assert_eq!(sync_stats, 1, "watch.setup() captures lightweight baseline stat stamps synchronously");
        assert_eq!(sync_reads, 0, "watch.setup() must not synchronously read watched files during apply");
        assert_eq!(notified, 1, "watch timer must still notify after background seed and file edit");
    }

    #[test]
    fn review_and_plugins_apply_defer_cold_ui_submodules() {
        let lua = mlua::Lua::new();
        let preset_plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins");
        let review_src = std::fs::read_to_string(preset_plugins.join("review.lua")).unwrap();
        let plugins_src = std::fs::read_to_string(preset_plugins.join("plugins.lua")).unwrap();
        let eager: Vec<String> = lua
            .load(format!(
                r#"
                local loaded = {{}}
                local real_require = require
                function require(name)
                    loaded[#loaded + 1] = name
                    if name == "mare_lib" then
                        return {{
                            follow = function() return function() return nil end end,
                            service = function() end,
                        }}
                    elseif name == "plugins.watch" then
                        return {{ setup = function() end }}
                    elseif name == "plugins.extras_list" then
                        return {{ runner = function() return function() end end }}
                    elseif name == "plugins.panel" or name == "plugins.choose" or name == "review.viewer" or name == "review.runner" then
                        return {{ create = function() return function() end end }}
                    elseif name == "plugins.news" then
                        return {{ news = function() return {{}} end }}
                    end
                    return real_require(name)
                end
                local ctx = {{
                    ui = {{ action = function() end }},
                    on = function() end,
                    inject = function() end,
                    spawn = function() end,
                }}
                local rev = (function() {review_src} end)()
                rev.apply(ctx, {{}})
                local pkg = (function() {plugins_src} end)()
                pkg.apply(ctx, {{ watch_ms = 0 }})
                return loaded
                "#
            ))
            .eval()
            .unwrap();
        for cold in
            ["review.viewer", "review.runner", "plugins.extras_list", "plugins.panel", "plugins.choose", "plugins.news"]
        {
            assert!(
                !eager.iter().any(|m| m == cold),
                "cold UI submodule `{cold}` should be lazy-required on demand, not during apply(); loaded={eager:?}"
            );
        }
    }

    #[test]
    fn plugins_watch_does_not_miss_edits_during_async_seed_or_startup_window() {
        let lua = mlua::Lua::new();
        let watch_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/lua/plugins/watch.lua"),
        )
        .unwrap();
        let (race_notifies, window_notifies, retry_notifies): (usize, usize, usize) = lua
            .load(format!(
                r#"
                local watch_factory = load([=[{watch_src}]=], "@watch.lua")

                -- Case A: ctx:every fires while seed() is yielded inside async mare.fs.read
                local function test_async_race()
                    local files = {{
                        ["/tmp/a.lua"] = {{ mtime = 100, body = "a0" }},
                        ["/tmp/b.lua"] = {{ mtime = 100, body = "b0" }},
                    }}
                    local notifies = 0
                    local yield_once = true
                    mare = {{
                        mode = "tui",
                        fs = {{
                            stat = function(p)
                                local f = files[p]
                                return f and {{ mtime = f.mtime, size = #f.body }} or nil
                            end,
                            read = function(p)
                                if p == "/tmp/a.lua" and yield_once then
                                    yield_once = false
                                    coroutine.yield("reading_a")
                                end
                                return files[p] and files[p].body or nil
                            end,
                        }},
                        plugins = {{
                            watched = function() return {{ "/tmp/a.lua", "/tmp/b.lua" }} end,
                        }},
                    }}
                    ui = {{ notify = function() notifies = notifies + 1 end }}
                    local spawned, tick_fn = {{}}, nil
                    local ctx = {{
                        spawn = function(_, fn) spawned[#spawned + 1] = fn end,
                        every = function(_, _, fn) tick_fn = fn end,
                    }}
                    local watch = watch_factory()
                    watch.setup(ctx, {{ watch_ms = 50 }})
                    local co = coroutine.create(spawned[1])
                    local ok1, tag = coroutine.resume(co)
                    assert(ok1 and tag == "reading_a", "seed should yield during async read")
                    -- Edit b.lua and fire ctx:every while seed() is mid-flight
                    files["/tmp/b.lua"] = {{ mtime = 200, body = "b1_edited" }}
                    tick_fn()
                    -- Resume seed() to completion and fire another tick
                    local ok2, err2 = coroutine.resume(co)
                    assert(ok2, tostring(err2))
                    tick_fn()
                    return notifies
                end

                -- Case B: file edited after watch.setup() before seed() runs
                local function test_startup_to_seed_window()
                    local files = {{ ["/tmp/init.lua"] = {{ mtime = 100, body = "v0" }} }}
                    local notifies = 0
                    mare = {{
                        mode = "tui",
                        fs = {{
                            stat = function(p)
                                local f = files[p]
                                return f and {{ mtime = f.mtime, size = #f.body }} or nil
                            end,
                            read = function(p) return files[p] and files[p].body or nil end,
                        }},
                        plugins = {{ watched = function() return {{ "/tmp/init.lua" }} end }},
                    }}
                    ui = {{ notify = function() notifies = notifies + 1 end }}
                    local spawned, tick_fn = {{}}, nil
                    local ctx = {{
                        spawn = function(_, fn) spawned[#spawned + 1] = fn end,
                        every = function(_, _, fn) tick_fn = fn end,
                    }}
                    local watch = watch_factory()
                    watch.setup(ctx, {{ watch_ms = 2000 }})
                    files["/tmp/init.lua"] = {{ mtime = 200, body = "v1_edited_before_seed" }}
                    spawned[1]()
                    tick_fn()
                    return notifies
                end

                -- Case C: seed() fails on first attempt, must retry on next tick and still detect edits
                local function test_seed_error_retry()
                    local files = {{ ["/tmp/init.lua"] = {{ mtime = 100, body = "v0" }} }}
                    local notifies, watched_calls = 0, 0
                    mare = {{
                        mode = "tui",
                        fs = {{
                            stat = function(p)
                                local f = files[p]
                                return f and {{ mtime = f.mtime, size = #f.body }} or nil
                            end,
                            read = function(p) return files[p] and files[p].body or nil end,
                        }},
                        plugins = {{
                            watched = function()
                                watched_calls = watched_calls + 1
                                if watched_calls == 2 then
                                    error("transient watched failure")
                                end
                                return {{ "/tmp/init.lua" }}
                            end,
                        }},
                    }}
                    ui = {{ notify = function() notifies = notifies + 1 end }}
                    local spawned, tick_fn = {{}}, nil
                    local ctx = {{
                        spawn = function(_, fn) spawned[#spawned + 1] = fn end,
                        every = function(_, _, fn) tick_fn = fn end,
                    }}
                    local watch = watch_factory()
                    watch.setup(ctx, {{ watch_ms = 2000 }})
                    pcall(spawned[1]) -- fails on watched_calls == 2
                    tick_fn() -- retries seed cleanly
                    files["/tmp/init.lua"] = {{ mtime = 200, body = "v1_after_retry" }}
                    tick_fn()
                    return notifies
                end

                return test_async_race(), test_startup_to_seed_window(), test_seed_error_retry()
                "#
            ))
            .eval()
            .unwrap();
        assert_eq!(race_notifies, 1, "edit during yielded seed() must not be swallowed by racing ctx:every");
        assert_eq!(window_notifies, 1, "edit between watch.setup() and seed() completion must be reported");
        assert_eq!(retry_notifies, 1, "failed seed() must not mark seeded=true and must retry cleanly");
    }

    #[test]
    fn review_get_runner_retries_cleanly_after_create_error() {
        let lua = mlua::Lua::new();
        let review_src =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/review.lua"))
                .unwrap();
        let (first_ok, second_res): (bool, String) = lua
            .load(format!(
                r#"
                local attempts = 0
                local real_require = require
                local registered_service
                function require(name)
                    if name == "mare_lib" then
                        return {{
                            follow = function() return function() return nil end end,
                            service = function(_, _, svc) registered_service = svc end,
                        }}
                    elseif name == "review.runner" then
                        return {{
                            create = function()
                                attempts = attempts + 1
                                if attempts == 1 then
                                    error("transient create failure")
                                end
                                return function(target) return "ok:" .. tostring(target.kind) end
                            end,
                        }}
                    end
                    return real_require(name)
                end
                local ctx = {{
                    ui = {{ action = function() end }},
                    on = function() end,
                    inject = function() end,
                    spawn = function() end,
                }}
                local rev = (function() {review_src} end)()
                rev.apply(ctx, {{}})
                local ok1 = pcall(registered_service.run, registered_service, ctx, {{ kind = "uncommitted" }})
                local res2 = registered_service.run(registered_service, ctx, {{ kind = "uncommitted" }})
                return ok1, res2
                "#
            ))
            .eval()
            .unwrap();
        assert!(!first_ok, "first create() failure must propagate");
        assert_eq!(second_res, "ok:uncommitted", "second call must retry runner.create() cleanly");
    }

    #[test]
    fn builtin_modules_are_strictly_sorted_for_binary_search() {
        #[cfg(not(debug_assertions))]
        {
            let mods = crate::loader::BUILTIN_MODULES;
            assert!(!mods.is_empty(), "release BUILTIN_MODULES must be non-empty");
            for pair in mods.windows(2) {
                assert!(
                    pair[0].0 < pair[1].0,
                    "BUILTIN_MODULES must be strictly sorted for binary_search: {:?} >= {:?}",
                    pair[0].0,
                    pair[1].0
                );
            }
        }
    }

    #[test]
    fn mare_lib_lazy_group_preserves_wrappers_and_recovers_from_failed_loads() {
        let lua = mlua::Lua::new();
        let mare_lib_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/lua/mare_lib.lua"),
        )
        .unwrap();
        let ok: bool = lua
            .load(format!(
                r###"
                local lib = load([===[{mare_lib_src}]===], "@mare_lib.lua")()

                -- 1. Deferred loading, fast_unloaded, wrapper preservation, and failure recovery.
                local target, extra = {{}}, {{}}
                local loads = 0
                local should_fail = true
                local wrapper_calls = 0

                local ensure, proxy = lib.lazy_group(target, {{ "run", "status" }}, function()
                    loads = loads + 1
                    -- Mutate target before failing to verify failed loads roll back saved wrappers.
                    target.run = function(_, x) return "partial:" .. tostring(x) end
                    extra.helper = function(v) return "partial_h:" .. tostring(v) end
                    if should_fail then
                        error("boom during load", 0)
                    end
                    target.run = function(_, x) return "loaded:" .. tostring(x) end
                    target.status = function() return "ready" end
                    extra.helper = function(v) return "helper:" .. tostring(v) end
                    return {{ api_fn = function(v) return "api:" .. tostring(v) end }}
                end, {{
                    fast_unloaded = {{
                        status = function() return "idle" end,
                    }},
                    extra_target = extra,
                    extra_fields = {{ "helper" }},
                }})

                assert(loads == 0, "lazy_group must not invoke loader at registration")
                -- Install an outer wrapper (like mare_lib.service or session's poison guard).
                local orig_run = target.run
                local orig_status = target.status
                target.run = function(...)
                    wrapper_calls = wrapper_calls + 1
                    return orig_run(...)
                end
                target.status = function(...)
                    wrapper_calls = wrapper_calls + 1
                    return orig_status(...)
                end
                local wrapped_run = target.run
                local wrapped_helper = extra.helper

                -- fast_unloaded executes without loading the submodule.
                assert(target.status() == "idle")
                assert(loads == 0, "fast_unloaded must not trigger loader")
                assert(wrapper_calls == 1)

                -- Failing load restores saved wrappers and re-raises without caching.
                local ok_fail, err_fail = pcall(target.run, target, "first")
                assert(not ok_fail and tostring(err_fail):find("boom during load", 1, true), tostring(err_fail))
                assert(loads == 1)
                assert(target.run == wrapped_run, "failed load must restore outer wrapper on target")
                assert(extra.helper == wrapped_helper, "failed load must restore stub on extra_target")
                assert(target.status() == "idle", "failed load must not mark group as loaded")
                assert(loads == 1)

                -- Subsequent retry succeeds, preserves outer wrappers, and disables fast_unloaded.
                should_fail = false
                assert(target.run(target, "second") == "loaded:second")
                assert(loads == 2)
                assert(target.run == wrapped_run, "successful load must preserve outer wrapper on target")
                assert(target.status() == "ready", "loaded status must bypass fast_unloaded")
                assert(extra.helper("ok") == "helper:ok")
                assert(proxy.api_fn("z") == "api:z")
                assert(ensure().api_fn("y") == "api:y")
                assert(loads == 2, "loader must run only once after succeeding")

                -- Wrapper continues to run on subsequent calls.
                local before_calls = wrapper_calls
                assert(target.run(target, "third") == "loaded:third")
                assert(wrapper_calls == before_calls + 1, "outer wrapper must still intercept calls")

                -- 2. Loading via extra_target preserves outer wrapper on target.
                local t2, e2 = {{}}, {{}}
                local w2_hits = 0
                lib.lazy_group(t2, {{ "op" }}, function()
                    t2.op = function() return "op_ok" end
                    e2.aux = function() return "aux_ok" end
                end, {{ extra_target = e2, extra_fields = {{ "aux" }} }})
                local orig_op = t2.op
                t2.op = function(...)
                    w2_hits = w2_hits + 1
                    return orig_op(...)
                end
                assert(e2.aux() == "aux_ok")
                assert(t2.op() == "op_ok")
                assert(w2_hits == 1, "wrapper on target must survive load triggered via extra_target")

                -- 3. Missing declared method rolls back saved wrapper and raises.
                local t3 = {{}}
                lib.lazy_group(t3, {{ "present", "missing" }}, function()
                    t3.present = function() return true end
                end)
                local saved_present = t3.present
                local ok_m, err_m = pcall(t3.present)
                assert(not ok_m and tostring(err_m):find("missing", 1, true), tostring(err_m))
                assert(t3.present == saved_present, "incomplete load must restore saved wrapper")

                return true
                "###
            ))
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn mare_lib_followers_uses_single_effect_and_clears_on_unload() {
        let lua = mlua::Lua::new();
        let preset_plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins");
        let mare_lib_src = std::fs::read_to_string(preset_plugins.join("lua/mare_lib.lua")).unwrap();
        let ok: bool = lua
            .load(format!(
                r###"
                local lib = assert(load({mare_lib_src:?}, "@mare_lib.lua"))()
                local effects = {{}}
                local services = {{ a = {{ id = "a1" }}, b = {{ id = "b1" }} }}
                local ctx = {{
                    effect = function(_, fn, label)
                        local cleanup = fn()
                        effects[#effects + 1] = {{ cleanup = cleanup, label = label }}
                    end,
                    get = function(_, name)
                        return services[name]
                    end,
                }}

                local svc = lib.followers(ctx, {{ "a", "b", "c" }})
                assert(#effects == 1, "followers must register exactly 1 shared effect, got " .. tostring(#effects))
                assert(svc.a == services.a and svc.b == services.b and svc.c == nil)
                assert(svc.unlisted == nil, "unlisted service must stay nil even if present")
                services.unlisted = {{ id = "x" }}
                assert(svc.unlisted == nil)
                services.c = {{ id = "c1" }}
                assert(svc.c == services.c, "dynamic service arrival must be visible immediately")

                local write_ok = pcall(function() svc.a = nil end)
                assert(not write_ok, "followers table must be read-only")

                -- Unload runs the single cleanup and clears every followed service.
                effects[1].cleanup()
                assert(svc.a == nil and svc.b == nil and svc.c == nil, "unload must return nil for all followed services")
                return true
                "###
            ))
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn session_apply_defers_cold_submodules_until_first_use() {
        let lua = mlua::Lua::new();
        let preset_plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins");
        let mare_lib_src = std::fs::read_to_string(preset_plugins.join("lua/mare_lib.lua")).unwrap();
        let session_src = std::fs::read_to_string(preset_plugins.join("session.lua")).unwrap();
        let commands_src = std::fs::read_to_string(preset_plugins.join("lua/session/commands.lua")).unwrap();
        let startup_src = std::fs::read_to_string(preset_plugins.join("lua/session/startup.lua")).unwrap();
        let (eager, all_present, prep_path, prep_loads, poisoned_blocked, helper_res, helper_poisoned): (
            Vec<String>,
            bool,
            String,
            usize,
            bool,
            String,
            bool,
        ) = lua
            .load(format!(
                r###"
                local real_mare_lib = load([===[{mare_lib_src}]===], "@mare_lib.lua")()
                local commands_mod = load([===[{commands_src}]===], "@session/commands.lua")()
                local startup_mod = load([===[{startup_src}]===], "@session/startup.lua")()
                local real_require = require

                local function run_instance()
                    local loaded = {{}}
                    local fork_loads = 0
                    local poisoned = nil
                    local registered_service = nil
                    local captured_D = nil
                    mare = {{
                        cwd = "/tmp",
                        mode = "tui",
                        args = {{}},
                        g = {{}},
                        store = {{
                            dir = function() return "/tmp/sessions" end,
                            index_warn_unsupported = function() return {{}} end,
                        }},
                        task = {{
                            session_error = function() return poisoned end,
                        }},
                    }}
                    function require(name)
                        loaded[#loaded + 1] = name
                        if name == "mare_lib" then
                            return {{
                                follow = function() return function() return nil end end,
                                service = function(_, _, svc) registered_service = svc end,
                                fail = function(code, msg) error(code .. ":" .. tostring(msg), 0) end,
                                lazy = real_mare_lib.lazy,
                                lazy_group = real_mare_lib.lazy_group,
                            }}
                        elseif name == "session.commands" then
                            return commands_mod
                        elseif name == "session.startup" then
                            return startup_mod
                        elseif name == "session.store" then
                            return function() return {{ add = function() end, ensure_file = function() end, retire_on_shutdown = function() end }} end
                        elseif name == "session.operations" then
                            return function() return {{}} end
                        elseif name == "session.history" then
                            return function() return {{ path_to = function() end, abandoned_messages = function() end, ensure_full = function() end, index_update = function() end }} end
                        elseif name == "session.lifecycle" then
                            return function(D)
                                captured_D = D
                                return {{ load_file = function() end, reset = function() end }}
                            end
                        elseif name == "session.transitions" or name == "session.snapshots" then
                            return function() return {{ touched_paths = function() return {{}} end }} end
                        elseif name == "session.queries" or name == "session.recorder" or name == "session.tool_state" then
                            return function() end
                        elseif name == "session.fork" then
                            fork_loads = fork_loads + 1
                            return function(D)
                                function D.session.set_leaf() end
                                function D.session.prepare_fork(_, _)
                                    return {{ path = "/tmp/sessions/s1.jsonl", images = {{ ["a.png"] = "raw" }} }}
                                end
                                function D.move_leaf() end
                                function D.validate_fork_source() end
                                function D.prepare_fork() end
                                function D.completed_fork_boundary() end
                                function D.completed_fork_source() end
                                function D.read_validated_fork_snapshot() end
                                function D.hydrate_verified_fork_images(snap, verified)
                                    return tostring(snap) .. ":" .. tostring(verified and verified["a.png"])
                                end
                                return {{
                                    fork_at = function() end,
                                    switch_fork = function() end,
                                    build_snapshot_state = function() end,
                                }}
                            end
                        elseif name == "session.children" or name == "session.compaction" or name == "session.fork_snapshot"
                            or name == "session.pickers" or name == "session.rewind" or name == "session.metadata" then
                            return function() return {{}} end
                        end
                        return real_require(name)
                    end
                    local inject_cbs = {{}}
                    local ctx = {{
                        on = function() end,
                        effect = function() end,
                        spawn = function() end,
                        inject = function(_, _, fn) inject_cbs[#inject_cbs + 1] = fn end,
                        policy = {{ voter = function() end }},
                        kernel = {{
                            reload_generation = function() return 0 end,
                        }},
                        commands = {{ register = function() end }},
                    }}
                    local sess = load([===[{session_src}]===], "@session.lua")()
                    sess.apply(ctx, {{}})
                    for _, fn in ipairs(inject_cbs) do fn(ctx) end
                    local eager_copy = {{}}
                    for i, m in ipairs(loaded) do eager_copy[i] = m end
                    return {{
                        eager = eager_copy,
                        svc = registered_service,
                        D = captured_D,
                        ctx = ctx,
                        set_poison = function(reason) poisoned = reason end,
                        fork_loads = function() return fork_loads end,
                    }}
                end

                local inst1 = run_instance()
                local expected = {{
                    "prepare_children", "child", "children", "load_transcript",
                    "append_compaction", "append_prune",
                    "set_leaf", "prepare_fork",
                    "snapshot_context", "snapshot",
                    "short_id",
                    "update", "label_item", "archive", "unarchive", "delete", "rewind",
                }}
                local all_present = true
                for _, m in ipairs(expected) do
                    if type(inst1.svc[m]) ~= "function" then
                        all_present = false
                    end
                end

                local prep = inst1.svc.prepare_fork(inst1.svc, inst1.ctx)
                local prep_path = type(prep) == "table" and tostring(prep.path) or ""
                local prep_loads = inst1.fork_loads()
                inst1.set_poison("disk full")
                local ok_after_poison, err_after_poison = pcall(inst1.svc.prepare_fork, inst1.svc, inst1.ctx)
                local poisoned_blocked = (not ok_after_poison) and tostring(err_after_poison):find("persist_failed", 1, true) ~= nil

                local inst2 = run_instance()
                local helper_res = inst2.D.hydrate_verified_fork_images("snap1", {{ ["a.png"] = "imgdata" }})
                inst2.set_poison("disk full")
                local ok_h, err_h = pcall(inst2.svc.prepare_fork, inst2.svc, inst2.ctx)
                local helper_poisoned = (not ok_h) and tostring(err_h):find("persist_failed", 1, true) ~= nil

                return inst1.eager, all_present, prep_path, prep_loads, poisoned_blocked, tostring(helper_res), helper_poisoned
                "###
            ))
            .eval()
            .unwrap();
        for cold in [
            "session.children",
            "session.compaction",
            "session.fork",
            "session.fork_snapshot",
            "session.pickers",
            "session.rewind",
            "session.metadata",
            "session.operations",
            "session.history",
            "session.recorder",
            "session.tool_state",
            "session.transitions",
            "session.snapshots",
        ] {
            assert!(
                !eager.iter().any(|m| m == cold),
                "cold session submodule `{cold}` must be lazy-required on demand, not during apply/commands; loaded={eager:?}"
            );
        }
        assert!(all_present, "all lazy session service methods must be present at service registration time");
        assert_eq!(prep_path, "/tmp/sessions/s1.jsonl");
        assert_eq!(prep_loads, 1, "session.fork must load exactly once on first prepare_fork call");
        assert!(poisoned_blocked, "poison guard must remain enforced after prepare_fork loads session.fork");
        assert_eq!(helper_res, "snap1:imgdata");
        assert!(helper_poisoned, "poison guard must remain enforced when session.fork loads via a D helper");
    }

    #[test]
    fn mare_lib_strip_utf8_bom_strips_at_most_one_leading_bom() {
        let lua = mlua::Lua::new();
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("preset/plugins/lua/mare_lib.lua"),
        )
        .unwrap();
        let (single, double, plain, empty): (String, String, String, String) = lua
            .load(format!(
                r###"
                local lib = (function() {src} end)()
                return lib.strip_utf8_bom("\239\187\191{{}}"),
                    lib.strip_utf8_bom("\239\187\191\239\187\191{{}}"),
                    lib.strip_utf8_bom("{{}}"),
                    lib.strip_utf8_bom("")
                "###
            ))
            .eval()
            .unwrap();
        assert_eq!(single, "{}");
        assert_eq!(double, "\u{FEFF}{}");
        assert_eq!(plain, "{}");
        assert_eq!(empty, "");
    }
}
