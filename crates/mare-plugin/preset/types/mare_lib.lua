---@meta mare_lib
-- `require("mare_lib")`: small helpers the built-in plugins share (preset/plugins/lua/mare_lib.lua).
-- Any plugin may require it; it only wraps what every plugin already has through `mare`, so it
-- grants nothing a plugin couldn't write itself. Load it at run time
-- (inside `apply` or later).

---@class mare_lib
local M = {}

--- Strip at most one leading UTF-8 BOM (`EF BB BF`) from file text before parsing JSON.
---@generic T
---@param text T
---@return T
function M.strip_utf8_bom(text) end

---@param value any
---@return boolean
function M.positive_integer(value) end

---@param requested any
---@param model_limit any
---@return integer|nil
function M.output_limit(requested, model_limit) end

---@param left table[]
---@param right table[]
---@return boolean
function M.equal_messages(left, right) end

--- Track selected fields of a streaming record without copying each string fragment.
--- The selected values remain available through ordinary field reads/writes. Tracking is
--- local to this Lua module; use stream_materialize before passing a final record across
--- a service boundary or serializing it. Existing metatables are replaced.
---@generic T: table
---@param record T
---@param keys string[] selected string fields
---@return T
function M.stream_record(record, keys) end

--- Generate a deterministic call ID and return the sequence used after collision checks.
---@param seen table<string, any>
---@param seed string
---@param hex_len integer
---@param seq integer
---@return string id
---@return integer seq
function M.next_hashed_call_id(seen, seed, hex_len, seq) end

--- Append to a field previously selected by stream_record. String fragments accumulate in
--- the public byte buffer; other values preserve Lua concatenation/coercion errors.
--- With empty_if_nil, an absent initial field behaves as an empty string.
---@param record table
---@param key string
---@param delta any
---@param empty_if_nil? boolean
function M.stream_append(record, key, delta, empty_if_nil) end

--- Copy a tracked record to an ordinary table, including all selected fields. Nested values
--- remain shared. An untracked record is returned unchanged.
---@generic T: table
---@param record T
---@return T
function M.stream_materialize(record) end

--- Capture provider-specific tool/function declarations before request hooks. The returned
--- guard rejects changes to either the captured values or wire.body and restores the original
--- declaration objects after validating them, preserving schema property-order metadata.
---@param body table
---@return fun(wire: {body: table})
function M.guard_tool_declarations(body) end

--- Tool-call arguments as a table. A parse failure is never silently `{}`: the second value is
--- the error.
---@param raw string|nil JSON text
---@return table args
---@return string|nil err
function M.parse_args(raw) end

--- fn(resp)'s results; the response is closed (llm:close_response) on every exit and errors are
--- re-raised unchanged.
---@param llm mare.LlmService
---@param resp table
---@param fn fun(resp: table): ...
---@return ...
function M.with_response(llm, resp, fn) end

--- Raise a coded error value `{code, message, data?}` (mare.Error) for a service contract: a core
--- code or one the service declares (`<service>.<name>`). Prints as `code: message`.
---@param code string
---@param message string
---@param data? any JSON data the code's declaration describes
function M.fail(code, message, data) end

--- Publish `impl` (methods `function(impl, caller, ...)`) as the mare.Service `name` from `ctx`:
--- each method gets the traced caller ctx (`self.ctx`) as `caller`, so `caller:effect(...)` in a
--- method is an effect of the caller's fiber. Every method must be on `impl` when this runs.
--- Returns the published instance.
---@param ctx table the plugin's ctx
---@param name string
---@param impl table
---@return table
function M.service(ctx, name, impl) end

--- Whether `v` can be called: a function or a callable table (a traced service method is one).
---@param v any
---@return boolean
function M.callable(v) end

--- A stable identity of the plugin a caller ctx belongs to (an old ctx's id, a cordis fiber's
--- uid), for per-owner bookkeeping; nil for no plugin (init.lua, the host).
---@param caller table|userdata|nil
---@return integer|nil
function M.caller_id(caller) end

