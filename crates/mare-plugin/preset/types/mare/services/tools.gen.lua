---@meta _
-- GENERATED from types/mare/services/contracts/tools.lua (service `tools`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ToolsService
local M = {}

--- Bind the tool declarations returned for a model request; returns opaque tokens and bound declarations.
---@param declarations table<string, any>[]
---@return table
---@return table[]
function M:bind_declarations(declarations) end

--- Declare feature capabilities implemented by the active tools service.
---@return {result_pipeline: integer, [string]: any}
function M:capabilities() end

--- Register a declarative command tool (`mare.CommandTool`).
---@param def mare.CommandTool
---@return table
function M:command(def) end

--- Whether tool call `call` is safe to run concurrently with adjacent calls.
---@param call table<string, any>
---@return boolean
function M:concurrency_safe(call) end

--- Current working directory used by tool execution.
---@return string
function M:cwd() end

--- Build wire-ready tool declaration tables for `model` (optionally filtered to `names`).
---@param model? mare.Model
---@param names? string[]
---@return table<string, any>[]
function M:definitions(model, names) end

--- Disable tool `name` for this session.
---@param name string
---@return table
function M:disable(name) end

--- Active edit tool strategy for `model` (`apply_patch` or `replace`).
---@param model? mare.Model
---@return "apply_patch"|"replace"
function M:edit_mode(model) end

--- Re-enable a previously disabled tool `name`.
---@param name string
---@return table
function M:enable(name) end

--- Return the `{env_include, env_exclude}` filter for child processes.
---@return {env_exclude: string[], env_include: string[], [string]: any}
function M:env_policy() end

--- Run a tool call through the guarded rewrite, validation, policy vote, approval, and sandbox execution pipeline.
---@param call {id: string, name: string, arguments: table}
---@param opts? {approve?: (fun(call: table, info: table, verdict: table): table?), cancel?: mare.Token?, on_output?: (fun(blocks: table[], opts?: table): {status: string, error?: string}?), on_state?: ((fun(call: table, state: "prepared"|"started", info: table): boolean?, string?)?), on_update?: fun(partial: table)?, sandbox?: any?, [string]: any}
---@return mare.ToolResult
---@return table
function M:execute(call, opts) end

--- Look up the registered executor backend for an exposure kind, or nil.
---@param exposure string
---@return table<string, any>|nil
function M:executor(exposure) end

--- Atomic, version-checked file write following `fs_guard`.
---@param path string
---@param content string
---@param cur? table<string, any>|nil
---@param tool string
---@param opts? table<string, any>
---@return boolean|nil
---@return string|nil
function M:fs_commit(path, content, cur, tool, opts) end

--- Pre-write guard for edit/write tools: checks read-before-edit and dispatches `fs/edit-intent` or `fs/write-intent`.
---@param path string
---@param verb "edit"|"write"
---@param tool string
---@return {version: table|nil, data: string|nil}|nil
---@return string|nil
function M:fs_guard(path, verb, tool) end

--- Return the registered tool definition for `name`, or nil.
---@param name string
---@return mare.ToolDef|nil
function M:get(name) end

--- Whether any registered tool carries dynamic admission hooks.
---@return boolean
function M:has_dynamic_admission() end

--- Invalidate cached tool declaration snapshots.
---@return table
function M:invalidate() end

--- Whether tool `name` is currently disabled.
---@param name string
---@return boolean
function M:is_disabled(name) end

--- Return all registered tool definitions.
---@return mare.ToolDef[]
function M:list() end

--- Return the latest read-before-edit observation record for `path`, or nil.
---@param path string
---@return ({hash?: string, kind: "present"|"absent", mtime?: number, size?: integer, [string]: any})|nil
function M:observation(path) end

--- Record that the agent has observed `path` (with optional read `data`); emits `fs/observed`.
---@param path string
---@param data? string
---@return table
function M:observe(path, data) end

--- Open an optional streaming output channel owned by the tools service, or `nil, err`.
---@return {close: fun()}|nil
---@return string?
function M:output_channel() end

--- Return `MARE_*` environment variables passed to child tool processes.
---@return table<string, string>
function M:process_env() end

--- Whether the named tool is declared read-only.
---@param name string
---@return boolean
function M:readonly(name) end

--- Register a tool owned by the caller; released on disposer/release or disconnect. Remote registrations force `unconfined = true` and dispatch `execute(args, ctx)` via `callback.call`.
---@param name string
---@param opts {access?: (fun(args: table, ctx?: table): table[]), concurrency_safe?: (boolean|fun(args: table): boolean), description: (string|fun(): string), exposure?: "direct"|"deferred"|"nested", guidelines?: (string[]|fun(): string[]), idempotent?: boolean, parameters: (table|fun(model?: mare.Model): table), side_effect?: "read"|"write"|"external", snippet?: string, timeout_ms?: (number|boolean), admit?: (fun(): boolean|string|nil), allow_nested?: boolean, command?: table, command_tool?: boolean, display?: table, enabled?: (boolean|fun(model?: mare.Model, edit_mode?: string): boolean), examples?: table[], execute?: (fun(args: table, ctx?: mare.ToolContext): mare.ToolResult|string), freeform?: table, is_current?: (fun(): boolean), output_schema?: table, param_order?: string[], permission_request?: boolean, render?: (fun(...): any), search?: table, sensitive_inputs?: string[], sort_key?: string, summary?: (string|fun(args: table): string), terminate?: boolean, timeout_policy?: "managed", unconfined?: boolean}
---@return fun()
---@return fun(): boolean
function M:register(name, opts) end

--- Register a named tool executor backend owned by the calling ctx.
---@param executor_id string
---@param definition {api_version: integer, exposures: string[], run: fun(spec: table, caller?: mare.Ctx): table}
---@return fun() dispose releases this registration
function M:register_executor(executor_id, definition) end

--- Register a search source prepared before `tool_search` reads the deferred catalog.
---@param definition {prepare: async fun()}
---@return fun() dispose releases this registration
function M:register_search_source(definition) end

--- Monotonic tool registry revision counter.
---@return integer
function M:revision() end

--- Send structured output blocks over a channel returned by `output_channel`.
---@param channel {close: fun()}
---@param blocks table<string, any>[]
---@param opts? {cancel?: mare.Token?, [string]: any}
---@return {error?: string, status: "accepted"|"rejected", [string]: any}
function M:send_output(channel, blocks, opts) end

--- Re-register a copy of a tool returned by `get`; revoked with its exact parent version.
---@param name string
---@param def table
---@return fun()
---@return fun(): boolean
function M:shadow(name, def) end

--- Return the default shell configuration (`mare.ShellConfig|nil, err?`) for model commands.
---@return mare.ShellConfig|nil
---@return string?
function M:shell() end

--- Build `(shell, args, input?)` for executing a shell command string.
---@param cmd string
---@return string
---@return string[]
---@return string?
function M:shell_command(cmd) end

--- Return `(side_effect, idempotent, unconfined)` classification for tool `name`.
---@param name string
---@return "read"|"write"|"external"
---@return boolean
---@return boolean
function M:side_effect(name) end

--- Prompt snippets for tools enabled for the given model, in registration order.
---@param model? mare.Model
---@return {guidelines?: string[], name: string, snippet: string, [string]: any}[]
function M:snippets(model) end

--- Bump the tool registry generation (for example after deferred tool loading changes).
---@return table
function M:touch() end

--- Run `fn` while holding the in-process file mutation locks for `paths` in sorted realpath order.
---@param paths string[]
---@param fn fun(): any
---@return any
function M:with_file_lock(paths, fn) end
