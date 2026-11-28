---@meta _
-- mare.plugins: what any plugin can ask about the plugins. The host keeps these
-- facts (failures, the blocking rule, explicit disables); the built-ins use this API like any
-- other plugin. Computed when called, so it always matches the running plugins; only failures()
-- answers from a snapshot while a full /reload runs.

--- One plugin's state.
---@class mare.PluginStatus
---@field name string
---@field defined boolean a definition with this name is registered (false: unknown, turned off, or its file could not be read)
---@field active boolean it is running (applied)
---@field disabled boolean turned off by the current config explicitly (`enabled = false`) and not running; one left out by `plugins.preset` is `defined = false`
---@field origin? "builtin"|"package"|"user"|"project" where the registered definition comes from
---@field overrides? "builtin"|"package"|"user"|"project" the origin of the same-name definition it replaced (`override = true`)
---@field provide string[] the services its definition declares
---@field tags string[] its definition's `tags` (also for a definition turned off by the config)
---@field wires string[] its definition's `wires` (also for a definition turned off by the config)
---@field providers string[] its definition's `providers` (also for a definition turned off by the config)
---@field sources mare.PluginSource[] every definition read under this name, lowest source first: the replaced, the refused and the turned-off ones included
---@field problems mare.PluginProblem[] active declaration warnings attached to this plugin
---@field failure? mare.PluginFailure its latest startup or reload failure, until a full /reload succeeds

--- A non-fatal declaration warning attached to a plugin, keyed by `(plugin, reporter, key)`.
---@class mare.PluginProblem
---@field plugin string the target plugin name
---@field reporter string the plugin (or `"host"`) that reported the problem
---@field key string stable per-problem key under that reporter
---@field kind "declaration"
---@field message string

--- One definition read under a plugin name.
---@class mare.PluginSource
---@field origin "builtin"|"package"|"user"|"project"
---@field path? string its file or directory (none for a built-in compiled into mare)
---@field tags string[]
---@field wires string[]
---@field providers string[]
---@field provide string[]
---@field override boolean it declares `override = true`

--- A plugin or config file that failed at startup or in the latest full /reload (kept until a
--- full /reload succeeds).
---@class mare.PluginFailure
---@field plugin string the plugin's name, or the config file (`init.lua`, `.mare/init.lua`)
---@field kind "plugin"|"config"
---@field phase "definition"|"setup"|"start"|"override"|"config" `start`: a dependency is missing; `override`: refused for colliding without `override = true`
---@field summary string one line: what failed, where, and the error without its traceback
---@field error string the whole error, traceback included
---@field blocks boolean every request is refused until it is fixed: the plugin is responsible for an essential service (agent, llm, tools, permissions, session), declares `required = true`, or belongs to an enabled package; a config file always blocks

---Runtime plugin inspection and package management share this table. `list()` and `status(name)`
---describe runtime definitions; `list_installed()` describes declared packages and their checkouts.
---@class mare.plugins
mare.plugins = {}

--- The state of plugin `name`; an unknown name gives `defined = false`.
---@param name string
---@return mare.PluginStatus
function mare.plugins.status(name) end

--- Every defined plugin, in definition order.
---@return mare.PluginStatus[]
function mare.plugins.list() end

--- What failed at startup or in the latest full /reload: plugins (also those whose definition
--- could not be read) and config files. During a full /reload, returns the failures as they were
--- when the reload started (a half-reloaded tree has no meaningful answer); live again when it
--- ends, before `reload/completed`.
---@return mare.PluginFailure[]
function mare.plugins.failures() end

--- The plugins whose definition declares `value` in `field` ("tags", "wires", or "providers"):
--- the defined ones in definition order, then the ones turned off by the config (`enabled = false`,
--- `plugins.preset`). Read from the definitions, so a plugin that is off or failed still counts.
---@param field "tags"|"wires"|"providers"
---@param value string
---@return string[]
function mare.plugins.declaring(field, value) end

--- Record or replace a non-fatal declaration problem on `plugin` under `(plugin, reporter, key)`.
--- Automatically cleared when either the target plugin or the reporter reloads or unloads.
---@param plugin string
---@param key string
---@param message string
function mare.plugins.report_problem(plugin, key, message) end

--- Clear the declaration problem on `plugin` reported by the caller under `key`.
---@param plugin string
---@param key string
function mare.plugins.clear_problem(plugin, key) end

-- LuaLS definitions: the package manager (entry: types/mare.lua)

---@class mare.PluginManifest
---@field name? string
---@field description? string
---@field requires? string
---@field version? string
---@field provides? {tools: string[], commands: string[], hooks: string[], skills: string[], prompts: string[], themes: string[], services: string[], flags: string[], mcp: string[]}
---@field platforms? ("linux"|"macos"|"windows")[]
---@field keywords? string[]
---@field license? string
---@field homepage? string
---@field news? string
---@field dependencies? string[]
---@field skills? string[]
---@field prompts? string[]
---@field themes? string[]
---@field category? string
---@field deps? {required: string[], optional: string[]}
---@field conflicts? string[]
---@field plugins? string[]

