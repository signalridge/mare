//! Where plugin definitions come from: [`Origin`], [`PluginSource`] and the evaluated [`PluginDef`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use mlua::{Lua, Table};

use crate::bytecode::load_cached_chunk;

/// Where a plugin definition came from (load order = precedence order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Builtin,
    Package,
    User,
    Project,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Package => "package",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "builtin" => Some(Self::Builtin),
            "package" => Some(Self::Package),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// Plugin source: embedded string or on-disk file (reloadable).
#[derive(Clone, Debug)]
pub struct PluginSource {
    pub name_hint: String,
    pub origin: Origin,
    pub path: Option<PathBuf>,
    /// embedded code: Lua source or precompiled bytecode
    pub code: Option<&'static [u8]>,
}

impl PluginSource {
    pub fn file(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| if p.is_dir() { p.join("init.lua") } else { p.clone() })
    }

    /// Source text (embedded bytecode yields an empty string; use `bytes` to load).
    pub fn read(&self) -> anyhow::Result<String> {
        if let Some(p) = self.file() {
            return Ok(std::fs::read_to_string(&p)?);
        }
        Ok(self.code.and_then(|c| std::str::from_utf8(c).ok()).unwrap_or("").to_string())
    }

    /// Load this plugin source directly into an executable Lua function bound to `env`.
    /// Third-party / user / project on-disk plugins go through the bytecode cache with a single
    /// undump on cache hit; built-in plugins load directly from embedded bytecode or source text
    /// without touching the disk bytecode cache.
    pub fn load_chunk(&self, lua: &Lua, env: Table) -> anyhow::Result<mlua::Function> {
        let chunk_name = self.chunk_name();
        match self.file() {
            Some(path) if self.origin == Origin::Builtin => {
                mare_shared::exec_ctx::check_process_path(lua, "read", &path.to_string_lossy())?;
                let bytes = std::fs::read(&path)?;
                lua.load(&bytes[..])
                    .set_mode(mlua::chunk::ChunkMode::Text)
                    .set_name(&chunk_name)
                    .set_environment(env)
                    .into_function()
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }
            Some(path) => {
                let owner_path = self.path.as_deref().unwrap_or(&path);
                load_cached_chunk(lua, Some(owner_path), &path, &chunk_name, Some(env))
            }
            None => {
                let code = self.code.unwrap_or(b"");
                let mode = if code.starts_with(b"\x1bLua") {
                    mlua::chunk::ChunkMode::Binary
                } else {
                    mlua::chunk::ChunkMode::Text
                };
                lua.load(code)
                    .set_mode(mode)
                    .set_name(&chunk_name)
                    .set_environment(env)
                    .into_function()
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }
        }
    }
    pub fn chunk_name(&self) -> String {
        match &self.path {
            Some(p) => format!("@{}", p.display()),
            None => format!("@builtin/{}.lua", self.name_hint),
        }
    }
}

/// Methods declared for a service, independently of any implementation. Every service contract
/// carries one [`MethodSpec`] per method (or is an event-only contract root with `owner` set).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ServiceContract {
    pub required: Vec<String>,
    pub optional: Vec<String>,
    pub doc: Option<String>,
    pub methods: BTreeMap<String, crate::dispatch::contracts::MethodSpec>,
    /// The service's own error codes (`errors = { name = { doc, data? } }`), raised as `<service>.<name>`.
    pub errors: BTreeMap<String, crate::dispatch::contracts::ErrorSpec>,
    /// Failure of a plugin responsible for this built-in service blocks requests.
    pub essential: bool,
    /// Methods allowed when the session barrier is poisoned (`None`: service is not guarded).
    pub poisoned_allow: Option<BTreeSet<String>>,
    /// When set, this entry is an event-only contract root (`contracts.<root>.events`) owned by
    /// the named service.
    pub owner: Option<String>,
    /// Contract event declarations keyed by short kebab-name (`contracts.<root>.events`).
    pub events: BTreeMap<String, crate::dispatch::contracts::EventSpec>,
    /// Named provider capabilities backed by declared methods (`capabilities = { name = { method, doc } }`).
    pub capabilities: BTreeMap<String, crate::dispatch::contracts::CapabilitySpec>,
    /// Named LuaLS types generated from contract schemas (`types = { stem = { class, schema, required?, lua_fields? } }`).
    pub types: BTreeMap<String, crate::dispatch::contracts::NamedTypeSpec>,
}