--- The plugin a caller ctx belongs to (an old-style ctx's `name`, a cordis ctx's fiber name), or nil.
---@param caller any
---@return string|nil
function M.caller_name(caller) end

--- The RPC connection id when `caller` runs on an RPC connection fiber, else nil.
---@param caller any
---@return any|nil
function M.rpc_caller_id(caller) end

--- Follow the optional service `name`: set up in `apply`; each
--- call reads the active provider (`ctx:get(name)`) and returns nil once the plugin itself is
--- unloaded.
---@param ctx table
---@param name string
---@return fun(): table|nil
function M.follow(ctx, name) end

--- Follow several optional services at once: a read-only table whose field `<name>` is the
--- service while it is up, nil otherwise. Read it on each call (`svc.session`).
---@param ctx table
---@param names string[]
---@return table<string, table|nil>
function M.followers(ctx, names) end

--- A registration owned by `caller`: `cleanup` runs when the caller's fiber ends or when the
--- returned release is called. An asynchronous cleanup returns its settlement handle.
---@param caller any
---@param cleanup fun(): any
---@param label? string
---@return fun(): any
function M.owned(caller, cleanup, label) end

---@class mare_lib.OwnerRegistry
---@field get fun(caller: any, create_fn?: fun(id: integer): any): any, integer look up (or lazily create) the caller's bucket and bind it to the caller's fiber
---@field close_all fun() close and clear every active owner bucket

--- Manage per-caller owner buckets tied to both the caller's fiber and the providing
--- service's own lifetime.
---@param ctx table
---@param close_fn fun(bucket: any)
---@param label string
---@return mare_lib.OwnerRegistry
function M.owner_registry(ctx, close_fn, label) end

---@class mare_lib.Ordered
---@field add fun(order: number, value: any): fun() add an entry; returns its (idempotent) removal
---@field first fun(fn: fun(value: any): any): any the first truthy fn(value) in order, or nil
---@field all fun(fn: fun(value: any): any): boolean runs fn on every entry; whether any was truthy

--- An ordered, removable list: ascending `order`, equal orders in the order added; traversals
--- run over a snapshot (the ordered chains, ui:handler and llm:readiness).
---@return mare_lib.Ordered
function M.ordered() end

--- `try again in 1.5s` / `in 20ms` / `in 2 seconds` in a rate-limit message → milliseconds.
---@param message any
---@return integer|nil
function M.retry_after_from_text(message) end

--- Remaining output tokens available in the context window when only `prompt + max_tokens`
--- exceeded it, or nil.
---@param msg any
---@return integer|nil
function M.parse_reply_left(msg) end

--- Recursive copy of a table (other values are returned as they are).
---@generic T
---@param t T
---@return T
function M.copy_deep(t) end

--- Yield to the scheduler every `stride` iterations when running in a yieldable coroutine.
---@param i integer
---@param stride? integer
function M.maybe_yield(i, stride) end

--- Memoize a zero-argument loader so a cold submodule or resource initializes once on first use.
--- Failed loads are not cached.
---@generic T
---@param loader fun(): T
---@return fun(): T
function M.lazy(loader) end

--- Bind a cold submodule's methods on `target` to lazy stubs while preserving outer wrappers
--- (such as `mare_lib.service` proxies or poison guards) installed afterward.
--- Failed loads restore saved wrappers, are not cached, and re-raise the error.
--- Returns `ensure, proxy`, where `ensure()` initializes once and returns `loader()`'s result,
--- and `proxy` forwards field reads to `ensure()[k]`.
---@generic T
---@param target table
---@param methods string[]
---@param loader fun(): T
---@param opts? {fast_unloaded?: table<string, fun(...): ...>, extra_target?: table, extra_fields?: string[]}
---@return fun(): T ensure
---@return T proxy
function M.lazy_group(target, methods, loader, opts) end

--- Compare JSON-shaped values without recursively expanding an entire history.
---@param left any
---@param right any
---@return boolean
function M.equal_values(left, right) end

--- Find a unique contiguous history inside the request messages.
---@param messages mare.Message[]
---@param history mare.Message[]
---@return integer|nil
function M.find_history(messages, history) end

