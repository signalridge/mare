---@meta _
-- LuaLS definitions: init.lua configuration: setup, options, built-ins, commands, packages, tools, models (entry: types/mare.lua)

-- ---------------------------------------------------------------------------
-- configuration (init.lua)
-- ---------------------------------------------------------------------------

-- BEGIN GENERATED SetupOpts (scripts/gen_options.py)
---@class mare.SetupOpts
---@field agents? mare.AgentLimits root-shared subagent limits: non-negative integers max_active (all live descendant threads) and max_depth (edges from root); zero disables delegation
---@field border? string panel/popup borders: rounded|single|double|thick|hidden
---@field cache_retention? "none"|"short"|"long" prompt cache retention tier: none|short|long (default short)
---@field char_widths? table display width overrides { { from = 0x2600, to = 0x26FF, width = 2 }, { char = "⚡", width = 1 }, … }
---@field copy? table /copy: { always_full = true } skips the code-block picker and copies the whole response
---@field editor? string editor for Ctrl+G (default $VISUAL / $EDITOR)
---@field effort? string invocation-only model effort override
---@field fallback_models? string[] up to 3 models (provider/id or alias) tried in order when the current one is overloaded, missing or out of quota
---@field favorite_models? table models (provider/id or alias) the model-cycle key steps through; without it, the recently used models
---@field flavor? string theme flavor: auto (OSC 11 background query)|dark|light|installed name
---@field flavor_dark? string flavor selected by auto on a dark or unknown terminal background
---@field flavor_light? string flavor selected by auto on a light terminal background
---@field icons? string icon set: nerd|unicode|ascii (default: auto-detect)
---@field images_protocol? string terminal image protocol: kitty|iterm2|sixel|none (default: detected)
---@field key_conflicts? table extra known key conflicts { { key = "ctrl+tab", terminal = "ghostty"|"any", by = "…", fix = "…" }, … } (appended to the built-in table)
---@field keys? table<string, string|false> key -> action shorthand
---@field logo? boolean|string startup logo on a new session: true (pixel logo) | false | "compact" (one line)
---@field lua_memory_mb? integer Lua VM memory limit in MiB (0 disables the limit). CLI/env only: --set lua_memory_mb=… or MARE_LUA_MEMORY_MB (the VM memory limit is applied before init.lua, so a value written in init.lua has no effect)
---@field markdown? table Markdown rendering: { glyphs = { bullet1 = "•", rule = "─", … }, code_line_numbers = false }
---@field model? string model as provider/id
---@field models? table<string, table> per-model overrides
---@field net? table network authorization: { fake_ip = "off"|"auto" } ("off" default; "auto" recognizes fake-IP 198.18.0.0/15 resolution shape and relaxes non-public checks for it)
---@field news? boolean after mare or an enabled plugin is updated, show its NEWS.md once in the scrollback
---@field paste_burst? boolean treat a fast stream of keys as a paste (an Enter inside it inserts a newline); default on Windows only, where pastes aren't bracketed
---@field peer? boolean|table peer socket + registry: false disables; { name = "...", allow = { "agent.prompt", "events.subscribe", ... } } names the session and opens methods beyond the peer defaults; inbound messages always wait for the user
---@field perf? table cache sizes (entries) { highlight_cache = 256, markdown_cache = 128, wrap_cache = 1024, preview_cache = 256 }
---@field plugins? table packages: { url_format = "https://github.com/%s.git" (short names), allow_insecure = false (http://, git://), timeout = 120 (seconds per git operation), concurrency = 8 (parallel fetches), sources = { "owner/repo", … } (more extras catalogs) }
---@field policy_timeout_ms? integer default timeout for policy/rewrite listeners without explicit timeout_ms (ms)
---@field prompt? string input prompt symbol
---@field providers? table<string, mare.ProviderOpts> providers (mare.provider): { base_url, api_key, headers, query_params, retries, stream_idle_timeout_ms, connect_timeout_ms, wire, wire_api, login_hint, proxy, hosted_tools, cache_retention } plus wire-declared provider_fields
---@field reduce_motion? boolean no spinner animation or other motion: static status text instead
---@field shutdown_grace_ms? integer grace period (ms) for cooperative tasks during host shutdown before process-tree termination
---@field slow_callback_ms? number Lua callback busy-time threshold in ms for slow-callback warnings (default 1000 ms; /profile aggregates callbacks over 2 ms)
---@field slow_callback_record_ms? number Lua callback busy-time threshold in ms above which individual slow callbacks are recorded in /profile
---@field slow_callback_table_rows? integer maximum number of (plugin, event) rows shown in the /profile callback timing table
---@field spinner? table spinner glyphs: a list { "a", "b", … } or { frames = {…}, still = "•" } (still: the marker under reduce_motion); default braille
---@field stall_ms? integer a plugin still loading, unloading or waiting for a service after this many ms is reported (/plugins tree, `mare plugin tree --json`, the log, a notice); never aborted; 0 turns it off
---@field terminal? mare.TerminalOpts terminal capability, probing, and ambiwidth configuration (`terminal.{ambiwidth,color,probe.{enabled,background}}`)
---@field terminal_caps? table terminal capability overrides { { match = { TERM_PROGRAM = "…" }, caps = { sync = false, … } }, … }; ui/caps listeners may also adjust them
---@field theme? table<mare.StyleName, string|mare.Style> named style overrides { MareX = spec }
---@field trust_default? string project trust for a folder with no saved decision (after --trust/--no-trust and trust/resolve plugins): ask|trust|distrust
---@field ui? mare.UiOpts UI configuration: { layout = { ... }, working = ..., tool_call = { ... }, stopping = { ... } }
---@field update_check? boolean check GitHub releases for a newer mare (at most daily, interactive sessions; nothing else is sent)
---@field watchdog_ms? integer per-callback limit for Lua that never yields (ms). CLI/env only: --set watchdog_ms=… or MARE_WATCHDOG_MS (the watchdog starts before init.lua, so a value written in init.lua has no effect)
---@field webserver? mare.WebserverOpts Host HTTP/WebSocket RPC listener: { enabled, host, port, token?, trusted_hosts, allowed_origins, remote_admin, tls_cert?, tls_key?, plain_http?, max_connections, header_read_timeout_ms, ws_max_message_bytes, ws_max_frame_bytes, ws_ping_interval_ms, ws_pong_timeout_ms, accept_partial }
-- END GENERATED SetupOpts

---@class mare.WebserverOpts
---@field enabled? boolean
---@field host? string
---@field port? integer
---@field token? string
---@field trusted_hosts? string[]
---@field allowed_origins? string[]
---@field remote_admin? boolean
---@field tls_cert? string
---@field tls_key? string
---@field plain_http? boolean
---@field max_connections? integer
---@field header_read_timeout_ms? integer
---@field ws_max_message_bytes? integer
---@field ws_max_frame_bytes? integer
---@field ws_ping_interval_ms? integer
---@field ws_pong_timeout_ms? integer
---@field accept_partial? string[]

---@class mare.AgentLimits
---@field max_active? integer maximum live descendant threads, including startup and shutdown (default 16; zero disables delegation)
---@field max_depth? integer maximum edges from the root (default 4; zero disables delegation)

--- Layers, lowest first: defaults < state.json < user init.lua < project .mare/init.lua
--- < env `MARE_<NAME>` < CLI `--set name=value`. An unknown name warns (did-you-mean); a wrong
--- type or value is reported and keeps the default (`--strict-config` refuses to start on either);
--- `/config` shows each value with its source and file:line.
---@param opts mare.SetupOpts
function mare.setup(opts) end
--- Override a plugin's default config (lazy.nvim merge: tables merge recursively, lists replace).
---@param name string
---@param overrides table
function mare.plugin(name, overrides) end
--- Clear one layer's override at `path` (`nil` cannot say "clear"). In a config file the value
--- (any depth, e.g. `"plugins.web.search.engine"`) and its sources go, so the default or a later
--- layer applies. At runtime the runtime write of a whole option (`"name"` or
--- `"plugins.<plugin>.<option>"`) goes and the startup configuration shows through again, applied
--- and announced like a write. Clearing what nothing set does nothing.
---@param path string
function mare.config_clear(path) end
--- Every declared plugin option, leaf by leaf (`"plugins.<plugin>.<path>"`; an object's fields are
--- its leaves, a list, map or union is one), sorted — set or not. `/config` and `config.describe`
--- list these.
---@return string[]
function mare.config_declared() end
--- Hide a configuration path and all its descendants in `/config` diagnostics (for example,
--- `plugins.myplugin.private_value`). This does not encrypt the configuration or redact arbitrary
--- plugin output. Marks are retained during config reloads.
---@param path string
function mare.config_sensitive(path) end
---@return table<string, boolean>
function mare.config_sensitive_paths() end
--- Whether a dotted configuration path is sensitive (via `mare.config_sensitive`,
--- `mare.options[k].sensitive`, or sensitive key/segment names such as `api_key`, `headers`, `query`).
---@param path string
---@return boolean
function mare.config_is_sensitive(path) end
--- Redact sensitive values in a configuration value at `path`: the one redactor for every way a
--- config value leaves (`/config`, RPC `options.get` / `config.describe`, `option/changed`). It
--- knows every rule — `mare.config_sensitive` marks, `sensitive` in core options and in plugins'
--- `config_schema` (at the path or a parent), key-name rules.
---@param path string
---@param value any
---@return any redacted, boolean was_redacted
function mare.config_redact(path, value) end

---@class mare.OptionInfo
---@field type string|string[]
---@field luals_type? string
---@field default any
---@field desc string
---@field sensitive? boolean hide this entire option in configuration diagnostics
---@field invocation_only? boolean only accepted from CLI/environment overrides (not saved in state.json)
--- Registry of core options (name → type/default/description).
---@type table<string, mare.OptionInfo>
mare.options = {}
--- Core options, Neovim `vim.o` style: reads fall back to the default; writes are checked
--- and take effect immediately (an `option/changed` event is emitted).
---@type mare.SetupOpts
mare.o = {}

--- Subscribe to a kernel event from init.lua, e.g. `mare.on("model/changed", function(e) ... end)`:
--- a native listener (a plugin's `ctx:on`). A waterfall listener is `fn(value, ..., next)` and
--- passes on with `next(...)`; a decision point is voted on with `mare.policy.voter`. Handlers are
--- dropped and re-registered on `/reload config`.
---@param event string
---@param fn fun(...): any
function mare.on(event, fn) end

mare.policy = {}
--- Vote on a decision point (`tool/before`, `tool/approval`, …) from init.lua, as a plugin's
--- `ctx.policy:voter`: return nil to abstain, `"allow"`, `"ask"`, `"deny"`, or a vote table.
--- Dropped and re-registered on `/reload config`.
---@param point string
---@param fn fun(...): mare.PolicyVote|"allow"|"ask"|"deny"|nil
---@param opts? { timeout_ms?: number } `timeout_ms`: the answer time, 0 = none. Voters run in their declaration order, after the host's readiness vote
function mare.policy.voter(point, fn, opts) end

---@class mare.prompt
mare.prompt = {}
--- Add (or replace) a system-prompt section.
---@param def mare.PromptSection
function mare.prompt.section(def) end
--- Remove a system-prompt section by name (global and agent-specific variants).
---@param name string
function mare.prompt.remove(name) end

---@class mare.command
mare.command = {}
--- `/short` runs `/target ...`.
---@param short string
---@param target string
function mare.command.alias(short, target) end
--- A package declaration (compatible with lazy.nvim specs). `"owner/repo"`, `"owner/repo:subdir"` or a
--- table; with no version field the newest semver tag is used, else the default branch.
--- A package declared more than once is merged: the last write wins for single values and
--- `opts` (deep merge); `dependencies` and `only` are merged; any `enabled = false` disables it
--- (and the dependencies only it needs); a package only declared `optional = true` is dropped.
---@class mare.PackageSpec
---@field [1]? string "owner/repo", "owner/repo:subdir" or a git URL
---@field name? string the shown name next to [1] (lazy.nvim); the source itself when there is no [1]
---@field url? string clone URL (default https://github.com/<owner/repo>.git)
---@field only? string[] plugins of a monorepo to load
---@field subdir? string
---@field version? string semver range over the tags ("^1.2")
---@field tag? string
---@field branch? string
---@field commit? string an exact commit (update leaves it)
---@field rev? string any revision; "=<commit>" is an exact pin
---@field pin? boolean update leaves it alone
---@field opts? table<string, table>|fun(pkg: {name: string}, opts: table): table? per plugin: defaults under that plugin's options (deep-merged, the last declaration wins; mare.plugin wins over all); a function gets the opts so far and returns new ones (or changes them in place)
---@field opts_extend? string[] dotted paths in opts ("todo.sources") whose lists are appended across declarations instead of replaced
---@field config? true|fun(pkg: {name: string, dir: string, subdir?: string}, opts: table) runs after the package's plugins load (again after /reload); true = the default (opts already reach the plugins). Not run for a package that isn't installed or can't load
---@field init? fun(pkg: {name: string, dir: string, subdir?: string}) runs before the package's plugins load
---@field optional? boolean only counts when the package is declared elsewhere too
---@field submodules? boolean false: don't fetch git submodules
---@field enabled? boolean|fun(spec: mare.PackageSpec): boolean false: neither installed nor loaded (its lock entry stays)
---@field dependencies? (string|mare.PackageSpec)[] loaded first (declared too)
---@field dir? string a local directory (lazy.nvim `dir`): loaded from there, never installed
---@field import? string modules under the config dir's `lua/`: `lua/<import>.lua`, `lua/<import>/init.lua`, `lua/<import>/*.lua`, `lua/<import>/<dir>/init.lua`, each returning declarations; imported once, cycles and nesting deeper than 16 are refused; may carry `enabled`
---@param list (mare.PackageSpec|string)[]
function mare.plugins.add(list) end

---@class mare.tool
mare.tool = {}
---@class mare.CommandToolParam
---@field type? "string"|"integer"|"number"|"boolean"
---@field required? boolean
---@field default? any
---@field description? string
---@class mare.CommandTool
---@field name string
---@field description string
---@field command string[] argv template; "{param}" placeholders are filled from arguments
---@field params? table<string, mare.CommandToolParam>
---@field timeout_ms? number
--- Register a tool that runs a command template.
---@param def mare.CommandTool
function mare.tool.command(def) end
--- Override the model-facing description of a tool.
---@param name string
---@param text string
function mare.tool.describe(name, text) end
---@class mare.tools
mare.tools = {}
---@param names string[]
function mare.tools.disable(names) end

---@class mare.ModelDef
---@field provider string
---@field id string
---@field name? string
---@field context_window? integer measured input limit
---@field max_output? integer
---@field reasoning? boolean
---@field efforts? string[]
---@field cost? {input?: number, output?: number, cache_read?: number, cache_write?: number} USD per 1M tokens
---@field edit_tool? "apply_patch"|"replace"
---@field alias? string another name: `/model <alias>`; with plain `params` values it pins them
--- a provider with several routes (opencode-go): this model's route, above every other layer
---@field route? "responses"|"anthropic"|"chat"|"google"|string
---@field tool_update? "addition_only"|false mid-conversation tool update capability (false clears a provider default)
---@field cache_retention? "none"|"short"|"long" prompt cache retention tier (default "short")
--- Wire plugins may declare additional model fields via `WireSpec.model_fields`.
--- this model's parameters: a definition (mare.ParamDef) adds or changes one, a plain value is
--- its default (or, with `alias`, the value the alias pins)
---@field params? table<string, mare.ParamDef|string|boolean>
--- Add a model or override fields of a built-in one.
---@param def mare.ModelDef
function mare.model(def) end

--- One option of an enum parameter.
---@class mare.ParamOption
---@field value string
---@field name? string shown name
---@field tooltip? string
---@field cost? boolean increases cost (marked in /model)
--- per route: a field table (`["a.b"] = v`, `"$value"`, `false` removes, `{at_least = n}` raises,
--- `{value = v}` writes v as it is) or `function(req, ctx)` editing req.body / req.headers
---@field wire? table<"responses"|"anthropic"|"chat"|"google"|string, table|fun(req: {body: table, headers: table}, ctx: {model: mare.Model, route: string, param: string, value: any, params: table})>

--- A model parameter. Without `type` it only changes a
--- parameter another layer defines.
---@class mare.ParamDef
---@field type? "enum"|"bool"
---@field name? string
---@field tooltip? string
---@field default? string|boolean
---@field options? (string|mare.ParamOption)[] ordered: cycling and clamping follow it
---@field only? string[] the options this model offers, in its order
---@field aliases? table<string, string> a value read as another (DeepSeek: xhigh = max)
---@field cycleable? boolean a key can cycle it via `mare.keymap` (effort: Alt+, / Alt+.)
---@field statusline? boolean
---@field visible? boolean
---@field on_unsupported? "clamp"|"error"|"as_is"
--- the parameter's mapping per route, for values whose option has none (`"$value"` passes the
--- value through; a bool parameter's is keyed by true / false)
---@field wire? table<"responses"|"anthropic"|"chat"|"google"|string, table|function>

---@class mare.ParamRule
---@field match? string provider/id glob ("opencode-go/glm-*"; without "/" the model id)
---@field provider? string
---@field family? string
--- a route some plugin provides: a registered wire's `describe.route`, a provider's route, or one
--- a plugin's own rules map (a third-party wire's route included); another one never matches and
--- `/doctor` names it
---@field route? "responses"|"anthropic"|"chat"|"google"|string
---@field define? table<string, mare.ParamDef>
---@field remove? string[]|table<string, string[]> whole parameters, or options of one
---@field on_unsupported? "clamp"|"error"|"as_is" default clamp (nearest option, lower on a tie)
--- Model parameters and their wire mappings. Layers, low → high:
--- built-in < provider < model < user (init.lua; provider < route < family/glob < one model;
--- state.json's remembered values below init.lua's) < trusted project (.mare/init.lua) < agent
--- < session (/model, /effort, keys) < request. Function mappings run with the trust of
--- provider/request hooks; from project scope they can't write authorization / x-api-key.
---@param rule mare.ParamRule
function mare.params(rule) end

---@class mare.ProviderOpts
---@field label? string human-readable display name
---@field wire? string wire protocol name (e.g. "openai-chat", "openai-responses")
---@field wire_api? "chat"|"responses"|"anthropic"|"google"|string shorthand wire selector
---@field dialect? string vendor dialect id understood by the wire plugin (e.g. "deepseek", "openrouter", "kimi", "cerebras", "zai", "together")
---@field models_dev? string|boolean models.dev provider id, or `false` to disable enrichment
---@field base_url? string e.g. a proxy
--- "ENV_NAME", a literal key, or a key helper: a command whose stdout is the key (or JSON
--- {key|token, expires_at}); run one at a time, cached until `refresh_interval_ms` / `expires_at`,
--- and run again after a 401
---@field api_key? string|{cmd: string[], refresh_interval_ms?: integer}
---@field api_key_env? string default environment variable name for credentials
---@field key_prefixes? string[] unambiguous API key prefixes used by `/login` key detection
---@field no_key? boolean whether this endpoint requires no credentials (e.g. local server)
---@field default_model? string default model id for this provider
---@field default_context? integer default context window for unknown models on this provider
---@field default_max_output? integer default max output tokens for unknown models on this provider
---@field default_reasoning? boolean whether unknown models default to reasoning-capable
---@field tool_update? "addition_only" default mid-conversation tool update capability for models on this provider
---@field models? table[] seed model list (replaces preset seeds on an overlay)
---@field list_models? boolean set `false` to disable live `GET /models` fetching
---@field headers? table<string, string|{env?: string, cmd?: string[]}> extra request headers
---@field query_params? table<string, string|{env?: string, cmd?: string[]}> extra URL query parameters
---@field proxy? string HTTP/SOCKS proxy URL for this provider
---@field retries? integer per-provider retry count override
---@field stream_idle_timeout_ms? integer per-provider stream idle timeout in milliseconds
---@field connect_timeout_ms? integer per-provider connect timeout in milliseconds
---@field login_hint? string custom hint shown when credentials are missing
---@field login_label? string label shown in the `/login` provider picker
---@field drop_fields? string[] request body fields stripped before sending
---@field transport? string `"auto"`, `"http"`, or a custom transport name
---@field hosted_tools? table<string, boolean|table> hosted tool declarations for this provider (each wire defines its supported tool names and options)
---@field cache_retention? "none"|"short"|"long" prompt cache retention tier for this provider (default "short")
--- Wire plugins may declare additional provider fields via `WireSpec.provider_fields`.
--- Configure a provider (endpoint / credentials).
---@param name string
---@param opts mare.ProviderOpts
function mare.provider(name, opts) end

--- The currently executing config layer and its root file. An unrelated coroutine or
--- ordinary plugin/runtime call returns {layer="runtime"}, even while init.lua yields.
--- Host-owned, detached metadata; changing this table cannot change config provenance.
---@return {layer: "user"|"project"|"runtime", file?: string}
function mare.config_origin() end

---@class mare.OptionSource
---@field layer string
---@field where? string

---@class mare.OptionDescriptor
---@field effective any redacted effective value (`nil` when unset)
---@field default? any redacted default value, when registered
---@field source? mare.OptionSource
---@field description? string
---@field type? string
---@field sensitive boolean
---@field redacted boolean

--- Resolve a preset-as-data configuration (`defaults < inherited preset < explicit leaf overrides`).
--- Supports `like` inheritance chains with cycle detection and did-you-mean diagnostics on unknown presets.
---@param spec { defaults?: table, presets?: table<string, table>, extra?: table<string, table>, default_preset?: string, inherit_key?: string, stop_at?: table<string, any> }
---@param value? string|table
---@return table? resolved, string? err, { preset?: string, base?: string, presets: table<string, table>, resolve_one: fun(name: string): table?, string?, string? }? meta
function mare.config_resolve_preset(spec, value) end

--- Where each effective config value came from: `{ [key] = {layer, where?} }`.
---@return table<string, mare.OptionSource>
function mare.config_sources() end
--- Validated value of `path` supplied via a dedicated startup CLI flag such as `--permission`,
--- `--yolo`, or `--approval-policy` (unaffected by runtime writes), or `nil` when `path` was not
--- pinned by a CLI flag.
---@param path string
---@return any
function mare.config_cli(path) end
--- Describe configuration options and dotted paths with redacted effective/default values and provenance.
---@param keys? string[]
---@param opts? {overrides?: table<string, any>}
---@return table<string, mare.OptionDescriptor>
function mare.config_describe(keys, opts) end
--- Config warnings (unknown names, wrong types) with file:line.
---@return string[]
function mare.config_warnings() end
--- Re-run init.lua and reload plugins whose options changed (`/reload config`).
--- `pending`: plugins left waiting for a service nobody provides (not failures; they start when it appears).
---@return {changed: string[], reloaded: string[], failures: string[], warnings: string[], pending: string[]}
function mare.reload_config() end
--- Full reload (`/reload`): init.lua again, newly declared packages installed and their