/// Declarative static frontend assets on a plugin definition (`web = { dir = ... }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPluginDef {
    pub dir: String,
    pub prefix: Option<String>,
    pub index: Option<String>,
    pub fallback: Option<String>,
}

/// A loaded (evaluated) plugin definition.
#[derive(Clone)]
pub struct PluginDef {
    pub name: String,
    pub source: PluginSource,
    pub table: Table,
    /// the plugin's isolated environment; modules it `require`s from its `lua/` dir run in it
    pub env: Table,
    pub inject: Vec<String>,
    /// Service contracts declared by this definition, independently of provide/apply.
    pub contracts: BTreeMap<String, ServiceContract>,
    pub provide: Vec<String>,
    /// Declarative CLI flags registered by this definition (`flags = { name = { type, ... } }`).
    pub flags: Vec<mare_shared::flags::FlagSpec>,
    /// Declarative web asset mount (`web = { dir, prefix?, index?, fallback? }`).
    pub web: Option<WebPluginDef>,
    /// `tags = { "provider", … }`: what kind of plugin it is; `plugins.preset
    /// = "minimal"` keeps the built-ins tagged `minimal` or `provider`
    pub tags: Vec<String>,
    /// `wires = { "<wire>", … }`: the llm wire formats it registers (`llm:register_wire`), so a
    /// route can name the wire's plugin while that plugin is turned off or failed
    pub wires: Vec<String>,
    /// `providers = { "<provider>", … }`: the llm provider names it registers
    /// (`llm:register_provider`), declared up front so missing-provider diagnosis and
    /// undeclared-provider warnings work reliably
    pub providers: Vec<String>,
    /// `override = true`: may replace a same-name definition from a lower origin
    pub overrides: bool,
    /// the origin of the definition this one replaced (set when the definitions are merged)
    pub replaced: Option<Origin>,
    /// the services the definition(s) it replaced declared (set when the definitions are
    /// merged): a replacement takes over their duty, whether or not it declares them itself
    pub replaced_provide: Vec<String>,
}

/// What a definition declares about itself, kept for every definition read for the current
/// config, by name and source, the ones turned off or replaced included: a route can still name
/// the plugin of a wire whose plugin is off (a package's metadata can be read without running
/// it), and `mare.plugins.status(name).sources` lists every source of a name.
#[derive(Clone, Debug, Default)]
pub struct Declared {
    pub name: String,
    pub tags: Vec<String>,
    pub wires: Vec<String>,
    pub providers: Vec<String>,
    /// `builtin`, `package`, `user` or `project`
    pub origin: String,
    pub path: Option<std::path::PathBuf>,
    pub provide: Vec<String>,
    /// Service names declared by contracts, retained when this definition is disabled.
    pub contracts: Vec<String>,
    /// `override = true`
    pub overrides: bool,
    /// the one the same-name rule picks for the name (not a refused twin or a refused
    /// definition without `override`); the host marks it ([`Kernel::set_declared`])
    pub effective: bool,
}

impl PluginDef {
    /// What it declares about itself.
    pub fn declared(&self) -> Declared {
        Declared {
            name: self.name.clone(),
            tags: self.tags.clone(),
            wires: self.wires.clone(),
            providers: self.providers.clone(),
            origin: self.source.origin.as_str().to_string(),
            path: self.source.path.clone(),
            provide: self.provide.clone(),
            contracts: self.contracts.keys().cloned().collect(),
            overrides: self.overrides,
            effective: false,
        }
    }

    /// The services this plugin is responsible for: the ones it declares in `provide` and the
    /// ones declared by the definitions it replaced (`override = true`).
    pub fn services(&self) -> Vec<String> {
        let mut out = self.provide.clone();
        out.extend(self.replaced_provide.iter().filter(|s| !self.provide.contains(s)).cloned());
        out
    }
}