--- Replace one history range while retaining messages on either side.
---@param messages mare.Message[]
---@param start integer
---@param count integer
---@param replacement mare.Message[]
---@return mare.Message[]
function M.replace_history(messages, start, count, replacement) end

--- `p` relative to the working directory when it lies below it, else `p` unchanged.
---@param p string
---@return string
function M.rel(p) end

--- One runtime choice from state.json (the whole table when `key` is nil), `default` when unset.
--- A missing, corrupt or unreadable file is logged through `ctx` (when given), never raised.
---@param ctx mare.Ctx|nil
---@param key? string
---@param default? any
---@return any
function M.load_state(ctx, key, default) end

--- Persist one runtime choice to state.json (not from a child agent). A failure is logged, and
--- with `notify` also shown in the UI.
---@param ctx mare.Ctx
---@param key string
---@param value any
---@param notify? boolean
function M.save_state(ctx, key, value, notify) end

--- Register a system prompt section for a plugin that may apply before `context` or doesn't
--- depend on it: `ctx:inject({ "context" }, ...)` — now if the `context` service is up, and
--- again for each new provider. Withdrawn with that provider and when the caller is disposed.
---@param ctx mare.Ctx
---@param def mare.PromptSection
function M.prompt_section(ctx, def) end

--- The steps' order in llm's readiness chain (`llm:readiness`, ascending): llm 10, mcp 20, context 30.
---@type {llm: integer, mcp: integer, context: integer}
M.READY_ORDER = {}

--- Policy-relevant subset of a tool definition or call info.
---@param src? table
---@return table
function M.tool_policy_info(src) end

--- Canonical call/metadata binding for a frozen policy evaluation (not an authorization token).
---@param call table
---@param info? table
---@return string
function M.policy_binding(call, info) end

--- Expand a `command.argv` placeholder template with `args` and optional `defaults`.
---@param tpl string[]
---@param defaults? table<string, string|number|boolean>
---@param args? table
---@return string[]|nil argv, string? err
function M.expand_argv_template(tpl, defaults, args) end

---@param inbox table
---@return boolean
function M.validate_inbox(inbox) end

--- Project `text` and optional `image_markers`, updating valid marker offsets to track projected text.
---@param text any
---@param image_markers? table[]
---@param array_copy? fun(value: any): any
---@return any, table[]?
function M.project_text_and_markers(text, image_markers, array_copy) end

--- Wrap outside content (tool/job/subagent/peer output, files) in a data boundary block
--- `<tag k="v">\nbody\n</tag>`: attribute values are XML-escaped (keys in sorted order) and every
--- closing form of `tag` inside `body` is escaped, so only the runtime can close the block.
---@param tag string
---@param attrs? table<string, string|number|boolean>
---@param body? string
---@return string
function M.data_block(tag, attrs, body) end

--- Re-project a queue `inbox` table and keep `image_markers` offsets aligned with projected text.
---@param inbox table
---@return table
function M.project_inbox(inbox) end

--- Truncate `s` on a UTF-8 boundary by lines/bytes (`max_lines`), code points (`max_chars`),
--- or bytes (`max_bytes`). Returns `(text, truncated)` (or `(s, false)` when no limit is set).
---@param s string
---@param opts? {max_lines?: integer, max_bytes?: integer, max_chars?: integer, keep?: "head"|"tail"|"middle", max_line_chars?: integer, suffix?: string, force_suffix?: boolean, floor_one_char?: boolean}
---@return string, boolean
function M.truncate_utf8(s, opts) end

--- Planned JSONL file path for a session `id` inside `dir`.
---@param dir string
---@param id string
---@return string
function M.session_file_path(dir, id) end

--- Sibling `.attachments` directory for a session JSONL `path`.
---@param path string
---@return string
function M.attachments_dir(path) end

--- Unique quarantine JSONL path under `<state_dir>/quarantine/`.
---@param state_dir string
---@param session_id? string
---@return string
function M.quarantine_path(state_dir, session_id) end

