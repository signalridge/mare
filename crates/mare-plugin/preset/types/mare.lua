---@meta
-- LuaLS type definitions for the mare plugin API (Lua 5.5).
-- Point your editor at this directory (see .luarc.json).
-- The namespaces live next to this file in mare/*.lua (one per namespace); point the
-- library at the whole types/ directory.
-- Independent approval policy/reviewer API: ctx.approvals (mare.ApprovalsService),
-- defined in mare/approvals.lua and mare/services/approvals.gen.lua.

---@class mare
mare = {}
---@type string
mare.version = ""
---@class mare.semver
mare.semver = {}
--- Pure SemVer 2.0.0 precedence comparison (`-1`, `0`, `1`), ignoring build metadata;
--- returns `nil` when either version string is malformed.
---@param a string
---@param b string
---@return integer?
function mare.semver.compare(a, b) end
--- Pure SemVer 2.0.0 precedence check: whether `candidate` has strictly greater
--- precedence than `current` (e.g. `1.0.1-rc.1` > `1.0.0`).
---@param candidate string
---@param current string
---@return boolean
function mare.semver.is_newer(candidate, current) end
--- Release-channel update policy: whether `candidate` should be offered as an update
--- over `current` under SemVer 2.0.0 precedence (prerelease → stable notifies; stable
--- never upgrades to a prerelease).
---@param candidate string
---@param current string
---@return boolean
function mare.semver.is_newer_release(candidate, current) end
--- Protocol and Lua API compatibility version (matches `mare_protocol::PROTOCOL_VERSION`).
---@type string
mare.api_version = ""
---@type string
mare.cwd = ""
---@type "tui"|"print"|"json"|"rpc"|"acp"|"child"
mare.mode = "tui"
--- in a child agent (mare.subagent.start): its id, e.g. "agent-3"; nil in the main session
---@type string|nil
mare.agent_id = nil
--- `--no-user-plugins`: only the built-in plugins load — no init.lua, no user or project
--- plugins, no package (`mare.plugins.dirs` returns nothing: no package skills, prompts, themes, types)
---@type boolean
mare.no_user_plugins = false
--- whether the current project workspace is trusted
---@type boolean
mare.trusted = false
--- process id of this mare
---@type integer
mare.pid = 0
---@type string
mare.os = ""
---@type string
mare.arch = ""
---@type table
mare.config = {}
---@type {session_id?: string, [string]: any}
--- `mare.args.session_id` is the requested session id; in a child VM it is assigned by the host.
--- `mare.args.trust`: "trusted" | "untrusted" | "undecided" (protected project resources were
--- skipped because nobody could be asked) | "none" (nothing protected, nothing saved). Context
--- labels a project's AGENTS.md "from an untrusted project" when untrusted or undecided.
mare.args = {}
---@type {config: string, state: string, data: string, auth: string, sessions: string, packages: string, runtime: string, log: string, crashes: string}
mare.paths = {}
---@class mare.InstallInfo
---@field channel "brew"|"scoop"|"aur"|"nix"|"standalone"|"other"
---@field managed boolean
---@field command string|nil
---@field argv string[]
---@field steps string[][]
---@field manual_hint string|nil
---@field release_url string
---@class mare.install
mare.install = {}
---@return "brew"|"scoop"|"aur"|"nix"|"standalone"|"other"
function mare.install.channel() end
---@param method? "brew"|"scoop"|"aur"|"nix"|"standalone"|"other"
---@return mare.InstallInfo
function mare.install.info(method) end
---@return string|nil
function mare.install.update_command() end
---@return boolean
function mare.install.is_managed() end
---@return boolean
function mare.install.update_check_disabled() end
--- Effective update-check policy resolved from `mare.o.update_check`, plugin `enabled`, and env opt-outs.
---@param config_update_check? boolean|"notify"|"auto"|string
---@param plugin_enabled? boolean
---@return "notify"|"auto"|"disabled"
function mare.install.policy(config_update_check, plugin_enabled) end
---@class mare.Project
---@field root string|nil
---@field start string
---@field ignored string[]
---@field rejected string[]
---@type mare.Project
mare.project = {}
---@class mare.env
---@overload fun(name: string): string|nil
---@field os string
---@field arch string
---@field home string|nil
mare.env = {}
---@param name string
---@return string|nil
function mare.env.get(name) end
---@return string
function mare.env.hostname() end
---@param level string
---@param msg string
function mare.log(level, msg) end
--- "file:line" of the nearest Lua caller outside the core prelude and the config API (`/config`
--- shows where an option was set, like Neovim's `:verbose set`); `skip` skips that many more
--- frames first (a helper names its caller's line, not its own). nil when there is none.
---@param skip? integer
---@return string|nil
function mare.caller_site(skip) end
---@param fmt? string strftime format
---@param ms? number epoch milliseconds
---@return string
function mare.date(fmt, ms) end

--- true when the current project is trusted (after project-trust resolution)
---@type boolean
mare.trusted = false
--- true in development builds (debug assertions on)
---@type boolean
mare.debug_build = false

--- The error a caller of a contract method catches: `code` is a core code
--- (`invalid_arguments`, `denied`, `timed_out`, `internal`, ...) or a declared `<service>.<name>`;
--- `data` is that code's additional data. A provider raises such a table (or returns `nil, it`);
--- anything else reaches callers as `internal` with a bounded message. Prints as `code: message`.
---@class mare.Error
---@field code string
---@field message string
---@field data? any

--- A mare.Error value to raise with `error(value, 0)`: a core code or one the service declares
--- (`<service>.<name>`), with data its declaration describes. It keeps its fields through
--- service proxies and mare.exec.run.
---@param code string
---@param message string
---@param data? any
---@return mare.Error
function mare.error(code, message, data) end

--- One table per name for the life of the Lua VM, the same for every plugin and kept across
--- plugin reloads and provider replacement: state the providers of one contract share (e.g.
--- `mare.shared("my.state")`).
---@param name string
---@return table
function mare.shared(name) end

mare.activity = {}
--- Synchronously invoke a provider and return its isolated JSON value; raises on yield,
--- cycles, non-JSON values or resource limits. Used by activity registries, public to any plugin.
---@param provider fun(): any
---@return any
function mare.activity.capture(provider) end
