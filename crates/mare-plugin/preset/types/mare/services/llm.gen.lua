---@meta _
-- GENERATED from types/mare/services/contracts/llm.lua (service `llm`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.LlmService
local M = {}

--- Return the winning credential descriptor for `provider` (`init.lua` > `auth.json` > env), or nil.
---@param provider string
---@param env_names? string[]
---@return ({key?: string, kind: "oauth"|"api_key"|"command", source: string, tier: "config"|"auth"|"env", [string]: any})|nil
function M:active_credential(provider, env_names) end

--- Return the currently active effort scope name, or nil.
---@return string|nil
function M:active_scope() end

--- Return what a model alias points to, or nil.
---@param name string
---@return string|nil
function M:alias_target(name) end

--- Resolve the API key string for `provider` from config, `auth.json`, or `env_names`, or nil.
---@param provider string
---@param env_names? string[]
---@return string|nil
function M:api_key(provider, env_names) end

--- Describe where a provider's API key would come from (never returns the secret), or nil.
---@param name string
---@param envs? string[]
---@return string|nil
function M:api_key_source(name, envs) end

--- Write resolved request parameters into `wire.body` and `wire.headers`.
---@param req table
---@param wire {body: table, headers?: table}
---@param opts? {only?: table<string, boolean>, prepare?: boolean, ctx?: table?, [string]: any}
---@return table
function M:apply_params(req, wire, opts) end

--- Read a stored credential record from `auth.json` (`version` is reserved).
---@param key string
---@return any
function M:auth_get(key) end

--- Stored credential key names in `auth.json`, optionally filtered by literal prefix; never returns secret values.
---@param prefix? string
---@return string[]
function M:auth_keys(prefix) end

--- Atomically write or delete a credential record in `auth.json`; returns whether committed.
---@param key string
---@param value? any
---@param opts? {expected?: table<string, any>, locked?: boolean, [string]: any}
---@return boolean
function M:auth_set(key, value, opts) end

--- Return JSON-safe masked credential and directory status for all providers (or one provider).
---@param provider? string
---@return {has_credentials: boolean, providers: ({account?: string, expires_at_ms?: integer, hint?: string, label?: string, logged_in: boolean, method?: string, no_key?: boolean, origin?: "plugin"|"config"|"rpc", overridden: string[], plugin?: string, provider: string, reason?: string, source?: string, state: "registered"|"disabled"|"failed"|"unresolved", wire?: string})[], [string]: any}
function M:auth_status(provider) end

--- Return the resolved base URL for `provider` (falling back to `default`), or nil.
---@param provider string
---@param default? string
---@return string|nil
function M:base_url(provider, default) end

--- Optional lifecycle extension: cancel auxiliary LLM requests belonging to `session_id`.
---@param session_id string
---@return table
function M:cancel_side_requests(session_id) end

--- Return a detached JSON-safe capability descriptor for `ref` (default current model), or `nil, err`.
---@param ref? (string|mare.Model)
---@return table|nil
---@return string|nil
function M:capabilities(ref) end

--- Clamp `effort` to the levels supported by `model`, or return nil.
---@param model mare.Model
---@param effort? string
---@return string|nil
function M:clamp_effort(model, effort) end

--- Classify a provider error table or message into `(kind, retryable)`.
---@param err any
---@param cancelled? boolean
---@return string
---@return boolean
function M:classify(err, cancelled) end

--- Clear a scoped effort override (`save = false` hides it for this session only).
---@param scope string
---@param model? (mare.Model|string)
---@param opts? {checked?: boolean, save?: boolean, [string]: any}
---@return table
function M:clear_scope(scope, model, opts) end

--- Close a provider HTTP response immediately to release its connection.
---@param resp any
---@return table
function M:close_response(resp) end

--- Run provider-side compaction; returns `{items, model_id, provider_id}` or `nil, err, kind`.
---@param opts {session_id?: string, system: string, tools: table<string, any>[], cancel?: any?, messages?: mare.Message[], model?: mare.Model?, [string]: any}
---@return {items: table[], model_id: string, provider_id: string}|nil
---@return string|nil
---@return string|nil
function M:compact_remote(opts) end

--- Non-streaming convenience wrapper around `stream`; returns the final assistant `mare.Message`.
---@param req mare.LlmRequest
---@return mare.Message
function M:complete(req) end

--- Apply all registered tool declaration constraints to `req` and return the constrained request copy.
---@param req mare.LlmRequest
---@return mare.LlmRequest
function M:constrain_request(req) end

--- Register a monotone tool declaration constraint applied once per model request; returns a remove disposer.
---@param fn fun(info: {purpose?: string, session_id?: string, model_ref?: string}): string[]|nil
---@return fun() dispose releases this registration
function M:constrain_tools(fn) end

--- Return all credential sources present for `provider`, active first (never returns secrets).
---@param provider string
---@return ({account?: string, active: boolean, expires_at_ms?: integer, kind: "oauth"|"api_key"|"command", source: string, [string]: any})[]
function M:credential_sources(provider) end

--- Return the currently selected `mare.Model`, or nil.
---@return mare.Model|nil
function M:current() end

--- Advance a `cycleable` parameter to the next offered option (`dir` +1 / -1); returns `value, err`.
---@param name string
---@param dir? integer
---@return any|nil
---@return string|nil
function M:cycle_param(name, dir) end

--- Register model parameter definitions and wire mappings owned by the calling plugin.
---@param rule mare.ParamRule|{when?: (fun(model: mare.Model): boolean), where?: string}
---@param layer? "provider"|"user"
---@return table
function M:define_params(rule, layer) end

--- Return a copy of the registered wire's `mare.WireDescribe`, or nil.
---@param name string
---@return mare.WireDescribe|nil
function M:describe_wire(name) end

--- Return the effective effort clamped to the current model (including active_scope override if set).
---@return string|nil
function M:effort() end

--- Return the effort values offered for `model` on its active route (same source as `param_list`), or nil.
---@param model? mare.Model
---@return string[]|nil
function M:effort_options(model) end

--- Return the built-in effort levels, lowest first ("off" ... "max").
---@return string[]
function M:efforts() end

--- Format human-readable explanation lines for the effective parameters of `ref`.
---@param ref? (string|mare.Model)
---@return string[]
function M:explain_lines(ref) end

--- Return the effective parameters, source layers, wire fields, and options for `ref`.
---@param ref? (string|mare.Model)
---@return table<string, any>|nil
function M:explain_params(ref) end

--- Compute the crash-guard fingerprint key of a model request.
---@param req table
---@return string
function M:fingerprint(req) end

--- The session's current model, or with ref any model the providers resolve (hidden ones and ids a provider serves without listing them included); never changes the selection.
---@param ref? string
---@return {available: boolean, context_window?: integer, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: "apply_patch"|"replace", efforts: string[], images: boolean, max_output?: integer, model_id: string, name?: string, parameters: ({cycleable?: boolean, default?: any, name?: string, options: ({cost?: number|table, name?: string, tooltip?: string, value: any})[], parameter_id: string, statusline?: any, tooltip?: string, type?: string, values: any[], visible?: any})[], provider: string, ready: boolean, ready_reason?: string, reasoning: boolean, ref: string, route?: string, route_error?: string, tool_update?: "addition_only", unavailable_reason?: string, wire_route?: string}
function M:get_model(ref) end

--- Return the mutable session selection snapshot (`model`, `default_model`, `recent`, `params`) for resume comparison.
---@return {default_model?: string, model?: string, params?: table<string, any>, recent?: string[], [string]: any}
function M:get_selection() end

--- Return the global unscoped effort clamped to `model`, ignoring `active_scope`.
---@param model? (mare.Model|string)
---@return string|nil
function M:global_effort(model) end

--- Whether an API key is available for `provider` without returning the secret.
---@param provider string
---@param env_names? string[]
---@return boolean
function M:has_api_key(provider, env_names) end

--- Whether `auth.json` holds a credential under `key` without returning the secret.
---@param key string
---@return boolean
function M:has_auth(key) end

--- Whether at least one provider has usable credentials (or the current model requires none).
---@return boolean
function M:has_credentials() end

--- Build a classified provider error table from a non-2xx HTTP response.
---@param label string
---@param resp mare.Response
---@param opts? {image_limit?: (fun(msg: string, code?: string): boolean)?, map?: fun(err: table, api_err: table, body: any)?, overflow?: (fun(msg: string, code?: string): boolean)?, [string]: any}
---@return table<string, any>
function M:http_error(label, resp, opts) end

--- Identify a registered login-capable provider from a pasted API key using declared `key_prefixes`, or nil when unmatched or ambiguous.
---@param key string
---@return string|nil
function M:identify_key_provider(key) end

--- Drop the cached API key for a key-helper provider so the next call re-runs the command.
---@param provider string
---@return table
function M:invalidate_api_key(provider) end

--- Invalidate in-memory catalog models for `name` (or all providers when omitted).
---@param name? string
---@return table
function M:invalidate_catalog(name) end

--- Stored credential key names in `auth.json`, optionally filtered by literal prefix; never returns secret values.
---@param prefix? string
---@return {data: string[]}
function M:list_auth_keys(prefix) end

--- Every visible model with its abilities, sorted by ref.
---@return {data: ({available: boolean, context_window?: integer, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: "apply_patch"|"replace", efforts: string[], images: boolean, max_output?: integer, model_id: string, name?: string, parameters: ({cycleable?: boolean, default?: any, name?: string, options: ({cost?: number|table, name?: string, tooltip?: string, value: any})[], parameter_id: string, statusline?: any, tooltip?: string, type?: string, values: any[], visible?: any})[], provider: string, ready: boolean, ready_reason?: string, reasoning: boolean, ref: string, route?: string, route_error?: string, tool_update?: "addition_only", unavailable_reason?: string, wire_route?: string})[]}
function M:list_models() end

--- Providers and their masked login state; never a secret value.
---@return {data: ({account?: string, expires_at_ms?: integer, hint?: string, label?: string, logged_in: boolean, method?: string, no_key?: boolean, origin?: "plugin"|"config"|"rpc", overridden: string[], plugin?: string, provider: string, reason?: string, source?: string, state: "registered"|"disabled"|"failed"|"unresolved", wire?: string})[]}
function M:list_providers() end

--- List providers that currently have credentials stored in `auth.json`.
---@return string[]
function M:logged_in() end

--- Run a provider's login. Without key the flow is interactive and needs a connected UI; Lua callers may pass their own say/ask/choose functions.
---@param provider string
---@param opts? {key?: string, method?: "oauth"|"device"|"api_key"|"auto", ask?: (fun(title: string, opts?: table): string|nil), cancel?: (fun(): mare.Token|nil), choose?: (fun(title: string, items: table[]): integer|nil), say?: fun(text: string)}
---@return table
function M:login(provider, opts) end

--- Run an interactive API-key login flow for provider `name`.
---@param name string
---@param title? string
---@param lio? table
---@return table
function M:login_api_key(name, title, lio) end

--- Complete a login I/O table with the connected UI's `say`, `ask`, and `choose` callbacks.
---@param lio? table
---@return table<string, any>
function M:login_io(lio) end

--- Remove every stored credential of a provider (revoked first where possible); kept lists the environment and config sources that cannot be removed here.
---@param provider string
---@return {kept: string[], removed: string[], warnings: string[]}
function M:logout(provider) end

--- Return the default value for `field` declared via the provider's `model_defaults` globs.
---@param model mare.Model|table
---@param field string
---@return any
function M:model_default(model, field) end

--- Return per-field model metadata provenance (`user`, `measured`, `server`, `static`, `models_dev`, `default`), or nil.
---@param ref? (mare.Model|string)
---@return table<string, string>|nil
function M:model_sources(ref) end

--- Return all known models across registered providers.
---@return mare.Model[]
function M:models() end

--- Whether this frontend cannot open a browser (headless, SSH), selecting device login flows.
---@return boolean
function M:no_browser() end

--- Record a parameter adjustment note and emit `llm/params-adjusted`.
---@param model mare.Model
---@param note {from?: any, kind: string, param: string, to?: any, [string]: any}
---@return table
function M:note_param(model, note) end

--- Return a model's parameter definitions in order.
---@param model? mare.Model
---@return table<string, any>[]
function M:param_list(model) end

--- Return recent parameter adjustments (oldest first).
---@return {at_ms: number, from?: any, kind: string, model: string, param: string, to?: any, [string]: any}[]
function M:params_log() end

--- Prepare `model` (default current) via credential refresh/retry and check readiness; returns `ready, reason?`.
---@param model? mare.Model
---@param opts? {timeout_ms?: integer, cancel?: mare.Token?, [string]: any}
---@return boolean
---@return string?
function M:prepare(model, opts) end

--- Run every registered readiness step's `prepare` hook in order; raises on the first failure.
---@param args mare.ReadinessArgs
---@return table
function M:prepare_request(args) end

--- Look up a guarded provider accessor by name (projecting I/O, tracking side requests and usage, and invalidated on replace/unload), or nil.
---@param name string
---@return mare.Provider|nil
function M:provider(name) end

--- Return a provider's merged config (`mare.config.providers[name]`), or nil; may carry secrets.
---@param name string
---@return table<string, any>|nil
function M:provider_config(name) end

--- Convert a user `providers.<id>` config table into a validated `mare.Provider` declaration, or `nil, err`.
---@param provider_id string
---@param cfg table<string, any>
---@param opts? table<string, any>
---@return mare.Provider|nil
---@return string|nil
function M:provider_from_config(provider_id, cfg, opts) end

--- Return data descriptors for every registered provider (excluding functions).
---@return {configured: boolean, default_model?: string, label?: string, login: boolean, login_label?: string, name: string, no_key: boolean, [string]: any}[]
function M:providers() end

--- Return the raw unclamped effort setting, or nil.
---@return string|nil
function M:raw_effort() end

--- Register a request-readiness step (`prepare` / `ready`) ordered by `def.order`; returns a dispose function.
---@param name string
---@param def mare.ReadinessStep
---@return fun() dispose releases this registration
function M:readiness(name, def) end

--- Synchronously check readiness of `model` (default current); returns `ready, reason?` without sending requests or refreshing auth.
---@param model? mare.Model
---@return boolean
---@return string?
function M:ready(model) end

--- Return recently chosen model refs, most recent first.
---@return string[]
function M:recent() end

--- Refresh provider model lists merged with `models.dev` metadata.
---@param opts? {configured?: boolean, force?: boolean, min_age_ms?: integer, name?: string, on_open?: boolean, [string]: any}
---@return table<string, integer|string>
function M:refresh_catalog(opts) end

--- Refresh a stored credential under the auth lock with single-flight coordination per key.
---@param provider string
---@param key string
---@param refresh_fn fun(credential: table): table
---@param opts? {force?: boolean, cancel?: mare.Token?, refreshed?: (fun(previous: table, current: table): boolean)?, same_account?: (fun(previous: table, current: table): boolean)?, valid?: (fun(credential: table): boolean), [string]: any}
---@return table|nil
---@return any|nil
function M:refresh_credential(provider, key, refresh_fn, opts) end

--- Fetch a provider's model catalogue now.
---@param name string
---@param opts? {configured?: boolean, force?: boolean, [string]: any}
---@return integer|table<string, any>
function M:refresh_models(name, opts) end

--- Register an LLM provider (`provider`, `default_model?`, `models?`, `replace?`) and optional remote `stream` callback; released on disposer/release or disconnect.
---@param opts {default_model?: string, models?: ({cache_retention?: "none"|"short"|"long", context_window?: number, context_window_measured?: boolean, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: string, effort_map?: table<string, any>, efforts?: string[], hidden?: boolean, images?: boolean, input?: string[], max_output?: number, max_output_default?: boolean, model_id: string, name?: string, params?: table<string, any>, protocol?: string, reasoning?: boolean, route?: string, tool_update?: "addition_only"|false, truncation?: table<string, any>, unavailable?: string|boolean})[], provider: string, replace?: boolean, aliases?: table, api_key?: (string|table), api_key_env?: string, auth_keys?: string[], auth_mode?: (fun(): (string|nil)|string), base_url?: string, cache_retention?: string, catalog?: (fun(for_fetch?: boolean): table), catalog_scope?: (fun(): string), catalog_target?: (fun(): table), compact?: (fun(req: table): table|nil, string|nil), connect_timeout_ms?: number, default_context?: integer, default_max_output?: integer, default_reasoning?: boolean, dialect?: string, drop_fields?: string[], endpoint?: (fun(req: mare.LlmRequest): table), errors?: table, headers?: table, hosted_tools?: table, is_configured?: (fun(): boolean), key_prefixes?: string[], label?: string, list_models?: (fun(): table[]), login?: ({ methods: ("oauth"|"device"|"api_key")[], run: fun(lio?: table) }|fun(lio?: table)), login_hint?: string, login_io_fn?: (fun(lio?: table)), login_label?: string, model_defaults?: table, model_routes?: table<string, string>, models_dev?: (string|boolean), models_dev_routes?: boolean, no_key?: boolean, oauth_key?: string, oauth_label?: string, on_unauthorized?: (fun(err: table, info?: table): boolean|nil), prepare?: fun(model: mare.Model, opts: table), proxy?: string, query_params?: table, retries?: number, revoke?: fun(), route?: string, routes?: table<string, boolean>, stream?: (fun(req: mare.LlmRequest, sink: fun(ev: mare.StreamEvent)): mare.Message), stream_idle_timeout_ms?: number, tool_id_policy?: table, tool_update?: ("addition_only"|false), transport?: string, unavailable_for?: (fun(model: mare.Model): string|nil), warm_url?: (fun(): string), wire?: string, wire_api?: string}
---@return fun() dispose releases this registration
function M:register_provider(opts) end

--- Lua-only: register multiple LLM providers in one batch and emit `llm/providers-changed` once; valid entries are registered even if others fail (each invalid entry is reported in the second return value and as a plugin problem); returns a single disposer releasing all registered entries.
---@param entries table[]
---@return fun() dispose releases all registered providers
---@return {index: integer, provider?: string, error: string}[]|nil
function M:register_providers(entries) end

--- Register a spec-based wire protocol (`register_wire(spec)` or `register_wire(name, spec)`); dropped on dispose.
---@param spec_or_name mare.WireSpec|string
---@param spec? mare.WireSpec
---@return table
function M:register_wire(spec_or_name, spec) end

--- Persist a parameter value for model `ref` in `state.json` `model_params`.
---@param ref string
---@param name string
---@param value? any
---@return table
function M:remember_param(ref, name, value) end

--- Run every registered readiness step's `ready` hook in order; returns the first non-allow vote or nil.
---@param args mare.ReadinessArgs
---@return table<string, any>|nil
function M:request_ready(args) end

--- Fallback to the default effort when a session effort cannot be restored.
---@return string|table|nil
function M:reset_session_effort() end

--- Fallback to the default model when a session model cannot be restored.
---@return mare.Model|nil
function M:reset_session_model() end

--- Resolve a model reference string (`provider/id` or alias) to a `mare.Model`, or nil.
---@param ref string
---@return mare.Model|nil
function M:resolve(ref) end

--- Resolve every parameter's effective value, source layer, and notes for `model`.
---@param model? mare.Model
---@param opts? {only?: table<string, boolean>, request?: table<string, any>, [string]: any}
---@return {params: table<string, any>, sources: table<string, {layer: string, where?: string}>, wanted: table<string, any>, notes: table[], error?: string, route?: string}
function M:resolve_params(model, opts) end

--- Restore session model and/or effort from history without budget admission or persisting defaults; emits checked change events to preserve the poison barrier.
---@param opts? {effort?: string, model?: string}
---@return table
function M:restore_selection(opts) end

--- Return `(route, source, wire_route, error, auth)` for `model` (default current).
---@param model? mare.Model
---@return string|nil
---@return string|nil
---@return string|nil
---@return string|nil
---@return string|nil
function M:route_of(model) end

--- Return the effort clamped to `model` for `scope`.
---@param scope string
---@param model? (mare.Model|string)
---@return string|nil
function M:scoped_effort(scope, model) end

--- Set or clear the active effort scope name.
---@param scope? string|nil
---@param opts? {checked?: boolean, [string]: any}
---@return table
function M:set_active_scope(scope, opts) end

--- Set the reasoning effort of the current model. target session|global (default global) decides whether it is remembered; scope_name names an override scope; also_global updates the unscoped value as well.
---@param effort string
---@param opts? {also_global?: boolean, save?: boolean, scope_name?: string, target?: "session"|"global"}
---@return table
function M:set_effort(effort, opts) end

--- Select a model after admitting its context budget; target session keeps the default, global (the default) also saves it. A compaction error keeps the old selection.
---@param ref string
---@param opts? {save?: boolean, target?: "session"|"global", source_unknown?: boolean}
---@return {available: boolean, context_window?: integer, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: "apply_patch"|"replace", efforts: string[], images: boolean, max_output?: integer, model_id: string, name?: string, parameters: ({cycleable?: boolean, default?: any, name?: string, options: ({cost?: number|table, name?: string, tooltip?: string, value: any})[], parameter_id: string, statusline?: any, tooltip?: string, type?: string, values: any[], visible?: any})[], provider: string, ready: boolean, ready_reason?: string, reasoning: boolean, ref: string, route?: string, route_error?: string, tool_update?: "addition_only", unavailable_reason?: string, wire_route?: string}
function M:set_model(ref, opts) end

--- Set a parameter of the current model for this session; returns `used, err`.
---@param name string
---@param value? any
---@param opts? {save?: boolean, [string]: any}
---@return any|nil
---@return string|nil
function M:set_param(name, value, opts) end

--- Optional lifecycle extension: whether auxiliary LLM requests are still pending for `session_id`.
---@param session_id string
---@return boolean
function M:side_requests_pending(session_id) end

--- Format one credential status line per provider plus warnings for overridden sources.
---@return ({logged_in?: boolean, provider: string, state?: "registered"|"disabled"|"failed"|"unresolved", text: string, unavailable?: boolean, warning?: boolean, [string]: any})[]
function M:status_lines() end

--- Execute a streaming LLM request with retries, idle watchdog, and fallback handling, returning the final `mare.Message`.
---@param req mare.LlmRequest
---@param sink? fun(ev: mare.StreamEvent)
---@return mare.Message
function M:stream(req, sink) end

--- Build a classified error table from a provider's mid-stream error event.
---@param label string
---@param e {code?: any, message?: string, status?: any, type?: string, [string]: any}
---@param opts? {map?: fun(err: table, api_err: table)?, overflow?: (fun(msg: string, code?: string): boolean)?, [string]: any}
---@return table<string, any>
function M:stream_error(label, e, opts) end

--- Return the resolved parameter layer a sub-agent on `model_ref` starts with, or nil.
---@param model_ref? string
---@return table<string, any>|nil
function M:subagent_params(model_ref) end

--- Whether `compact_remote` is supported for `model` (default: current model).
---@param model? mare.Model
---@return boolean
function M:supports_remote_compact(model) end

--- Extract concatenated visible text content from a `mare.Message`.
---@param msg mare.Message
---@return string
function M:text_of(msg) end

--- Convert message history into the form `model` expects without mutating the source history.
---@param messages mare.Message[]
---@param model mare.Model
---@param tools? table<string, any>[]
---@param policy? {max?: integer, [string]: any}
---@return mare.Message[]
function M:transform_messages(messages, model, tools, policy) end

--- Look up registered wire `name`; returns `(stream_fn, owner_plugin)` or nil.
---@param name string
---@return (fun(endpoint: mare.WireEndpoint, req: mare.LlmRequest, sink: fun(ev: mare.StreamEvent)): mare.Message)|nil
---@return string|nil
function M:wire(name) end

--- Return the plugin name that owns or declares wire `name`, or nil.
---@param name string
---@return string|nil
function M:wire_owner(name) end