--- Decode a string previously escaped by quarantine UTF-8 sanitization.
---@param s string
---@return string
function M.unescape_quarantine_utf8(s) end

--- Append failed session `entries` as JSONL to `path`, anchored to `root_dir` (refusing symlinks).
---@param path string
---@param entries table[]
---@param reason? string
---@param root_dir? string
---@return string|nil path, string|nil err
function M.quarantine_records(path, entries, reason, root_dir) end

--- Append quarantine status to a persistence error message without duplicating it.
---@param err any
---@param quarantine_path? string|string[]
---@param quarantine_error? string
---@return string
function M.format_save_error(err, quarantine_path, quarantine_error) end

--- True when a dialect/endpoint targets OpenRouter.
---@param dialect string|nil
---@param url string|nil
---@return boolean
function M.is_openrouter(dialect, url) end

--- Provider-reported billed cost from OpenRouter `usage` accounting, or nil when incomplete.
---@param usage table
---@return number|nil
function M.openrouter_cost(usage) end

--- Compute one request's USD cost from provider-reported `usage.cost` or `model.cost` catalog rates.
---@param model mare.Model|nil
---@param usage table
---@param subscription? boolean
---@return number|nil
function M.usage_cost(model, usage, subscription) end

--- Derive a model's image capability (`true`, `false`, or `nil` when unknown).
---@param model mare.Model|table|nil
---@return boolean|nil
function M.model_images(model) end

--- Validate a cache_retention value ("none" | "short" | "long").
---@param val any
---@param source? string
---@return "none"|"short"|"long"|nil
function M.validate_cache_retention(val, source) end

--- Resolve a cache capability flag across model -> provider -> route -> plugin -> default.
---@generic T
---@param key string
---@param default_val T
---@param model? table
---@param prov_cfg? table
---@param route_cfg? table
---@param plugin_cfg? table
---@return T
function M.resolve_cache_cap(key, default_val, model, prov_cfg, route_cfg, plugin_cfg) end

--- Emit a caller-visible warning when a requested cache_retention tier cannot be guaranteed.
---@param ctx? table
---@param msg string
function M.warn_cache_limitation(ctx, msg) end

--- Resolve effective prompt cache retention ("none" | "short" | "long"), read_only flag,
--- supports_long flag, and resolution source across request, model, provider, route, plugin, and
--- global config layers.
---@param req? table
---@param model? table
---@param prov_cfg? table
---@param plugin_cfg? table
---@param route_cfg? table
---@param global_cfg? table
---@return "none"|"short"|"long" retention
---@return boolean read_only
---@return boolean supports_long
---@return string|nil source
function M.resolve_cache_retention(req, model, prov_cfg, plugin_cfg, route_cfg, global_cfg) end

--- Build an Anthropic cache_control marker table (or nil when caching is disabled / read-only).
---@param retention "none"|"short"|"long"
---@param supports_long boolean
---@param read_only boolean
---@return {type: "ephemeral", ttl?: "1h"}|nil
function M.anthropic_cache_control(retention, supports_long, read_only) end

--- True when the last assistant message's usage on `path`/`entries` is stale (before the last compaction/prune).
---@param path table|nil
---@param entries table|nil
---@param compacted? boolean
---@return boolean
function M.path_usage_stale(path, entries, compacted) end

--- Local token estimate of one message.
---@param m table
---@return integer
function M.message_tokens(m) end

--- Local token estimate of a message list (fallback context size when provider usage is stale).
---@param msgs table[]|nil
---@return integer
function M.estimate_messages(msgs) end

--- True when the last assistant message's usage in a loaded session is stale (before the last compaction/prune).
---@param session table|nil
---@param ev table|nil
---@return boolean
function M.session_usage_stale(session, ev) end