--- Package manager for `/plugins`; the same operations as `mare plugin …`.
---@async
---@param opts? {cancel?: mare.Token, manifest?: boolean}
---@return {name: string, scope: "user"|"project"|"init.lua", display: string, url: string, dir: string, local: boolean, installed: boolean, disabled: boolean, commit?: string, locked?: string, branch?: string, dirty: boolean, problem?: string, constraint?: string, manifest?: mare.PluginManifest}[] commit: what is checked out (HEAD); locked: the lock's
function mare.plugins.list_installed(opts) end
---@async
---@param action "sync"|"install"|"update"|"uninstall"|"restore"|"clean"|"log"|"diff"
---@param name? string only this package (log / diff need it)
---@param opts? {on_line?: fun(line: string), cancel?: mare.Token, stash?: boolean, force?: boolean, dry_run?: boolean} on_line: the log as it comes; cancel stops git; dry_run previews clean without changes
---@return {ok: boolean, log: string[], error?: string, cancelled?: boolean}
function mare.plugins.run(action, name, opts) end
--- Install a catalog plugin by short name or sync a declared source.
---@async
---@param name? string
---@param opts? {on_line?: fun(line: string), cancel?: mare.Token}
---@return {ok: boolean, log: string[], error?: string, cancelled?: boolean}
function mare.plugins.install(name, opts) end
--- Update declared packages to their newest allowed revision (`update`).
---@async
---@param name? string
---@param opts? {on_line?: fun(line: string), cancel?: mare.Token, stash?: boolean}
---@return {ok: boolean, log: string[], error?: string, cancelled?: boolean}
function mare.plugins.update(name, opts) end
--- Fetch and compare revisions without moving checkouts or modifying locks.
---@async
---@param names? string[]
---@param opts? {cancel?: mare.Token}
---@return {name: string, status: "up_to_date"|"outdated"|"pinned"|"local"|"unreachable", behind_count: integer, commits: string[], breaking: boolean, error?: string}[]
function mare.plugins.check(names, opts) end
--- Uninstall a catalog plugin by short name or clean an unused source.
---@async
---@param name? string
---@param opts? {on_line?: fun(line: string), cancel?: mare.Token}
---@return {ok: boolean, log: string[], error?: string, cancelled?: boolean}
function mare.plugins.uninstall(name, opts) end
--- Read `mare-plugin.toml` for an installed, enabled package.
---@async
---@param name string
---@return mare.PluginManifest?
function mare.plugins.manifest(name) end
--- The plugin catalog with `enabled` / `recommended` for this project.
---@async
---@return {name: string, scope: "user"|"project"|"init.lua", plugin: string, source: string, category: string, description: string, required: string[], optional: string[], conflicts: string[], plugins: string[], needed_by: string[], enabled: boolean, managed: boolean, recommended: boolean, provides?: {tools: string[], commands: string[], hooks: string[], skills: string[], prompts: string[], themes: string[], services: string[], flags: string[], mcp: string[]}, platforms: ("linux"|"macos"|"windows")[], version?: string, news?: string}[]
function mare.plugins.catalog() end
--- Catalog and installed subplugin manifest problems.
---@async
---@return {plugin: string, path: string, constraint?: string, message: string}[]
function mare.plugins.diagnostics() end
--- `mare doctor` checks: git and its version, the lock, plugin names defined twice.
---@async
---@return {name: string, ok: boolean, detail: string}[]
function mare.plugins.doctor() end
--- Files whose change means "/reload to apply" (init.lua, plugins.json, a trusted .mare/init.lua).
---@return string[]
function mare.plugins.watched() end
---@async
---@param name string "plugin-name"
---@param on boolean
---@param source? string catalog source when a short name appears in more than one source
---@return true|nil ok
---@return string? err a conflict with an enabled plugin
function mare.plugins.set_enabled(name, on, source) end
---@async
---@return {ok: boolean, log: string[], error?: string}
function mare.plugins.fetch_catalog() end
---@return string text mare's NEWS.md
function mare.plugins.mare_news() end
--- Read a UTF-8 read-only resource from an installed package's `resources/<path>` directory
--- (in load order, or scoped to `opts.package`), falling back to built-in `preset/resources/<path>`.
--- Files ending in `.gz` are transparently gunzipped. Returns `nil` when not found.
---@param path string relative path inside `resources/` (no `..` or leading `/`)
---@param opts? {package?: string}
---@return string? text
function mare.plugins.read_resource(path, opts) end
--- Format version for `state/news.json`.
---@type integer
mare.plugins.news_version = 1
--- The installed packages' `skills/`, `prompts/`, `themes/` or `types/`
--- directories (or custom paths declared in `mare-plugin.toml`, or package roots when `kind` is
--- omitted), in load order (`context` reads skills and prompts, `theme-default` reads
--- themes).
--- Empty under `--no-user-plugins` (`mare.no_user_plugins`): no package loads, nor its content.
---@param kind? "skills"|"prompts"|"themes"|"types"
---@return {name: string, dir: string}[]
function mare.plugins.dirs(kind) end
---@type string
mare.plugins.default_source = "signalridge/mare-plugins"