--- True when `path` is project-relative (not `~...`, `/...`, or a Windows drive `X:\`/`X:/`).
---@param path string
---@return boolean
function M.is_project_relative(path) end

--- Resolve `rel` inside `mare.project.root`, or nil when there is no resource root or when
--- `rel` targets a rejected `.mare` symlink.
---@param rel string
---@return string|nil
function M.project_file(rel) end

--- Resolve a directory option entry: project-relative entries resolve inside `mare.project.root`
--- (or nil), while `~` and absolute entries resolve via `mare.fs.resolve`.
---@param entry string
---@return string|nil
function M.resolve_project_dir(entry) end

---@type table<string, table>
M.APPLY_PATCH_MODEL_DEFAULTS = {}

---@param t any
---@return boolean
function M.is_json_array(t) end

--- True (plus element count) when `value` is a table whose keys are dense integers `1..n` (`0..n`).
---@param value any
---@return boolean
---@return integer
function M.is_dense_list(value) end

---@param t any
---@return boolean
function M.has_error_field(t) end

---@param raw string
---@param field string
---@param id_field string
---@param label string
---@return table[], table[], table
function M.decode_models_envelope(raw, field, id_field, label) end

--- Slice `items` by `{ cursor?, limit? }` with positive-integer validation.
---@generic T
---@param items T[]
---@param opts? {cursor?: string, limit?: integer}
---@return {data: T[], next_cursor?: string}
function M.paginate(items, opts) end

--- Normalize a custom message or entry renderer result (`nil`/`json.null` -> `nil`, strip nulls, validate node).
---@param res any
---@param opts? {wire?: boolean}
---@return mare.Node|nil
function M.normalize_render_result(res, opts) end

--- Resolve whether a wire should inject placeholder reasoning on assistant tool-call turns.
--- Precedence: user model > user provider override > models.dev > preset > fallback.
---@param model mare.Model|table|nil
---@param ep table|nil
---@param fallback? boolean
---@return boolean
function M.needs_reasoning_placeholder(model, ep, fallback, sources) end

--- Round a numeric timestamp into integer Unix epoch milliseconds (`unit == "s"` converts seconds).
---@param value any
---@param unit? "ms"|"s"
---@return integer|nil
function M.to_epoch_ms(value, unit) end

--- Normalize an error string or table into `{ code, message, data? }`.
---@param err any
---@param code? string
---@param data? any
---@return {code: string, message: string, data?: any}|nil
function M.structured_error(err, code, data) end

--- Extract the human-readable message from a structured `{ code, message }` error or plain string.
---@param err any
---@return string|nil
function M.error_message(err) end

--- Read every custom record of `kind` via paginated `session:list_records` (default branch = "all").
---@param session mare.SessionService|table|nil
---@param kind string
---@param opts? {branch?: "current"|"all", current?: boolean, session_id?: string, session_epoch?: string, loaded_only?: boolean}
---@return table[]|nil, string|nil
function M.all_records(session, kind, opts) end

---@class mare_lib.Guard
---@field disarm fun() cancel the pending `<close>` cleanup
---@field release fun() run the cleanup immediately (idempotent)

---@class mare_lib.Mutex
---@field acquire fun(self: mare_lib.Mutex): mare_lib.Guard acquire the mutex and return a `<close>` lease
---@field with fun(self: mare_lib.Mutex, fn: fun(...): any, ...): any run `fn(...)` under a `<close>` lease

--- Return a `<close>`-able guard that invokes `cleanup()` at most once when its scope exits,
--- including on errors and `mare.task.select` coroutine aborts.
---@param cleanup fun()
---@return mare_lib.Guard
function M.guard(cleanup) end

--- Create a coroutine-safe mutex whose lease is released via `<close>` so coroutine aborts
--- never drop the lock token.
---@return mare_lib.Mutex
function M.mutex() end

--- Single-window input token count for silent context-overflow checks (prefers per-iteration
--- window occupancy over cumulative billed totals).
---@param u table|nil
---@return number
function M.usage_window_input(u) end

--- Final-window output token count for silent length-overflow checks (prefers final-iteration
--- output over cumulative billed output).
---@param u table|nil
---@return number
function M.usage_window_output(u) end

--- Final-window context occupancy (`context_tokens`, or `context_input + context_output`,
--- falling back to total input + cache + output).
---@param u table|nil
---@return number
function M.usage_context_tokens(u) end

return M
