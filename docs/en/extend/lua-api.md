# Lua API

The public `mare.*`, `ui.*`, and `ctx.*` functions, taken from the LuaLS definitions that ship with mare. Run `mare init` to get completion and inline documentation for all of them in your editor.

> This page is generated from the code by `scripts/gen_docs.py`. Do not edit it by hand.

## ctx

Defined in `types/mare/events.gen.lua`, `types/mare/services/ctx.lua`.

| | |
|---|---|
| `ctx:on(name, listener, options)` |  |
| `ctx:inject(names, fn)` | A plugin's ctx (`apply(ctx, cfg)`): the cordis context (`effect`, `on`, `emit`, `inject({...}, fn)`, `get`, ...; `ctx.bus` for mare events, `ctx.policy` for decisions), with each service its definition injects as a field, and `after` / `every` / `spawn` from the `task` host plugin (`inject = { "task" }`). `inject({ names }, fn)` (Cordis): run `fn(ctx)` while the named optional services are up — a child fiber on the cordis tree, undone when one goes away or is replaced; the plugin itself does not restart. Use it for registrations into an optional service made at apply time. |
| `ctx:after(ms, fn)` | Run `fn` once after `ms`; cancelled when the plugin is disposed. Errors are shown in the UI. |
| `ctx:every(ms, fn)` | Run `fn` every `ms` until it returns false or the plugin is disposed. Errors are shown in the UI (the timer keeps going). |
| `ctx:spawn(fn, ...)` | Run `fn(...)` as a task of this plugin, cancelled when the plugin is disposed. |
| `ctx:get(name)` | The service `name` as this context reads it, or nil when it is not up (Cordis `get`: for an optional service the definition does not inject). |

## ctx.agent

Defined in `types/mare/services/agent.gen.lua`.

| | |
|---|---|
| `ctx.agent:activity()` | Synchronously capture detached agent activity state without yielding, queueing, or persisting partials. |
| `ctx.agent:adopt_session(snapshot)` | Atomically install a prepared session state snapshot without yielding or emitting events. |
| `ctx.agent:append(msg, opts)` | Append a message to the active conversation; returns `ok, err`. |
| `ctx.agent:baseline_cut()` | Non-yielding snapshot cut of agent runtime state and active turn items. |
| `ctx.agent:cancel(opts)` | Cancel or halt the active turn (optionally checking `turn_id` and/or clearing queued inputs) and return any removed queued items. |
| `ctx.agent:capabilities()` | Declare feature capabilities implemented by the active agent service. |
| `ctx.agent:check_session_revision(revision)` | Return false for a stale or foreign session revision token, synchronously and without mutation or events. |
| `ctx.agent:continue(opts)` | Resume an interrupted turn or run queued inputs (`steer` and `follow_up`). |
| `ctx.agent:delete_queued(input_id)` | Remove one queued input item by `input_id`. |
| `ctx.agent:enqueue(input, owner, opts)` | Durably enqueue an input item into the active session inbox without starting a turn, returning a receipt scoped to the active session owner. |
| `ctx.agent:get()` | Snapshot of current agent activity (`busy`, `turn_id`, `queued` count, `model`, `effort`, `activity`). |
| `ctx.agent:interrupted()` | True when the conversation context ends mid-turn and the loop can resume via `continue()`. |
| `ctx.agent:list_messages(opts)` | Current conversation messages visible to the model (optionally paginated by cursor and limit). |
| `ctx.agent:list_queue()` | Queued inputs in delivery order (steer items first, then follow-ups; image metadata only). |
| `ctx.agent:messages()` | Raw lossless conversation messages array for bounded in-process host inspection. |
| `ctx.agent:notify(text, opts)` | Queue a notification message for the model (joins the next step while busy, or wakes a new turn when `wake = true`). |
| `ctx.agent:pending_request()` | Return the interrupted request that a resume will retry, or nil. |
| `ctx.agent:persist_error()` | Return the current persistence fault error message, or nil when healthy. |
| `ctx.agent:prepare_commit(inbox, messages, turn_id)` | Prepare one atomic session-owned inbox/message publication; external readers use `list_queue()`. |
| `ctx.agent:prompt(input, opts)` | Submit user input to the agent (queued inputs enter follow_up by default, or steer via `agent.steer`); starts a turn when idle, or handles busy state via `when_busy` (`queue` default, or `reject`). |
| `ctx.agent:queue_custom(msg, mode, opts)` | Queue or append a custom message with delivery mode (`next_turn` \| `pending` \| `steer` \| `follow_up`). |
| `ctx.agent:register_activity(namespace, snapshot_fn)` | Register a namespaced synchronous JSON snapshot provider owned by the caller ctx; returns an unregister disposer. |
| `ctx.agent:resume_input(owner, opts)` | Un-park and start a turn for a durably queued input scoped to the active session owner. |
| `ctx.agent:session_revision()` | Return an opaque provider-owned revision token for the current session state, synchronously and without mutation. |
| `ctx.agent:set_messages(msgs, opts)` | Replace the active conversation message array (`notify = false` is synchronous and event-free for guarded publication). |
| `ctx.agent:set_pending_request(rec, opts)` | Record an interrupted request marker found on resume so it can be retried with the same request id. |
| `ctx.agent:set_queued(q)` | Restore queued inputs on session resume so they run as follow-ups. |
| `ctx.agent:set_session_id(session_id)` | Bind the active session id for durable queue persistence. |
| `ctx.agent:set_tool_states(states)` | Restore recorded tool call states on resume (`{ [tool_call_id] = {state, side_effect, idempotent} }`). |
| `ctx.agent:steer(input, opts)` | Inject steering input into the currently running turn. |
| `ctx.agent:step_participant(name, prepare, opts)` | Register a namespaced model-step participant whose prepared JSON state commits atomically before each model request. |
| `ctx.agent:step_preview(name, step_id)` | Return the frozen candidate state of the active step (`step_id`), or the committed state when omitted. |
| `ctx.agent:step_state(name)` | Return a detached JSON snapshot of the last committed step participant state, restored per session. |
| `ctx.agent:update_queued(input_id, opts)` | Update the content and/or lane of one queued input item. |
| `ctx.agent:wait(turn_id, opts)` | Block until the specified (or active) turn settles and return its `Turn` snapshot. |

## ctx.approvals

Defined in `types/mare/services/approvals.gen.lua`.

| | |
|---|---|
| `ctx.approvals:admit(cancel)` | Validate the selected reviewer within the configured review timeout. |
| `ctx.approvals:delegation_services()` | Services that delegation requires even when a child changes provider configuration. |
| `ctx.approvals:gate(category)` | Evaluate the independent policy gate for a category; returns a denial or nil. |
| `ctx.approvals:inheritance()` | All ancestor policy ceilings plus this session's live policy, copied for delegation. |
| `ctx.approvals:policy()` | Current effective approval policy. |
| `ctx.approvals:register_reviewer(name, review, opts)` | Register a named approval reviewer owned by the calling ctx; automatically withdrawn on dispose. |
| `ctx.approvals:request(category, action, policy, answer, cancel, opts)` | Apply the independent gate, selected reviewer, cancellation and audit lifecycle for an approval request. |
| `ctx.approvals:review_required()` | Whether every request requires review under current configuration or inherited flags. |
| `ctx.approvals:revision()` | Monotonic revision counter incremented whenever policy or reviewers change. |
| `ctx.approvals:set(value)` | Persist a session override before changing the effective approval policy. |
| `ctx.approvals:version()` | Reload-safe service incarnation plus policy/reviewer revision. |

## ctx.bus

Defined in `types/mare/host/bus.lua`.

| | |
|---|---|
| `ctx.bus:emit(event, ...)` | Notify every listener in order; a failing one is reported and the rest still run. |
| `ctx.bus:emit_checked(event, ...)` | Like `emit`, but the first failing listener (or one past its timeout_ms) raises. |
| `ctx.bus:bail(event, ...)` | The first listener's non-nil answer (`false` included), or nil. |
| `ctx.bus:waterfall(event, value, ...)` | `value` through the listeners in order (each may pass on a replacement); a failure keeps `value`. |
| `ctx.bus:waterfall_checked(event, value, ...)` | Like `waterfall`, but a failing listener raises instead of keeping `value`. |

## ctx.cells

Defined in `types/mare/services/cells.gen.lua`.

| | |
|---|---|
| `ctx.cells:cancel(cell_id)` | Cancel an active cell owned by this caller. |
| `ctx.cells:get()` | Status of the caller's cell environment. |
| `ctx.cells:list(opts)` | List cells owned by this caller. |
| `ctx.cells:list_tools(opts)` | Discover tools currently callable from code cells, including the MCP codemode tier. |
| `ctx.cells:reset()` | Cancel active cells owned by this caller and release caller-owned retained VM state. |
| `ctx.cells:start(code, opts)` | Start an asynchronous script cell in this caller's scope and wait up to yield_time_ms for initial output or completion. Lua callers may pass `cancel` / `wait_cancel` returning their cancel token. |
| `ctx.cells:wait(cell_id, opts)` | Wait for an active or completed cell owned by this caller, consuming newly emitted output. terminate cancels the cell and waits up to 5 s. Lua callers may pass `cancel` returning their cancel token. |

## ctx.changes

Defined in `types/mare/services/changes.gen.lua`.

| | |
|---|---|
| `ctx.changes:diffs()` | Each turn's before/after file contents and diff stats, oldest first. |
| `ctx.changes:rewind(turn, opts)` | Restore workspace files to their snapshot before `turn`. |
| `ctx.changes:turns()` | Recorded journal turns with modified files, oldest first. |

## ctx.commands

Defined in `types/mare/services/commands.gen.lua`.

| | |
|---|---|
| `ctx.commands:get(name)` | Current definition data of command `name` (or alias), or nil if absent. |
| `ctx.commands:list()` | Registered non-alias slash commands, sorted by name (data only, no callbacks). |
| `ctx.commands:register(name, opts)` | Register a slash command or alias (`alias_of`) owned by the caller; remote callers must pass `replace = true` to overwrite an existing name, and releasing a registration restores any previous definition. |
| `ctx.commands:run(line, input, opts)` | Run a slash command line (optionally targeting a loaded `session_id`); returns {result?} on success and throws a coded error on failure. |

## ctx.compact

Defined in `types/mare/services/compact.gen.lua`.

| | |
|---|---|
| `ctx.compact:busy()` | Whether compaction or a branch summary is currently running. |
| `ctx.compact:cancel()` | Stop a running compaction. |
| `ctx.compact:measure(opts)` | Current usage: the larger of the local estimate and provider-reported usage (or a per-source breakdown table when `{ by_source = true }`). |
| `ctx.compact:offload_images(opts)` | The offload stage on its own, for a provider's image_limit error. |
| `ctx.compact:prepare_request(request, history)` | Measure a prepared request and compact its uniquely mapped session history when needed. The budget uses the protocol-independent request and the output cap from its resolved parameter mappings; later `provider/request` changes to the output cap are not included. |
| `ctx.compact:register_stage(name, fn, opts)` | Add a stage or replace a built-in one; order comes from the `stages` option. Withdrawn when the caller is disposed or the returned disposer is called. |
| `ctx.compact:run(instructions, opts)` | Compact the session's context now (`reason`: manual by default); `instructions` adds a focus to the summary. Returns `{status, tokens_before, tokens_after}` (or `{status = "summarized", tokens_before = 0, tokens_after = 0, summary, dropped}` with `summarize_only`). A failure raises `compact.not_compacted`. |
| `ctx.compact:threshold(model)` | Tokens at which auto compaction triggers for a model (default: current); nil when `auto` is off or budgets are invalid. |

## ctx.context

Defined in `types/mare/services/context.gen.lua`.

| | |
|---|---|
| `ctx.context:expand_template(body, args)` | Fill a template body with shell-style split arguments ($1, $@, ${1:-default}...). |
| `ctx.context:files()` | The context files read into the prompt: {path, content}. |
| `ctx.context:invalidate()` | Drop the cached prompt; the next request assembles it again. |
| `ctx.context:preview(model, agent, opts)` | Preview the assembled prompt with candidate sections and section diagnostics without modifying the registry. |
| `ctx.context:replaced()` | Whether SYSTEM.md or the `system` option replaces the whole default prompt, else nil. |
| `ctx.context:report()` | Per-source /context report table. |
| `ctx.context:section(def)` | Register a prompt section (`name`, `order`, `text`, `when`, `scope`, ...); released with the caller or returned disposer. |
| `ctx.context:skill_problems()` | What the last skill scan warned about or skipped: {path, message, skipped?}. |
| `ctx.context:skills()` | The skills offered to the model: {name, description, path}. |
| `ctx.context:suppress(name)` | Hide a section by name in every scope until released; sections registered under that name later stay hidden too. |
| `ctx.context:suppress_runtime(opts)` | Hide non-required runtime sections (`runtime = true`, not `required = true`) in the matching layer until released. |
| `ctx.context:system_prompt(model, agent)` | The assembled prompt (cached per model and agent); raises when it can't be built. |
| `ctx.context:templates()` | Prompt templates, each registered as /name unless a command with that name exists. |
| `ctx.context:variable(name, fn)` | Register a `{{name}}` for interpolated sections (name: ^[a-z][a-z0-9_]*$) until released. |

## ctx.conversations

Defined in `types/mare/services/conversations.gen.lua`.

| | |
|---|---|
| `ctx.conversations:cancel()` | Cancel the active side conversation turn without closing it. |
| `ctx.conversations:close()` | Cancel and discard the active side conversation. |
| `ctx.conversations:info()` | Return the open side conversation snapshot, or nil when closed. |
| `ctx.conversations:start(opts)` | Start a side conversation from a committed snapshot of the current session. |
| `ctx.conversations:toggle()` | Toggle focus between the main conversation and an open side conversation. |

## ctx.copy

Defined in `types/mare/services/copy.gen.lua`.

| | |
|---|---|
| `ctx.copy:align_tables(md)` | Re-pad every GFM table in Markdown text so its columns line up. |
| `ctx.copy:blocks(md)` | Extract fenced code blocks from Markdown text, in order. |
| `ctx.copy:last(n)` | Copy the Nth-last response (default 1); opens a code-block picker when code blocks are present. |
| `ctx.copy:response(n)` | Markdown source of the Nth-last assistant response (1 = last), or nil. |
| `ctx.copy:text(text, target)` | Copy text to the clipboard, reporting the path used, and emit `copy/completed`. |
| `ctx.copy:write(text, lang)` | Write text to `mare-copy-<time>.<ext>` in the working directory and return the path, or nil on error. |

## ctx.fs

Defined in `types/mare/services/fs.gen.lua`.

| | |
|---|---|
| `ctx.fs:list(path)` | List directory entries filtered by the active sandbox read policy. |
| `ctx.fs:read(path, opts)` | Read a file as base64, bounded by active sandbox policy and optional max_bytes. |
| `ctx.fs:stat(path)` | Inspect file metadata within the active sandbox read policy. |
| `ctx.fs:watch(path)` | Watch a workspace path for policy-filtered filesystem changes (emits `fs/changed`; release via `registrations.release`). |
| `ctx.fs:write(path, data_base64, opts)` | Write base64 bytes to a file within writable sandbox roots and protected-path exclusions. |

## ctx.hooks

Defined in `types/mare/services/hooks.gen.lua`.

| | |
|---|---|
| `ctx.hooks:subscribe(event, opts)` | Subscribe `handle(event, mode, args)` to a bus event or policy decision point. Returns a registration removed on release or client disconnect. `prepend = true` inserts before earlier listeners (`prepend = false` keeps append order); `timeout_ms` requires `>= 1` and is clamped to the host policy timeout ceiling for remote callers. |

## ctx.images

Defined in `types/mare/services/images.gen.lua`.

| | |
|---|---|
| `ctx.images:show(content)` | Render image blocks from a message or tool-result content list when an image protocol is active. |

## ctx.jobs

Defined in `types/mare/services/jobs.gen.lua`.

| | |
|---|---|
| `ctx.jobs:background(job_id)` | Mark a job as backgrounded so completion emits a notification. |
| `ctx.jobs:cancel(job_id, reason, opts)` | Request a running job to stop (SIGTERM, then SIGKILL after kill_grace_ms). |
| `ctx.jobs:get(job_id, owner)` | Look up a tracked job by id (optionally scoped to `owner`), or nil. |
| `ctx.jobs:list(opts)` | Background jobs in start order, optionally filtered by owner (with optional cursor pagination). |
| `ctx.jobs:read(job_id, opts)` | Non-consumingly read job output starting at byte `offset`, with a trailing [status: ...] line. |
| `ctx.jobs:start(opts)` | Start and track a background process job. |
| `ctx.jobs:track(opts)` | Track an external handle implementing the `mare.Job` interface (sub-agents, terminals) as a job. |
| `ctx.jobs:write(job_id, data, opts)` | Write stdin data to a running job (optionally closing stdin with `eof = true`). |

## ctx.kernel

Defined in `types/mare/services/kernel.gen.lua`.

| | |
|---|---|
| `ctx.kernel:begin_session_switch(opts)` | begin an atomic session identity switch: reserve target `session_id` (if given) and revoke session-only trust before publish; `handle:commit(session_id)` automatically marks any active session export call as committed |
| `ctx.kernel:get_caller()` | return the opaque caller identifier of the running request |
| `ctx.kernel:is_builtin(name)` | whether plugin `name` is a built-in |
| `ctx.kernel:list_sessions()` | list currently open sessions across managed runtimes (or this standalone host) |
| `ctx.kernel:mark_call_committed()` | mark the running service export call as past its commit boundary |
| `ctx.kernel:plugin_row(name)` | plugin `name`'s row in the plugin list, 0 if it has none |
| `ctx.kernel:plugins()` | the plugin rows: name, origin, active, overrides |
| `ctx.kernel:problems()` | the current problem lines |
| `ctx.kernel:provides(name)` | whether the caller's fiber provides `name` |
| `ctx.kernel:reload(name)` | reload plugin `name`, or all plugins |
| `ctx.kernel:reload_barrier()` | wait until no reload is running |
| `ctx.kernel:reload_generation()` | the running (or latest) reload's generation: one more per plugin reload and per config row update |
| `ctx.kernel:request_lease(cancel)` | acquire the exclusive per-host request/reload lease bound to the caller owner lifetime |
| `ctx.kernel:session_capacity()` | whether this host orchestrates a single session runtime ("single") or multiple concurrent runtimes ("multi") |
| `ctx.kernel:session_epoch()` | the current event stream session epoch |
| `ctx.kernel:stalled()` | the plugins stuck loading, unloading or waiting for `stall_ms` or longer; observed, never aborted |

## ctx.llm

Defined in `types/mare/services/llm.gen.lua`.

| | |
|---|---|
| `ctx.llm:active_credential(provider, env_names)` | Return the winning credential descriptor for `provider` (`init.lua` &gt; `auth.json` &gt; env), or nil. |
| `ctx.llm:active_scope()` | Return the currently active effort scope name, or nil. |
| `ctx.llm:alias_target(name)` | Return what a model alias points to, or nil. |
| `ctx.llm:api_key(provider, env_names)` | Resolve the API key string for `provider` from config, `auth.json`, or `env_names`, or nil. |
| `ctx.llm:api_key_source(name, envs)` | Describe where a provider's API key would come from (never returns the secret), or nil. |
| `ctx.llm:apply_params(req, wire, opts)` | Write resolved request parameters into `wire.body` and `wire.headers`. |
| `ctx.llm:auth_get(key)` | Read a stored credential record from `auth.json` (`version` is reserved). |
| `ctx.llm:auth_keys(prefix)` | Stored credential key names in `auth.json`, optionally filtered by literal prefix; never returns secret values. |
| `ctx.llm:auth_set(key, value, opts)` | Atomically write or delete a credential record in `auth.json`; returns whether committed. |
| `ctx.llm:auth_status(provider)` | Return JSON-safe masked credential and directory status for all providers (or one provider). |
| `ctx.llm:base_url(provider, default)` | Return the resolved base URL for `provider` (falling back to `default`), or nil. |
| `ctx.llm:cancel_side_requests(session_id)` | Optional lifecycle extension: cancel auxiliary LLM requests belonging to `session_id`. |
| `ctx.llm:capabilities(ref)` | Return a detached JSON-safe capability descriptor for `ref` (default current model), or `nil, err`. |
| `ctx.llm:clamp_effort(model, effort)` | Clamp `effort` to the levels supported by `model`, or return nil. |
| `ctx.llm:classify(err, cancelled)` | Classify a provider error table or message into `(kind, retryable)`. |
| `ctx.llm:clear_scope(scope, model, opts)` | Clear a scoped effort override (`save = false` hides it for this session only). |
| `ctx.llm:close_response(resp)` | Close a provider HTTP response immediately to release its connection. |
| `ctx.llm:compact_remote(opts)` | Run provider-side compaction; returns `{items, model_id, provider_id}` or `nil, err, kind`. |
| `ctx.llm:complete(req)` | Non-streaming convenience wrapper around `stream`; returns the final assistant `mare.Message`. |
| `ctx.llm:constrain_request(req)` | Apply all registered tool declaration constraints to `req` and return the constrained request copy. |
| `ctx.llm:constrain_tools(fn)` | Register a monotone tool declaration constraint applied once per model request; returns a remove disposer. |
| `ctx.llm:credential_sources(provider)` | Return all credential sources present for `provider`, active first (never returns secrets). |
| `ctx.llm:current()` | Return the currently selected `mare.Model`, or nil. |
| `ctx.llm:cycle_param(name, dir)` | Advance a `cycleable` parameter to the next offered option (`dir` +1 / -1); returns `value, err`. |
| `ctx.llm:define_params(rule, layer)` | Register model parameter definitions and wire mappings owned by the calling plugin. |
| `ctx.llm:describe_wire(name)` | Return a copy of the registered wire's `mare.WireDescribe`, or nil. |
| `ctx.llm:effort()` | Return the effective effort clamped to the current model (including active_scope override if set). |
| `ctx.llm:effort_options(model)` | Return every effort value defined for `model` (built-in levels plus rule additions), or nil. |
| `ctx.llm:efforts()` | Return the built-in effort levels, lowest first ("off" ... "max"). |
| `ctx.llm:explain_lines(ref)` | Format human-readable explanation lines for the effective parameters of `ref`. |
| `ctx.llm:explain_params(ref)` | Return the effective parameters, source layers, wire fields, and options for `ref`. |
| `ctx.llm:fingerprint(req)` | Compute the crash-guard fingerprint key of a model request. |
| `ctx.llm:get_model(ref)` | The session's current model, or with ref any model the providers resolve (hidden ones and ids a provider serves without listing them included); never changes the selection. |
| `ctx.llm:get_selection()` | Return the mutable session selection snapshot (`model`, `default_model`, `recent`, `params`) for resume comparison. |
| `ctx.llm:global_effort(model)` | Return the global unscoped effort clamped to `model`, ignoring `active_scope`. |
| `ctx.llm:has_api_key(provider, env_names)` | Whether an API key is available for `provider` without returning the secret. |
| `ctx.llm:has_auth(key)` | Whether `auth.json` holds a credential under `key` without returning the secret. |
| `ctx.llm:has_credentials()` | Whether at least one provider has usable credentials (or the current model requires none). |
| `ctx.llm:http_error(label, resp, opts)` | Build a classified provider error table from a non-2xx HTTP response. |
| `ctx.llm:identify_key_provider(key)` | Identify a registered login-capable provider from a pasted API key using declared `key_prefixes`, or nil when unmatched or ambiguous. |
| `ctx.llm:invalidate_api_key(provider)` | Drop the cached API key for a key-helper provider so the next call re-runs the command. |
| `ctx.llm:invalidate_catalog(name)` | Invalidate in-memory catalog models for `name` (or all providers when omitted). |
| `ctx.llm:list_auth_keys(prefix)` | Stored credential key names in `auth.json`, optionally filtered by literal prefix; never returns secret values. |
| `ctx.llm:list_models()` | Every visible model with its abilities, sorted by ref. |
| `ctx.llm:list_providers()` | Providers and their masked login state; never a secret value. |
| `ctx.llm:logged_in()` | List providers that currently have credentials stored in `auth.json`. |
| `ctx.llm:login(provider, opts)` | Run a provider's login. Without key the flow is interactive and needs a connected UI; Lua callers may pass their own say/ask/choose functions. |
| `ctx.llm:login_api_key(name, title, lio)` | Run an interactive API-key login flow for provider `name`. |
| `ctx.llm:login_io(lio)` | Complete a login I/O table with the connected UI's `say`, `ask`, and `choose` callbacks. |
| `ctx.llm:logout(provider)` | Remove every stored credential of a provider (revoked first where possible); kept lists the environment and config sources that cannot be removed here. |
| `ctx.llm:model_default(model, field)` | Return the default value for `field` declared via the provider's `model_defaults` globs. |
| `ctx.llm:model_sources(ref)` | Return per-field model metadata provenance (`user`, `measured`, `server`, `static`, `models_dev`, `default`), or nil. |
| `ctx.llm:models()` | Return all known models across registered providers. |
| `ctx.llm:no_browser()` | Whether this frontend cannot open a browser (headless, SSH), selecting device login flows. |
| `ctx.llm:note_param(model, note)` | Record a parameter adjustment note and emit `llm/params-adjusted`. |
| `ctx.llm:param_list(model)` | Return a model's parameter definitions in order. |
| `ctx.llm:params_log()` | Return recent parameter adjustments (oldest first). |
| `ctx.llm:prepare(model, opts)` | Prepare `model` (default current) via credential refresh/retry and check readiness; returns `ready, reason?`. |
| `ctx.llm:prepare_request(args)` | Run every registered readiness step's `prepare` hook in order; raises on the first failure. |
| `ctx.llm:provider(name)` | Look up a guarded provider accessor by name (projecting I/O, tracking side requests and usage, and invalidated on replace/unload), or nil. |
| `ctx.llm:provider_config(name)` | Return a provider's merged config (`mare.config.providers[name]`), or nil; may carry secrets. |
| `ctx.llm:provider_from_config(provider_id, cfg, opts)` | Convert a user `providers.<id>` config table into a validated `mare.Provider` declaration, or `nil, err`. |
| `ctx.llm:providers()` | Return data descriptors for every registered provider (excluding functions). |
| `ctx.llm:raw_effort()` | Return the raw unclamped effort setting, or nil. |
| `ctx.llm:readiness(name, def)` | Register a request-readiness step (`prepare` / `ready`) ordered by `def.order`; returns a dispose function. |
| `ctx.llm:ready(model)` | Synchronously check readiness of `model` (default current); returns `ready, reason?` without sending requests or refreshing auth. |
| `ctx.llm:recent()` | Return recently chosen model refs, most recent first. |
| `ctx.llm:refresh_catalog(opts)` | Refresh provider model lists merged with `models.dev` metadata. |
| `ctx.llm:refresh_credential(provider, key, refresh_fn, opts)` | Refresh a stored credential under the auth lock with single-flight coordination per key. |
| `ctx.llm:refresh_models(name, opts)` | Fetch a provider's model catalogue now. |
| `ctx.llm:register_provider(opts)` | Register an LLM provider (`provider`, `default_model?`, `models?`, `replace?`) and optional remote `stream` callback; released on disposer/release or disconnect. |
| `ctx.llm:register_providers(entries)` | Lua-only: register multiple LLM providers in one batch and emit `llm/providers-changed` once; valid entries are registered even if others fail (each invalid entry is reported in the second return value and as a plugin problem); returns a single disposer releasing all registered entries. |
| `ctx.llm:register_wire(spec_or_name, spec)` | Register a spec-based wire protocol (`register_wire(spec)` or `register_wire(name, spec)`); dropped on dispose. |
| `ctx.llm:remember_param(ref, name, value)` | Persist a parameter value for model `ref` in `state.json` `model_params`. |
| `ctx.llm:request_ready(args)` | Run every registered readiness step's `ready` hook in order; returns the first non-allow vote or nil. |
| `ctx.llm:reset_session_effort()` | Fallback to the default effort when a session effort cannot be restored. |
| `ctx.llm:reset_session_model()` | Fallback to the default model when a session model cannot be restored. |
| `ctx.llm:resolve(ref)` | Resolve a model reference string (`provider/id` or alias) to a `mare.Model`, or nil. |
| `ctx.llm:resolve_params(model, opts)` | Resolve every parameter's effective value, source layer, and notes for `model`. |
| `ctx.llm:restore_selection(opts)` | Restore session model and/or effort from history without budget admission or persisting defaults; emits checked change events to preserve the poison barrier. |
| `ctx.llm:route_of(model)` | Return `(route, source, wire_route, error, auth)` for `model` (default current). |
| `ctx.llm:scoped_effort(scope, model)` | Return the effort clamped to `model` for `scope`. |
| `ctx.llm:set_active_scope(scope, opts)` | Set or clear the active effort scope name. |
| `ctx.llm:set_effort(effort, opts)` | Set the reasoning effort of the current model. target session\|global (default global) decides whether it is remembered; scope_name names an override scope; also_global updates the unscoped value as well. |
| `ctx.llm:set_model(ref, opts)` | Select a model after admitting its context budget; target session keeps the default, global (the default) also saves it. A compaction error keeps the old selection. |
| `ctx.llm:set_param(name, value, opts)` | Set a parameter of the current model for this session; returns `used, err`. |
| `ctx.llm:side_requests_pending(session_id)` | Optional lifecycle extension: whether auxiliary LLM requests are still pending for `session_id`. |
| `ctx.llm:status_lines()` | Format one credential status line per provider plus warnings for overridden sources. |
| `ctx.llm:stream(req, sink)` | Execute a streaming LLM request with retries, idle watchdog, and fallback handling, returning the final `mare.Message`. |
| `ctx.llm:stream_error(label, e, opts)` | Build a classified error table from a provider's mid-stream error event. |
| `ctx.llm:subagent_params(model_ref)` | Return the resolved parameter layer a sub-agent on `model_ref` starts with, or nil. |
| `ctx.llm:supports_remote_compact(model)` | Whether `compact_remote` is supported for `model` (default: current model). |
| `ctx.llm:text_of(msg)` | Extract concatenated visible text content from a `mare.Message`. |
| `ctx.llm:transform_messages(messages, model, tools, policy)` | Convert message history into the form `model` expects without mutating the source history. |
| `ctx.llm:wire(name)` | Look up registered wire `name`; returns `(stream_fn, owner_plugin)` or nil. |
| `ctx.llm:wire_owner(name)` | Return the plugin name that owns or declares wire `name`, or nil. |

## ctx.mcp

Defined in `types/mare/services/mcp.gen.lua`.

| | |
|---|---|
| `ctx.mcp:catalog(tier)` | Prepare/discover one exposure tier using lazy connection and tool registration (default `codemode`). |
| `ctx.mcp:connect(server)` | Connect one configured MCP server and return its status. |
| `ctx.mcp:get_prompt(server, name, arguments)` | Expand a prompt on a configured MCP server. |
| `ctx.mcp:list_prompts(opts)` | Prompts exposed by connected MCP servers (or one named server). |
| `ctx.mcp:list_resources(opts)` | Resources and resource templates exposed by connected MCP servers (or one named server). |
| `ctx.mcp:list_servers()` | Configured MCP servers and their current connection status (never connects). |
| `ctx.mcp:list_tools(opts)` | Tools discovered from configured MCP servers (or one named server). |
| `ctx.mcp:read_resource(server, uri)` | Read one MCP resource URI from a server. |
| `ctx.mcp:reconnect(server)` | Reconnect one configured MCP server and return its status. |

## ctx.message

Defined in `types/mare/services/message.gen.lua`.

| | |
|---|---|
| `ctx.message:append(opts)` | Append a custom message that enters model context while honoring display, delivery, and optional `request_id` idempotency. |
| `ctx.message:get_type(custom_type)` | Look up the normalized active spec for a registered custom message type (Lua-only). |
| `ctx.message:list_types()` | List currently registered custom message types in sorted name order. |
| `ctx.message:register_type(type, opts)` | Register a custom message type with optional validation and render callbacks; returns a disposer (`registration_id` over RPC). |

## ctx.options

Defined in `types/mare/services/options.gen.lua`.

| | |
|---|---|
| `ctx.options:get(keys, opts)` | Read effective options (or descriptors when opts.describe is true) with live service overrides and shared sensitive-path redaction. |
| `ctx.options:set(values, opts)` | Validate and transactionally commit a batch of option updates (core options, llm model/effort, and plugins.&lt;plugin&gt;.&lt;option&gt;) with full rollback on refusal. |

## ctx.permissions

Defined in `types/mare/services/permissions.gen.lua`.

| | |
|---|---|
| `ctx.permissions:add_rules(rules)` | Add a rule source owned by the calling plugin (removed when the caller is disposed). |
| `ctx.permissions:check(call, info)` | Evaluate the vote the permissions plugin casts for a tool call (for UIs and tests). |
| `ctx.permissions:commit_request(call)` | Recheck and commit a previously resolved structured permission request immediately before execution. |
| `ctx.permissions:describe(name)` | Return the display metadata of a built-in or configured preset, or nil if unknown. |
| `ctx.permissions:describe_request(call, expected)` | Describe a frozen call's structured permission request without granting it. |
| `ctx.permissions:get()` | Current active permission preset and policy revision for this session (`rules` and `grants` are included for primary/in-process callers and omitted for peer callers). |
| `ctx.permissions:grants()` | Detached snapshot of this turn's and session's granted permission profiles. |
| `ctx.permissions:is_partial()` | Whether the active sandbox backend cannot enforce network restrictions requested by the preset. |
| `ctx.permissions:list()` | Available permission presets (built-in and configured) with display metadata and active indicator. |
| `ctx.permissions:policy_revision()` | Monotonic revision counter incremented whenever permission policy or rules change. |
| `ctx.permissions:preset()` | Current sandbox preset ("off" when the host policy is unavailable). |
| `ctx.permissions:presets()` | List available permission preset names. |
| `ctx.permissions:request(profile, opts)` | Request structured permissions through the ordinary tool policy, approval, and execution chain. |
| `ctx.permissions:resolve_request(call, constraints, scope, expected)` | Intersect approval constraints with a frozen permission request and current policy. |
| `ctx.permissions:rule_verdict(call, info)` | What configured and remembered rules alone decide about a call (excluding session-only rules), or nil. |
| `ctx.permissions:rules()` | Effective permission rules: `init.lua` rules, persisted rules, and session rules. |
| `ctx.permissions:set(name, opts)` | Switch the active permission preset (`confirm` asks before the first switch to full access); returns the updated permission state or raises a coded error. |

## ctx.plugins

Defined in `types/mare/services/plugins.gen.lua`.

| | |
|---|---|
| `ctx.plugins:check(names)` | Fetch and compare plugin revisions without moving checkouts or modifying locks. |
| `ctx.plugins:choose_plugins()` | Offer the optional plugins, install selected packages, and return the outcome; nil when the offer is not due. |
| `ctx.plugins:install(name, opts)` | Install a catalog plugin by short name or sync a declared source; raises `plugins.failed` on error. |
| `ctx.plugins:list(kind)` | Declared/installed packages, parsed `mare-plugin.toml` manifests, and resource directories. |
| `ctx.plugins:news()` | Show NEWS.md sections for mare / enabled plugins whose version changed since last seen. |
| `ctx.plugins:uninstall(name, opts)` | Uninstall a catalog plugin by short name or clean an unused source; raises `plugins.failed` on error. |
| `ctx.plugins:update(name, opts)` | Update declared git packages to their newest allowed revision (`update`); raises `plugins.failed` on error. |

## ctx.policy

Defined in `types/mare/host/policy.lua`.

| | |
|---|---|
| `ctx.policy:voter(point, fn, opts)` | Register a voter for decision point `point`; an effect of the caller's fiber (it goes with it). The voter gets the decision's arguments (frozen) and returns nil (no objection), "allow", "deny", "ask" or a vote table. |
| `ctx.policy:rewriter(point, fn, opts)` | Register a rewriter for rewrite point `point` (`tool/rewrite`): it gets a copy of the call with the arguments so far and returns nil (keep) or a table of new arguments. |
| `ctx.policy:decide(point, args, options)` | Decide `point` over `args`: the voters in order, the first deny stops, an error, timeout or invalid vote is a deny. Returns the folded decision `{decision, reason?, reasons, grant?, ...}`. |
| `ctx.policy:rewrite(point, call)` | Rewrite `call` through `point`'s rewriters: returns the rewritten `arguments` table directly, or raises a structured `mare.Error` (`policy.rewrite_refused`, with `message` set to the failure reason) when any rewriter refuses, times out, or returns a non-table value. |
| `ctx.policy:epoch()` | The policy epoch: it moves when what a decision would answer may change (an approval's fingerprint). |
| `ctx.policy:has_voters(point)` | Whether any voter (rewriter) takes part in `point` now. |

## ctx.proc

Defined in `types/mare/services/proc.gen.lua`.

| | |
|---|---|
| `ctx.proc:kill(opts)` | Terminate an owned running process and its process group. |
| `ctx.proc:resize(opts)` | Resize the PTY of an owned process started with tty = true. |
| `ctx.proc:spawn(opts)` | Start a sandboxed piped or PTY child process bound to the caller lifecycle. |
| `ctx.proc:write(opts)` | Write base64 bytes to stdin or PTY of an owned process, optionally closing piped stdin. |

## ctx.query

Defined in `types/mare/services/query.gen.lua`.

| | |
|---|---|
| `ctx.query:call(operation, arguments, opts)` | Run an in-process query operation inside an ephemeral-read-only sandbox rooted at the active execution context's cwd, or at `mare.cwd` when called directly from Lua without an active `mare.exec.current()`. Lua callers may pass `cancel`, a function returning their cancel token or nil. |
| `ctx.query:list_providers()` | Registered query providers and the operations they serve, sorted by name. |
| `ctx.query:register(name, def)` | Register capability operations owned by this caller; removed automatically on disposal. Paginated results return next_cursor; subsequent requests receive cursor. |
| `ctx.query:supports(operation)` | Whether a registered in-process provider serves this operation. |

## ctx.review

Defined in `types/mare/services/review.gen.lua`.

| | |
|---|---|
| `ctx.review:run(target)` | Run an isolated read-only code review for `target` and return findings or `nil, err`. |
| `ctx.review:viewer(only)` | Open the interactive change diff viewer, optionally narrowed to `only`. |

## ctx.session

Defined in `types/mare/services/session.gen.lua`.

| | |
|---|---|
| `ctx.session:add_record(kind, data, opts)` | Append a custom plugin record outside the model message tree. |
| `ctx.session:add_workspace_dirs(dirs)` | Atomically validate, persist, and publish additional workspace directories. |
| `ctx.session:append_compaction(summary, first_kept_msg, tokens_before, details, extra)` | Record a compaction entry and return the updated message context or `nil, err`. |
| `ctx.session:append_prune(map, meta)` | Record a message-content prune entry and return the updated message context or `nil, err`. |
| `ctx.session:archive(session_id)` | Archive a saved inactive session (and its child agent sessions) by id, unique prefix, or path. |
| `ctx.session:available()` | Whether the session store is usable now (`false, reason` under `--no-session`, in a child agent, or during a persistence fault). |
| `ctx.session:baseline_cut()` | Non-yielding snapshot cut of in-memory session entries. |
| `ctx.session:child(opts)` | Open a sub-session transcript handle under `<session dir>/agents/<run_id>/` owned by the caller. |
| `ctx.session:children(run_id)` | List sub-session transcript files, newest run first. |
| `ctx.session:claim_operation(identity, cancel)` | Claim or replay a generic idempotent operation; concurrent duplicates wait on the same writer. |
| `ctx.session:close(session_id, opts)` | Unload the active session runtime and release its lock without exiting the host. |
| `ctx.session:commit(batch, opts)` | Atomically append accepted user messages and the remaining identified inbox. |
| `ctx.session:commit_step(record, messages, opts)` | Atomically commit one model step's `agent/step-state` record and validated context disclosure messages. |
| `ctx.session:create(opts)` | Create a new idle session runtime (optionally named, in cwd, or idempotent by request_id). |
| `ctx.session:delete(session_id)` | Permanently delete a saved inactive session (and its child agent sessions) by id, unique prefix, or path. |
| `ctx.session:entries(opts)` | Return session journal entries (`expand = true` expands compacted/pruned views). |
| `ctx.session:entry_id(msg)` | Return the session entry id of a message in the current context, or nil when unknown. |
| `ctx.session:file_changes()` | Return files edited by tools in this session (`old`/`new` content or `false` when absent). |
| `ctx.session:fork(opts)` | Create a new session forked from an active or saved session (optionally before an item). |
| `ctx.session:get(session_id)` | Fetch one session snapshot by id, unique prefix, or path. |
| `ctx.session:get_item(opts)` | Read one semantic item by session_id and item_id, inline or UTF-8 chunked, with optional neighbors. |
| `ctx.session:info()` | Return live session identity, path, leaf entry id, message count, and status flags. |
| `ctx.session:label_item(label, opts)` | Set or clear a label on a session entry (defaults to the current leaf). |
| `ctx.session:leaf()` | Return the id of the current session leaf entry, or nil before the first message. |
| `ctx.session:list(opts)` | List saved sessions in the index, newest first. |
| `ctx.session:list_items(opts)` | List semantic items of the open session, optionally filtered by branch, item type, or order. |
| `ctx.session:list_records(kind, opts)` | List custom plugin records of the given kind in the open session. |
| `ctx.session:live_snapshot()` | Return a detached live snapshot of the active session for step-bounded history queries, or nil. |
| `ctx.session:load(opts)` | Load an existing session by session_id, unique prefix, name, or (for primary) path. |
| `ctx.session:load_transcript(path, title, opts)` | Open a session transcript file in the live transcript viewer. |
| `ctx.session:on_drain(fn)` | Register a pre-switch session resource drain callback owned by the caller; returns a release disposer. |
| `ctx.session:operation_receipt(identity)` | Read-only lookup of a committed generic operation receipt without claiming or mutating state. |
| `ctx.session:path_to(leaf_entry_id)` | Return the branch path of entries leading to `leaf_entry_id` (default: current leaf). |
| `ctx.session:persist_message(opts)` | Persist one conversation message synchronously to the session journal. |
| `ctx.session:prepare_children()` | Ensure the parent session header exists before recording sub-agent transcripts. |
| `ctx.session:prepare_fork()` | Drain side requests, flush the active journal, and return `{ path, images? }` for a multi-runtime fork. |
| `ctx.session:record_request(opts)` | Record an agent/request marker synchronously. |
| `ctx.session:record_request_aborted(opts)` | Record an agent/request-aborted marker synchronously. |
| `ctx.session:record_request_end(opts)` | Record an agent/request-end marker synchronously. |
| `ctx.session:record_run_end(opts)` | Record an agent/run-end fork boundary marker durably. |
| `ctx.session:record_tool_state(opts)` | Record one tool execution phase marker synchronously. |
| `ctx.session:release_operation(claim, reason)` | Release an uncommitted operation claim and wake duplicate waiters without changing durable state. |
| `ctx.session:rewind(opts)` | Move the active session leaf back before an item (optionally summarizing or restoring files). |
| `ctx.session:search(opts)` | Full-text and metadata search across persisted and live sessions in the active workspace. |
| `ctx.session:search_items(opts)` | Search matching semantic items inside one session under workspace policy. |
| `ctx.session:set_default_sandbox_mode(spec)` | Retain the configured default sandbox policy for new or unrecorded resumed sessions without changing the current policy. |
| `ctx.session:set_leaf(entry_id)` | Move the session leaf to `entry_id` (`nil` = before the first message) and return the new context or `nil, err`. |
| `ctx.session:set_sandbox_mode(spec)` | Switch this session's sandbox policy, log it in the session, and emit `sandbox/changed`. |
| `ctx.session:short_id(session_id)` | Return the shortest unique prefix (8+ characters) of a session id in the index. |
| `ctx.session:snapshot(opts)` | Create a persisted fork snapshot of an active or saved session without changing the active session target or cancelling running turns. |
| `ctx.session:snapshot_context(opts)` | Build a normalized SES-FORK context snapshot without switching or modifying the source session. |
| `ctx.session:switch(opts)` | Switch the active session in-place on a single-runtime host (`new`, `load`, or `fork`). |
| `ctx.session:switching()` | True while a session switch is preparing or finishing, or after an indeterminate failure. |
| `ctx.session:trace(session_id)` | Trace ancestor and descendant session lineage under workspace policy. |
| `ctx.session:trace_item(opts)` | Trace the containing entry and compaction surface for one semantic item. |
| `ctx.session:transition(spec, opts)` | Orchestrate a clean-context (`new`) or `workspace` session transition. |
| `ctx.session:unarchive(session_id)` | Restore an archived session (and its child agent sessions) by id, unique prefix, or path. |
| `ctx.session:update(name, opts)` | Rename the open session (optionally guarded by expected_revision) and return its Session snapshot. |

## ctx.spill

Defined in `types/mare/services/spill.gen.lua`.

| | |
|---|---|
| `ctx.spill:delete(spill_id)` | Invalidate and delete a session-owned spill object. |
| `ctx.spill:delete_owned(session_id, spill_id)` | Delete a spill object owned by an explicit `session_id` (Lua-only). |
| `ctx.spill:list_backends()` | Registered spill storage backends, with which one is currently selected. |
| `ctx.spill:read(spill_id, opts)` | Read a UTF-8 text page (default) or base64 byte page (`encoding = "base64"`) from a session-owned spill object. |
| `ctx.spill:read_bytes(session_id, spill_id, opts)` | Read raw binary bytes owned by an explicit `session_id` (Lua-only; used by UI image materializers). |
| `ctx.spill:read_owned(session_id, spill_id, opts)` | Read UTF-8 text owned by an explicit `session_id` (Lua-only). |
| `ctx.spill:register(name, backend)` | Register a named spill backend; removed automatically on disposal. |
| `ctx.spill:save(opts)` | Persist UTF-8 text (`content`) or strict padded base64 bytes (`data_base64`, max 8 MiB decoded) in the active session's spill storage. |
| `ctx.spill:save_bytes(session_id, bytes, opts)` | Save raw binary bytes (max 8 MiB) owned by an explicit `session_id` (Lua-only; includes host `path`). |
| `ctx.spill:save_owned(session_id, content, opts)` | Save UTF-8 text owned by an explicit `session_id` (Lua-only; includes host `path`). |

## ctx.status

Defined in `types/mare/services/status.gen.lua`.

| | |
|---|---|
| `ctx.status:get()` | The current unified agent status. |

## ctx.subagent

Defined in `types/mare/services/subagent.gen.lua`.

| | |
|---|---|
| `ctx.subagent:cancel(subagent_id)` | Request cancellation of a subagent and its descendants. |
| `ctx.subagent:drain()` | Close admission and wait for the current session's subagents and completion notices. |
| `ctx.subagent:get(subagent_id)` | Get a subagent by its subagent_id (or unique name). |
| `ctx.subagent:list(opts)` | List subagents started in this host (optionally paginated by cursor and limit). |
| `ctx.subagent:list_types()` | List current subagent type definitions in bytewise name order. |
| `ctx.subagent:register_type(definition, opts)` | Register a session-owned subagent type. Names use ASCII letters, digits, underscore, hyphen or dot; tool lists are dense, nonempty-string arrays without duplicates. |
| `ctx.subagent:start(opts)` | Start an isolated child subagent and return its subagent info. |
| `ctx.subagent:steer(subagent_id, text)` | Deliver steering input to a running subagent. |
| `ctx.subagent:wait(subagent_id, opts)` | Wait for a subagent to finish (non-consuming; timeout or cancellation does not cancel the child). |

## ctx.terminal

Defined in `types/mare/services/terminal.gen.lua`.

| | |
|---|---|
| `ctx.terminal:close(opts)` | Stop and close an owned PTY session cleanly. |
| `ctx.terminal:create(opts)` | Create an owner-bound PTY session; automatically closed when the caller context or RPC connection ends. |
| `ctx.terminal:list()` | List PTY sessions owned by the caller connection or plugin. |
| `ctx.terminal:read(opts)` | Read incremental `output_offset` or VT-paged output from an owned PTY, capped at `max_bytes` bytes. |
| `ctx.terminal:resize(opts)` | Resize an owned PTY window. |
| `ctx.terminal:send(opts)` | Send input bytes/text to an owned PTY and optionally wait for output/exit, capped at `max_bytes` bytes. |
| `ctx.terminal:signal(opts)` | Deliver a signal to the foreground process group of an owned PTY. |

## ctx.theme

Defined in `types/mare/services/theme.gen.lua`.

| | |
|---|---|
| `ctx.theme:cancel_preview()` | End the current preview. |
| `ctx.theme:get()` | The active theme snapshot. |
| `ctx.theme:inspect()` | Builtin, dynamic, inactive and override-only style tokens. |
| `ctx.theme:list()` | Installed and registered themes, sorted by name. |
| `ctx.theme:preview(theme)` | Preview a theme. Affects rendering only, never the preference; ends with its caller. |
| `ctx.theme:register(definition)` | Register a theme owned by the caller. Duplicate or invalid definitions fail without changing state; releasing the registration removes only it, and releasing the selected theme restores auto. |
| `ctx.theme:register_override(layer_id, styles)` | An owned style layer; later layers win, config.theme and explicit ui.style win over layers. Replacing a layer id keeps one layer; an older registration cannot remove its replacement. |
| `ctx.theme:select(theme, opts)` | Validate, optionally save atomically, then publish the selection. save defaults to false (process only); init.lua still wins on the next start. |

## ctx.tools

Defined in `types/mare/services/tools.gen.lua`.

| | |
|---|---|
| `ctx.tools:bind_declarations(declarations)` | Bind the tool declarations returned for a model request; returns opaque tokens and bound declarations. |
| `ctx.tools:capabilities()` | Declare feature capabilities implemented by the active tools service. |
| `ctx.tools:command(def)` | Register a declarative command tool (`mare.CommandTool`). |
| `ctx.tools:concurrency_safe(call)` | Whether tool call `call` is safe to run concurrently with adjacent calls. |
| `ctx.tools:cwd()` | Current working directory used by tool execution. |
| `ctx.tools:definitions(model, names)` | Build wire-ready tool declaration tables for `model` (optionally filtered to `names`). |
| `ctx.tools:disable(name)` | Disable tool `name` for this session. |
| `ctx.tools:edit_mode(model)` | Active edit tool strategy for `model` (`apply_patch` or `replace`). |
| `ctx.tools:enable(name)` | Re-enable a previously disabled tool `name`. |
| `ctx.tools:env_policy()` | Return the `{env_include, env_exclude}` filter for child processes. |
| `ctx.tools:execute(call, opts)` | Run a tool call through the guarded rewrite, validation, policy vote, approval, and sandbox execution pipeline. |
| `ctx.tools:executor(exposure)` | Look up the registered executor backend for an exposure kind, or nil. |
| `ctx.tools:fs_commit(path, content, cur, tool, opts)` | Atomic, version-checked file write following `fs_guard`. |
| `ctx.tools:fs_guard(path, verb, tool)` | Pre-write guard for edit/write tools: checks read-before-edit and dispatches `fs/edit-intent` or `fs/write-intent`. |
| `ctx.tools:get(name)` | Return the registered tool definition for `name`, or nil. |
| `ctx.tools:has_dynamic_admission()` | Whether any registered tool carries dynamic admission hooks. |
| `ctx.tools:invalidate()` | Invalidate cached tool declaration snapshots. |
| `ctx.tools:is_disabled(name)` | Whether tool `name` is currently disabled. |
| `ctx.tools:list()` | Return all registered tool definitions. |
| `ctx.tools:observation(path)` | Return the latest read-before-edit observation record for `path`, or nil. |
| `ctx.tools:observe(path, data)` | Record that the agent has observed `path` (with optional read `data`); emits `fs/observed`. |
| `ctx.tools:output_channel()` | Open an optional streaming output channel owned by the tools service, or `nil, err`. |
| `ctx.tools:process_env()` | Return `MARE_*` environment variables passed to child tool processes. |
| `ctx.tools:readonly(name)` | Whether the named tool is declared read-only. |
| `ctx.tools:register(name, opts)` | Register a tool owned by the caller; released on disposer/release or disconnect. Remote registrations force `unconfined = true` and dispatch `execute(args, ctx)` via `callback.call`. |
| `ctx.tools:register_executor(executor_id, definition)` | Register a named tool executor backend owned by the calling ctx. |
| `ctx.tools:register_search_source(definition)` | Register a search source prepared before `tool_search` reads the deferred catalog. |
| `ctx.tools:revision()` | Monotonic tool registry revision counter. |
| `ctx.tools:send_output(channel, blocks, opts)` | Send structured output blocks over a channel returned by `output_channel`. |
| `ctx.tools:shadow(name, def)` | Re-register a copy of a tool returned by `get`; revoked with its exact parent version. |
| `ctx.tools:shell()` | Return the default shell configuration (`mare.ShellConfig\|nil, err?`) for model commands. |
| `ctx.tools:shell_command(cmd)` | Build `(shell, args, input?)` for executing a shell command string. |
| `ctx.tools:side_effect(name)` | Return `(side_effect, idempotent, unconfined)` classification for tool `name`. |
| `ctx.tools:snippets(model)` | Prompt snippets for tools enabled for the given model, in registration order. |
| `ctx.tools:touch()` | Bump the tool registry generation (for example after deferred tool loading changes). |
| `ctx.tools:with_file_lock(paths, fn)` | Run `fn` while holding the in-process file mutation locks for `paths` in sorted realpath order. |

## ctx.ui

Defined in `types/mare/services/ui.gen.lua`.

| | |
|---|---|
| `ctx.ui:action(name, fn, opts)` | Register a named key action with optional default keys; removed when the caller is disposed. |
| `ctx.ui:artifact_image(artifact, opts)` | Bind a saved output image artifact to the current session epoch and return a renderable `mare.Node`. |
| `ctx.ui:call_keymap(context, key, action)` | Dispatch a key sequence or named keymap action in the session runtime. |
| `ctx.ui:cancel_markdown(document_id)` | Cancel outstanding presenter work for a Markdown document and fall back to source. |
| `ctx.ui:card(opts)` | Emit a durable, reflowable titled Markdown card in the transcript; returns block id/status or `nil, err`. |
| `ctx.ui:clear()` | Clear the conversation scrollback in the connected UI frontend. |
| `ctx.ui:completion_register(spec)` | Register a completion source for `/` commands, `@` mentions, or custom triggers; withdrawn on dispose. |
| `ctx.ui:confirm(prompt, default, opts)` | Present a yes/no confirmation prompt and return `boolean\|nil, err?`. |
| `ctx.ui:context(name, opts)` | Register a plugin-scoped keymap subcontext; removed when the caller is disposed. |
| `ctx.ui:delete_keymap(context, key)` | Delete the topmost runtime keymap binding of a key sequence in a context. |
| `ctx.ui:delete_markdown(document_id)` | Remove a caller-owned Markdown document and cancel its pending work. |
| `ctx.ui:editor(opts)` | Open an interactive multi-line editor dialog and return the edited text or `nil, err`. |
| `ctx.ui:editor_config(opts)` | Configure editor appearance options (`prompt`, `placeholder`, `busy_placeholder`, `border`, `shell`). |
| `ctx.ui:editor_cursor()` | Return the current 0-based editor cursor position `{row, col}` aligned to grapheme boundaries. |
| `ctx.ui:editor_hint(text)` | Set the transient hint text displayed beneath the editor. |
| `ctx.ui:editor_insert(text)` | Insert `text` at the current cursor (replacing any active selection) as an undoable edit. |
| `ctx.ui:editor_replace(range, text)` | Replace `range` (`{start, end}` or nil for selection/cursor) with `text` as an undoable edit. |
| `ctx.ui:editor_selection()` | Return the active editor selection `{start, end, anchor, head, text}`, or nil when collapsed. |
| `ctx.ui:editor_state()` | Return the full editor state snapshot (`text`, `cursor`, `selection`, `empty`, `revision`). |
| `ctx.ui:exit(opts)` | Request the UI frontend to exit (`force = true` bypasses exit guards). |
| `ctx.ui:exit_guard(on)` | Enable or disable the frontend exit guard. |
| `ctx.ui:flag(name, value)` | Set a named boolean UI context flag used by keymap `when` expressions. |
| `ctx.ui:get_editor()` | Return the current raw editor input text (including leading `!`/`!!` in shell mode). |
| `ctx.ui:get_keymap(context)` | Return the runtime keymap snapshot (contexts, actions, bindings, and essential conflicts). |
| `ctx.ui:get_markdown(document_id)` | Latest rendered snapshot for a caller-owned Markdown document. |
| `ctx.ui:get_select_preview(select_id, index, tab)` | Evaluate the active `ui.select` preview callback for the highlighted item and tab. |
| `ctx.ui:handler(kind, name, opts, fn)` | Register an ordered handler for `input/submit` ("submit") or `key/interrupt` ("interrupt"); withdrawn with the caller. |
| `ctx.ui:input(opts)` | Present a single-line input prompt and return the entered string or `nil, err`. |
| `ctx.ui:invalidate(component, opts)` | Invalidate a rendered widget or status segment, or update viewport dimensions and trigger resize. |
| `ctx.ui:invalidate_presentations()` | Invalidate cached Markdown presentations across active documents after a display or presenter state change. |
| `ctx.ui:keymap()` | Return a summary map of active key bindings. |
| `ctx.ui:list_markdown_presenters()` | Active fenced-code presenter registrations, sorted by language. |
| `ctx.ui:markdown_presenter(language, def)` | Register a fenced-code presenter for `language`; disposing the returned function removes it and cancels its work. |
| `ctx.ui:notify(text, level)` | Show a transient notification message with optional severity level (`info`, `success`, `warning`, `error`). |
| `ctx.ui:overlay(title, node, opts)` | Display a titled modal overlay containing `node`. |
| `ctx.ui:print(node)` | Append a rendered `mare.Node` block to the conversation scrollback. |
| `ctx.ui:questions(opts)` | Present a structured multi-question prompt to the user and return the answers or `nil, err`. |
| `ctx.ui:register_entry_renderer(kind, opts)` | Register a display-only custom entry renderer owned by the caller. |
| `ctx.ui:register_markdown_transform(opts)` | Register a Markdown transformer for selected message surfaces, owned by the caller. |
| `ctx.ui:register_override(name, fn)` | Wrap or replace `select`, `input`, `confirm`, or `notify`; released on dispose. |
| `ctx.ui:render_markdown(document_id, source, opts)` | Render fenced blocks in a caller-scoped Markdown document asynchronously via registered presenters. |
| `ctx.ui:render_text(node, width)` | Render a `mare.Node` into plain text wrapped to optional column `width`. |
| `ctx.ui:renderer(kind, r)` | Register a display renderer for a message or tool kind (`tool:<name>`, `message:user`, etc.); removed on dispose. |
| `ctx.ui:select(opts)` | Present an interactive item picker and return `(index, tab_or_error, key, filter)`. |
| `ctx.ui:send_editor(text, opts)` | Submit the current editor buffer (or override text) to the session's agent lane. |
| `ctx.ui:set_editor(text, opts)` | Set or update the main input editor buffer text and optional draft/attachment options. |
| `ctx.ui:set_editor_cursor(row, col)` | Move the editor cursor to `(row, col)` (or `{row, col}`), clamped to grapheme boundaries, and clear selection. |
| `ctx.ui:set_editor_selection(anchor, head)` | Set the editor selection `{anchor, head}` or pass nil to clear it. |
| `ctx.ui:set_external_editor(action, text)` | Acquire, commit, or cancel the per-session external editor lock. |
| `ctx.ui:set_keymap(context, key, action, opts)` | Bind or mask a key sequence in a context for the caller's lifetime. |
| `ctx.ui:set_queue_mode(mode)` | Update the active queue input mode indicator in the UI. |
| `ctx.ui:set_queued(n)` | Update the displayed count of queued inputs in the UI. |
| `ctx.ui:status(status_id, def)` | Register a named statusline segment; returns a release disposer. |
| `ctx.ui:style(name, spec)` | Set a named UI style override; the previous style returns when the caller is disposed. |
| `ctx.ui:theme(colors)` | Apply a color map to the connected UI frontend. |
| `ctx.ui:transcript(title, node, opts)` | Open or update the live transcript view with `title` and `node`. |
| `ctx.ui:transcript_close(title)` | Close the open transcript view (optionally matching `title`). |
| `ctx.ui:transcript_title()` | Return the title of the currently open transcript view, or nil. |
| `ctx.ui:transform_markdown(text, opts)` | Run registered Markdown transformers for a message surface. |
| `ctx.ui:update_editor(text, opts)` | Synchronize the external editor draft state and broadcast updated state to attached frontends. |
| `ctx.ui:widget(widget_id, def)` | Register a named widget rendered above or below the editor; withdrawn when the caller is disposed. |
| `ctx.ui:width()` | Return the current frontend viewport width in columns. |
| `ctx.ui:working(label, detail, opts)` | Set or clear the active working indicator (`label = nil` clears it). |

## ctx.usage

Defined in `types/mare/services/usage.gen.lua`.

| | |
|---|---|
| `ctx.usage:context_tokens()` | Tokens occupying the context window after the last request. |
| `ctx.usage:cost_text(format)` | Cost as text: "$0.1234", "subscription, not billed", or both. |
| `ctx.usage:get()` | Totals, context tokens and rate limits of the session in one object. |
| `ctx.usage:quota_lines()` | Subscription quota as display lines, one per window (/status, the statusline). |
| `ctx.usage:rate_limits()` | Latest subscription quota per provider (the llm/rate-limits event). |
| `ctx.usage:set_context(tokens, opts)` | Update the context token estimate and emit usage/changed; notify = false is silent. |
| `ctx.usage:totals()` | The session's totals: reset by a new session, recounted on resume. |

## ctx.web

Defined in `types/mare/services/web.gen.lua`.

| | |
|---|---|
| `ctx.web:fetch(url, opts)` | Fetch a URL using the currently selected fetch backend under active sandbox limits. |
| `ctx.web:list_providers(kind)` | Registered web backends of `kind` sorted by provider_id, each marked `selected` when active. |
| `ctx.web:register_fetch(opts)` | Register a fetch backend owned by the caller; removed on release or disconnect. |
| `ctx.web:register_search(opts)` | Register a search backend owned by the caller; removed on release or disconnect. |
| `ctx.web:search(queries, opts)` | Search the web using the currently selected search backend under active sandbox limits. |

## ctx.webserver

Defined in `types/mare/services/webserver.gen.lua`.

| | |
|---|---|
| `ctx.webserver:get()` | Status and bound endpoints of the host webserver listener. |
| `ctx.webserver:list_assets()` | Registered static asset mounts on the host webserver. |
| `ctx.webserver:list_routes()` | Registered HTTP routes on the host webserver. |
| `ctx.webserver:register_assets(dir, opts)` | Register a static asset directory served under a URL prefix on the host webserver. |
| `ctx.webserver:register_route(path, opts)` | Register a named HTTP route on the host webserver. Requests are forwarded to the caller handler. |

## ctx.workspace

Defined in `types/mare/services/workspace.gen.lua`.

| | |
|---|---|
| `ctx.workspace:get()` | Return the canonical workspace root, trust state, and workspace directory list. |
| `ctx.workspace:set_trust(trusted, save)` | Update project trust for this workspace across all active sessions attached to it. |

## mare

Defined in `types/mare.lua`, `types/mare/config.lua`, `types/mare/loader.lua`, `types/mare/profile.lua`.

| | |
|---|---|
| `mare.setup(opts)` | Layers, lowest first: defaults &lt; state.json &lt; user init.lua &lt; project .mare/init.lua &lt; env `MARE_<NAME>` &lt; CLI `--set name=value`. An unknown name warns (did-you-mean); a wrong type or value is reported and keeps the default (`--strict-config` refuses to start on either); `/config` shows each value with its source and file:line. |
| `mare.plugin(name, overrides)` | Override a plugin's default config (lazy.nvim merge: tables merge recursively, lists replace). |
| `mare.config_clear(path)` | Clear one layer's override at `path` (`nil` cannot say "clear"). In a config file the value (any depth, e.g. `"plugins.web.search.engine"`) and its sources go, so the default or a later layer applies. At runtime the runtime write of a whole option (`"name"` or `"plugins.<plugin>.<option>"`) goes and the startup configuration shows through again, applied and announced like a write. Clearing what nothing set does nothing. |
| `mare.config_declared()` | Every declared plugin option, leaf by leaf (`"plugins.<plugin>.<path>"`; an object's fields are its leaves, a list, map or union is one), sorted — set or not. `/config` and `config.describe` list these. |
| `mare.config_sensitive(path)` | Hide a configuration path and all its descendants in `/config` diagnostics (for example, `plugins.myplugin.private_value`). This does not encrypt the configuration or redact arbitrary plugin output. Marks are retained during config reloads. |
| `mare.config_sensitive_paths()` |  |
| `mare.config_is_sensitive(path)` | Whether a dotted configuration path is sensitive (via `mare.config_sensitive`, `mare.options[k].sensitive`, or sensitive key/segment names such as `api_key`, `headers`, `query`). |
| `mare.config_redact(path, value)` | Redact sensitive values in a configuration value at `path`: the one redactor for every way a config value leaves (`/config`, RPC `options.get` / `config.describe`, `option/changed`). It knows every rule — `mare.config_sensitive` marks, `sensitive` in core options and in plugins' `config_schema` (at the path or a parent), key-name rules. |
| `mare.on(event, fn)` | Subscribe to a kernel event from init.lua, e.g. `mare.on("model/changed", function(e) ... end)`: a native listener (a plugin's `ctx:on`). A waterfall listener is `fn(value, ..., next)` and passes on with `next(...)`; a decision point is voted on with `mare.policy.voter`. Handlers are dropped and re-registered on `/reload config`. |
| `mare.model(def)` | a provider with several routes (opencode-go): this model's route, above every other layer Wire plugins may declare additional model fields via `WireSpec.model_fields`. this model's parameters: a definition (mare.ParamDef) adds or changes one, a plain value is its default (or, with `alias`, the value the alias pins) Add a model or override fields of a built-in one. |
| `mare.params(rule)` | a route some plugin provides: a registered wire's `describe.route`, a provider's route, or one a plugin's own rules map (a third-party wire's route included); another one never matches and `/doctor` names it Model parameters and their wire mappings. Layers, low → high: built-in &lt; provider &lt; model &lt; user (init.lua; provider &lt; route &lt; family/glob &lt; one model; state.json's remembered values below init.lua's) &lt; trusted project (.mare/init.lua) &lt; agent &lt; session (/model, /effort, keys) &lt; request. Function mappings run with the trust of provider/request hooks; from project scope they can't write authorization / x-api-key. |
| `mare.provider(name, opts)` | "ENV_NAME", a literal key, or a key helper: a command whose stdout is the key (or JSON {key\|token, expires_at}); run one at a time, cached until `refresh_interval_ms` / `expires_at`, and run again after a 401 Wire plugins may declare additional provider fields via `WireSpec.provider_fields`. Configure a provider (endpoint / credentials). |
| `mare.config_origin()` | The currently executing config layer and its root file. An unrelated coroutine or ordinary plugin/runtime call returns {layer="runtime"}, even while init.lua yields. Host-owned, detached metadata; changing this table cannot change config provenance. |
| `mare.config_resolve_preset(spec, value)` | Resolve a preset-as-data configuration (`defaults < inherited preset < explicit leaf overrides`). Supports `like` inheritance chains with cycle detection and did-you-mean diagnostics on unknown presets. |
| `mare.config_sources()` | Where each effective config value came from: `{ [key] = {layer, where?} }`. |
| `mare.config_cli(path)` | Validated value of `path` supplied via a dedicated startup CLI flag such as `--permission`, `--yolo`, or `--approval-policy` (unaffected by runtime writes), or `nil` when `path` was not pinned by a CLI flag. |
| `mare.config_describe(keys, opts)` | Describe configuration options and dotted paths with redacted effective/default values and provenance. |
| `mare.config_warnings()` | Config warnings (unknown names, wrong types) with file:line. |
| `mare.reload_config()` | Re-run init.lua and reload plugins whose options changed (`/reload config`). `pending`: plugins left waiting for a service nobody provides (not failures; they start when it appears). |
| `mare.reload()` | plugins started, plugins of removed packages disposed, everything else reloaded (a plugin whose new definition fails keeps the old one), packages' config() again. Also emitted as `reload/completed`. |
| `mare.reload_version()` | The host's reload state as an opaque string: it changes with every config or plugin reload (and while one is running or incomplete). Compare it with an earlier value to tell whether a cached decision (e.g. a tool approval) predates a reload. `"closed"` once the host is gone. |
| `mare.log(level, msg)` | Write a log line attributed to the running plugin. |
| `mare.caller_site(skip)` | "file:line" of the nearest Lua caller outside the core prelude and the config API (`/config` shows where an option was set, like Neovim's `:verbose set`); `skip` skips that many more frames first (a helper names its caller's line, not its own). nil when there is none. |
| `mare.date(fmt, ms)` |  |
| `mare.error(code, message, data)` | A mare.Error value to raise with `error(value, 0)`: a core code or one the service declares (`<service>.<name>`), with data its declaration describes. It keeps its fields through service proxies and mare.exec.run. |
| `mare.shared(name)` | One table per name for the life of the Lua VM, the same for every plugin and kept across plugin reloads and provider replacement: state the providers of one contract share (e.g. `mare.shared("my.state")`). |

## mare.Service

Defined in `types/mare/host/service.lua`.

| | |
|---|---|
| `mare.Service.constructor(self, ctx, name, options)` | Construct and provide service `name` from `ctx` under its contract (admitted before anything is published; a refusal fails the providing fiber). |
| `mare.Service.new(ctx, ...)` | A new instance (`cordis.class` constructor form). |
| `mare.Service.is(value)` | Whether `value` is an instance of this class. |

## mare.activity

Defined in `types/mare.lua`.

| | |
|---|---|
| `mare.activity.capture(provider)` | Synchronously invoke a provider and return its isolated JSON value; raises on yield, cycles, non-JSON values or resource limits. Used by activity registries, public to any plugin. |

## mare.approvals

Defined in `types/mare/approvals.lua`.

| | |
|---|---|
| `mare.approvals.check_human(token, binding, arguments, decision)` | Check native human evidence without consuming it. Lua cannot issue evidence. |
| `mare.approvals.consume_human(token, binding, arguments, revision, decision)` | Consume native evidence once, after matching all owner/turn/request/snapshot fields. |
| `mare.approvals.register_snapshot(binding, action)` |  |
| `mare.approvals.public_result(result)` | Project a full local result into the closed public audit DTO. No free-form reason, arguments, schema, permissions, amendment, snapshot, or receipt is exposed. |

## mare.clipboard

Defined in `types/mare/ui.lua`.

| | |
|---|---|
| `mare.clipboard.copy(text)` |  |

## mare.command

Defined in `types/mare/config.lua`.

| | |
|---|---|
| `mare.command.alias(short, target)` | `/short` runs `/target ...`. |

## mare.contracts

Defined in `types/mare/host/contracts.lua`.

| | |
|---|---|
| `mare.contracts.permitted(service, method, caller)` | Whether the explicit caller (or unrestricted host call when omitted) may call `service.method`. |
| `mare.contracts.is_peer(caller)` | Whether the explicit caller is a restricted peer RPC connection (never true for owners or in-process Lua). |
| `mare.contracts.allows_session_path(caller)` | Whether the explicit caller may address sessions by filesystem path (local/admin owner only). |
| `mare.contracts.validate(service, method, value)` | Check `value` against `service.method`'s result contract without calling anything. |

## mare.crash

Defined in `types/mare/store.lua`.

| | |
|---|---|
| `mare.crash.begin(fingerprint)` | Durably mark a request in flight (persisted before it returns); a mark a dead process left counts as one crash. The fingerprint is the stable request id (`llm` uses `"req:" .. id`). |
| `mare.crash.finish(fingerprint)` | The attempt ended normally: the entry ends (kept, evictable) and its count is cleared. |
| `mare.crash.release(fingerprint)` | The request was refused, not sent (crash limit): the entry ends (evictable) but keeps its count, so the same request stays refused and the refusal is not counted as another crash. |

## mare.crypto

Defined in `types/mare/text.lua`.

| | |
|---|---|
| `mare.crypto.sha256(s)` |  |
| `mare.crypto.random_bytes(n)` |  |
| `mare.crypto.random_hex(n)` |  |
| `mare.crypto.uuid()` |  |
| `mare.crypto.pkce()` |  |
| `mare.crypto.base64_encode(s)` |  |
| `mare.crypto.base64_decode(s)` |  |
| `mare.crypto.base64url_encode(s)` |  |
| `mare.crypto.base64url_decode(s)` |  |

## mare.env

Defined in `types/mare.lua`.

| | |
|---|---|
| `mare.env.get(name)` |  |
| `mare.env.hostname()` |  |

## mare.exec

Defined in `types/mare/proc.lua`.

| | |
|---|---|
| `mare.exec.run(ctx, fn, ...)` | Run `fn(...)` tagged with `ctx` (an outer context is never replaced: a nested run belongs to the outer call). While a session journal (`mare.store.open`) is open, a new call is admitted only on its durable start — a journal write with `ack = ctx.tool_call_id`; otherwise it raises `EXEC_NOT_DURABLE: <reason>` and `fn` never runs. Each ack admits once. `approval_key` names the frozen call (tools service): in a sub-agent, a call the parent approved is matched by it and escapes the inherited ceiling. lifetime="ephemeral-read-only" skips journal admission only with an explicit confined read-only sandbox blocking all network. Unknown or unsafe lifetimes fail before fn runs. |
| `mare.exec.current()` | A detached, read-only view of the calling code's context (nil outside tool calls). `denials` lists refusals so far; changing this view cannot change the active policy. |
| `mare.exec.check(op, target)` | Raise if `op` ("write"\|"read"\|"network") on `target` is refused for the calling code. |
| `mare.exec.check_mutation(operation)` | Refuse a host-state mutation inside ephemeral-read-only execution. Called before changing options or service state; filesystem/network/process operations also enforce their policies. |
| `mare.exec.refuse_unconfinable(what)` | Raise if `what` cannot be confined while a sandbox policy is active. |

## mare.flags

Defined in `types/mare/flags.lua`.

| | |
|---|---|
| `mare.flags.register(name, opts)` | Register a CLI flag (`--<name>` / `--<name>=<val>`). |
| `mare.flags.get(name)` | Read a registered CLI flag value (CLI override -&gt; default -&gt; nil; nil if unregistered). |
| `mare.flags.list()` | List all registered CLI flags and their resolved values. |

## mare.fs

Defined in `types/mare/fs.lua`.

| | |
|---|---|
| `mare.fs.resolve(p)` |  |
| `mare.fs.relative(p)` | `p` without the working directory in front when it lies below it, else `p` as given (Windows: separators, case and `\\?\` folded; canonical forms compared as a fallback). |
| `mare.fs.read(p, opts)` |  |
| `mare.fs.read_window(p, opts)` | Read a 1-based line slice of `p`, returning `{lines, total, size, hash}` or `nil, err`. |
| `mare.fs.write(p, data, opts)` | `sync_boundary`: for atomic writes, also sync ancestor entries up to this vouched durable directory. `atomic`: temp file + fsync + rename (keeps an existing file's permissions); `create_new`: fail if the path exists (with atomic: hard-linked into place, never clobbers). |
| `mare.fs.realpath(p)` | Canonical path (symlinks resolved), or the absolute path when it doesn't exist yet. |
| `mare.fs.resolve_path(path, base)` | Resolve the logical and physical forms of a path, failing if its identity is ambiguous. |
| `mare.fs.project_root()` | Current project resource root, or the session working directory when no root is present. |
| `mare.fs.compile_path_glob(pattern, base)` | Compile a separator-aware absolute or base-relative path glob. |
| `mare.fs.path_within(path, root, conservative)` |  |
| `mare.fs.exists(p)` |  |
| `mare.fs.git_dir(p)` | Return the validated Git directory of the worktree at `p` (Git's rules: a `.git` directory or gitfile naming one with a valid HEAD, objects and refs). nil when there is none or the `.git` directory is not valid (discovery goes on upward); nil, "invalid gitfile" for a `.git` file that names no valid Git directory (Git's discovery stops there: not a repository). Inside a tool call, an unreadable metadata path raises like the other fs functions. |
| `mare.fs.git_branch(p)` | Synchronously read the current Git branch name (or 7-character detached HEAD commit prefix) by walking upward from `p` (default: working directory) to the nearest validated `.git` directory or gitfile and reading `HEAD`. Returns nil outside a Git repository or at an invalid `.git` file. Inside a tool call, an unreadable metadata path raises like the other fs functions. |
| `mare.fs.stat(p, opts)` |  |
| `mare.fs.identity(p)` | Stable cross-platform file identity (`dev:ino` on Unix; volume/index on Windows) when `p` exists. |
| `mare.fs.sniff(p)` | Async bounded content sniff for `p` (`binary`, `"text"`/`"empty"`/`"image"`/`"pdf"`/`"binary"`, and byte `size`). |
| `mare.fs.mkdir(p)` |  |
| `mare.fs.remove(p, recursive)` |  |
| `mare.fs.rename(a, b)` |  |
| `mare.fs.list(p, opts)` |  |
| `mare.fs.walk(dir, opts)` |  |
| `mare.fs.index(dir, opts)` | An incrementally maintained, gitignore-aware file list (the `@` completion index). |
| `mare.fs.watch(path, on_change)` | Watch `path` for policy-filtered filesystem changes, invoking `on_change({ changed_paths, truncated })`. |

## mare.http

Defined in `types/mare/net.lua`.

| | |
|---|---|
| `mare.http.request(opts)` | Failures read `transport: …` (retryable), `malformed_response: …` (an unparseable stream, including invalid UTF-8) or `aborted` (cancelled). `idle_timeout_ms`: no bytes at all for this long fails the read (heartbeat comments count as activity); `header_timeout_ms`: wait for the response headers; `fresh`: a new connection instead of a pooled one. Proxies come from HTTP(S)_PROXY / ALL_PROXY / NO_PROXY (SOCKS and PAC are a clear error); custom CAs from MARE_CA_CERTIFICATE, else SSL_CERT_FILE plus SSL_CERT_DIR (a directory of PEM / .crt files). `proxy`: this request's own HTTP(S) proxy (NO_PROXY still applies); `connect_timeout_ms`: its connect timeout (default 30 s). `sse_max_line_bytes` / `sse_max_event_bytes`: `next_event` fails with `malformed_response` once one SSE line, or one event's lines together, exceed this many bytes (default 16 MiB each). `redirect`: `"follow"` (default), `"manual"` (return 3xx without following), or `"same-origin"`. `max_redirects`: maximum redirects allowed (default 10). |
| `mare.http.retry_after(headers)` | `retry-after-ms`, or `Retry-After` in seconds or as an HTTP date, in milliseconds. |
| `mare.http.listen_once(opts)` | `ports`: the first free one is used; `on_bound(port)` runs once listening (build the redirect URL). |
| `mare.http.url_encode(s)` |  |
| `mare.http.build_url(base, query)` |  |
| `mare.http.websocket(opts)` |  |
| `mare.http.serve(opts)` | Start a loopback-only HTTP/1.1 + SSE + WebSocket server. Defaults to `127.0.0.1` and ephemeral port `0`. Rejects non-loopback `host` binds, validates `Host` against loopback names/IPs (and optional `trusted_hosts`) (DNS rebinding protection), and enforces `Sec-Fetch-Site` / `Origin` checks (rejects cross-site browser requests and WebSockets). Token authentication is mandatory by default: when `token` is omitted, uses the persisted 32-byte random runtime token (`<state>/host/token`, `0600`, rotated via `mare token rotate` for new requests/connections); pass `token = false` to explicitly disable authentication, or pass a non-empty string for a custom static token. `<state>/host/` is protected by the `fs_host_protect` sandbox capability (gated before creation/rotation unless `accept_partial = { "fs_host_protect" }` is set). Not protected by `fs_host_protect`: `danger-full-access`, disabled permissions plugin, approved unconfined escalations, custom static tokens, restricted processes launched with a raw Seatbelt profile before token creation, and pre-existing or external-process sandboxes on partial backends. Accepts `Authorization: Bearer <token>`, `x-mare-token`, or browser `GET ?token=<token>` (exchanged via `303 See Other` for an in-memory `HttpOnly; SameSite=Strict` session cookie); WebSocket upgrades also accept `?token=`. `header_read_timeout_ms` sets the HTTP header read timeout (default `30000`, at least 1) to mitigate slowloris. `ws_max_message_bytes` (default `4194304`) and `ws_max_frame_bytes` (default `1048576`) cap WebSocket sizes. `max_concurrency` and `max_connections` must be at least 1 and are clamped to Tokio's `Semaphore::MAX_PERMITS`. `close_grace_ms` sets the WebSocket close grace period in milliseconds (default `5000`, non-negative) before aborting a non-draining handler. `body_stall_ms` bounds how long a request body may stay unread while the client keeps sending, and how long a WebSocket may stay backpressured by a full, unconsumed inbound queue (default `60000`, non-negative); the connection is then cancelled and closed. Cancellation happens early when a disconnect is observable; otherwise the stall timeout is the backstop. `cancel` (optional `mare.Token`) only cancels waiting for the startup `.token.lock` (within the 5000 ms lock acquisition timeout when using the default runtime token); once `mare.http.serve` has started, cancelling it does not stop the server (call `HttpServer:close()` to shut down). |

## mare.image

Defined in `types/mare/ui.lua`.

| | |
|---|---|
| `mare.image.load(path, opts)` | Load, downscale (longest side ≤ max_dim) and encode an image file. |
| `mare.image.clipboard(opts)` |  |
| `mare.image.is_image(path)` |  |
| `mare.image.to_png(data)` | Decode base64-encoded image bytes within safety limits, downscale if oversized, and re-encode as PNG (or JPEG when large). |

## mare.json

Defined in `types/mare/text.lua`.

| | |
|---|---|
| `mare.json.encode(v, opts)` |  |
| `mare.json.decode(s)` |  |
| `mare.json.decode_unique(s)` | Decode JSON and reject duplicate object members at every depth (after key unescaping). |
| `mare.json.decode_lossless(s)` | Preserve null and empty containers from the serde_json parsed value; reject further Lua conversion loss. Raw number precision and duplicate keys may already be lost during parsing. |
| `mare.json.encode_strict(v)` | Encode a JSON value; reject functions, non-finite numbers, cycles, sparse arrays and unsupported metatables. |
| `mare.json.clone(v)` | Deep-clone a Lua value through strict JSON value representation, preserving `mare.json.null` and empty arrays. |
| `mare.json.copy_metadata(source, target)` | Copy JSON schema property-ordering metadata recursively from `source` tables to matching `target` tables. |
| `mare.json.array(t)` |  |
| `mare.json.is_array(t)` | Whether a table carries the JSON array marker (including an empty array). |

## mare.keymap

Defined in `types/mare/keymap.lua`.

| | |
|---|---|
| `mare.keymap.set(context, key, action, opts)` | Bind a key (Neovim `vim.keymap.set`). context: "editor" (or a list of contexts). key: "&lt;C-t&gt;", "&lt;M-CR&gt;", "ctrl+t", ... `unique = true` raises when the key is already bound in that context. `priority` (finite number, default 0) breaks ties within the same source layer (`project > user > plugin > default`). `when`: the binding takes the key only while these hold, otherwise the key goes on to the binding below it (then along the lookup chain). Anonymous function actions require a non-empty `desc`. The handle undoes the binding (`handle:remove()` or `mare.keymap.remove(handle)`). `false` masks the caller's layer and all lower contexts along the chain; it accepts no `when` and ignores `unique`. Services delegating on behalf of a caller may pass `owner` and `layer` (`"default"` or `"plugin"`). |
| `mare.keymap.set_many(specs, opts)` | Register multiple bindings in one atomic batch. Each item is either `{ context, key_or_keys, action, opts? }` or `{ context?, key?, keys?, action, desc?, unique?, priority?, when? }`. Flushes all changed stacks in a single host update; `handle:remove()` or `mare.keymap.remove(handle)` removes the whole batch. |
| `mare.keymap.del(context, key, opts)` | Remove the topmost binding of `key` in `context` in the caller's layer (or `opts.owner` / `opts.layer`); returns whether a binding was removed and the normalized key sequence. |
| `mare.keymap.context(name, opts)` | Declare a plugin subcontext (`name` must be `parent .. "." .. seg` with `seg` matching `^[a-z][a-z0-9_-]*$`). `handle:remove()` deletes the context, its descendants, and their bindings. |
| `mare.keymap.list(opts)` | Without `opts.snapshot = true`, returns every reachable unmasked binding of each key in each context (optionally filtered by `opts.context`), ordered by source layer, `priority`, and binding order. With `opts.snapshot = true`, returns the full keymap snapshot (`contexts`, `actions`, `bindings`, `conflicts`). |
| `mare.keymap.hint(action, context)` | The keys that trigger `action` with focus at `context` (default "editor"), ordered by source layer (`project > user > plugin > default`), then `priority`, then binding order. |
| `mare.keymap.actions()` | Registered core and Lua actions (`core = true` for core actions). |
| `mare.keymap.action(name, fn, opts)` | A named Lua action, bound by name: `mare.keymap.set("editor", "<M-c>", "copy.last")`. Built-in ones: "copy.last" (copy), "agents" (/agents; bound to ← with `when = "editor_empty"`), "agents.back" / "agents.prev" / "agents.next" (Esc, Alt+←/→ in a sub-agent's view; they decline in views the agents plugin didn't open). `fn(ev)` runs as the key is handled, up to its first yield: returning exactly `false` declines (the key goes on to the binding below, then the editor), anything else takes the key; decide before waiting on anything. An error takes the key and shows a notice. The handle removes it (`ctx.ui:action` does that when the plugin is disposed). |
| `mare.keymap.resolve(context, seq, opts)` | Resolve a key or key sequence along `context`'s lookup chain under optional predicate flags. |
| `mare.keymap.invoke(action, opts)` | Run a registered Lua action (named or `fn:N`). Returns `true` if handled, `false` if declined, or `nil` if no such Lua action is registered. |
| `mare.keymap.is_reserved(key)` | Whether `key` (or any token in a key sequence) is reserved by the core. |
| `mare.keymap.remove(handle)` | Remove every binding registered by a handle returned from `set` or `set_many`. |
| `mare.keymap.callback(id)` | The Lua function bound under an action id (from `set`), or nil. |

## mare.loader

Defined in `types/mare/loader.lua`.

| | |
|---|---|
| `mare.loader.reset(path)` | Drop the whole cache, or one source file's entry. Returns how many entries were removed. |
| `mare.loader.prune()` | Drop entries for another mare version or a changed / missing source (also done at most daily on startup). |

## mare.lock

Defined in `types/mare/fs.lua`.

| | |
|---|---|
| `mare.lock.file(path, opts)` | Exclusive lock on `path` (created if missing); `cancel` / `timeout_ms` only end the wait. |

## mare.mcp

Defined in `types/mare/mcp.lua`.

| | |
|---|---|
| `mare.mcp.connect(spec, cancel)` |  |
| `mare.mcp.oauth_begin(url, key, opts)` |  |
| `mare.mcp.logout(key, url)` |  |
| `mare.mcp.has_credentials(key, url)` |  |
| `mare.mcp.spill(data, ext)` |  |
| `mare.mcp.log(server, level, msg)` |  |
| `mare.mcp.redact(s)` |  |

## mare.net.unix

Defined in `types/mare/net.lua`.

| | |
|---|---|
| `mare.net.unix.connect(path, opts)` |  |
| `mare.net.unix.request(path, line, opts)` | One connection, one line out, one line back; `timeout_ms` bounds the whole exchange. |
| `mare.net.unix.listen(path, handler, opts)` | Listen on a Unix socket: `handler(conn)` runs (async) once per accepted connection, so several clients can be connected at once. A stale socket file is replaced; one that still answers is refused ("already in use"). The file mode defaults to 0600. |

## mare.peers

Defined in `types/mare/net.lua`.

| | |
|---|---|
| `mare.peers.publish(namespace, fields)` | Put `fields` (JSON-serialisable, named) into this process's registry entry as `plugins.<namespace>`, replacing what that namespace held; `nil` removes it. Other namespaces and the core fields are never touched. `namespace`: lowercase letters, digits, `.`, `_`, `-` — by convention the publishing plugin's name (one owner per namespace). Written with the next registry rewrite (at most every 200 ms); kept, unwritten, while the session isn't registered. |
| `mare.peers.list()` | Registered sessions on this machine (this one included); dead ones are pruned. |
| `mare.peers.self()` | This session's entry, or nil when it isn't registered (headless/--rpc without --peer, or `peer = false`). |
| `mare.peers.dir()` | The registry directory. |
| `mare.peers.connect(target, opts)` | Open a registered peer. The caller owns the client; close it, e.g. from a ctx:effect cleanup. Clients survive session switches; each message names the session current when it is sent (`from.session_id`, self-reported; none for a message sent while the host is still starting). Default peer privileges never allow arbitrary control. No arbitrary socket path is accepted. |

## mare.plugins

Defined in `types/mare/config.lua`, `types/mare/plugins.lua`.

| | |
|---|---|
| `mare.plugins.add(list)` | A package declaration (compatible with lazy.nvim specs). `"owner/repo"`, `"owner/repo:subdir"` or a table; with no version field the newest semver tag is used, else the default branch. A package declared more than once is merged: the last write wins for single values and `opts` (deep merge); `dependencies` and `only` are merged; any `enabled = false` disables it (and the dependencies only it needs); a package only declared `optional = true` is dropped. |
| `mare.plugins.status(name)` | The state of plugin `name`; an unknown name gives `defined = false`. |
| `mare.plugins.list()` | Every defined plugin, in definition order. |
| `mare.plugins.failures()` | What failed at startup or in the latest full /reload: plugins (also those whose definition could not be read) and config files. During a full /reload, returns the failures as they were when the reload started (a half-reloaded tree has no meaningful answer); live again when it ends, before `reload/completed`. |
| `mare.plugins.declaring(field, value)` | The plugins whose definition declares `value` in `field` ("tags", "wires", or "providers"): the defined ones in definition order, then the ones turned off by the config (`enabled = false`, `plugins.preset`). Read from the definitions, so a plugin that is off or failed still counts. |
| `mare.plugins.report_problem(plugin, key, message)` | Record or replace a non-fatal declaration problem on `plugin` under `(plugin, reporter, key)`. Automatically cleared when either the target plugin or the reporter reloads or unloads. |
| `mare.plugins.clear_problem(plugin, key)` | Clear the declaration problem on `plugin` reported by the caller under `key`. |
| `mare.plugins.list_installed(opts)` | Package manager for `/plugins`; the same operations as `mare plugin …`. |
| `mare.plugins.run(action, name, opts)` |  |
| `mare.plugins.install(name, opts)` | Install a catalog plugin by short name or sync a declared source. |
| `mare.plugins.update(name, opts)` | Update declared packages to their newest allowed revision (`update`). |
| `mare.plugins.check(names, opts)` | Fetch and compare revisions without moving checkouts or modifying locks. |
| `mare.plugins.uninstall(name, opts)` | Uninstall a catalog plugin by short name or clean an unused source. |
| `mare.plugins.manifest(name)` | Read `mare-plugin.toml` for an installed, enabled package. |
| `mare.plugins.catalog()` | The plugin catalog with `enabled` / `recommended` for this project. |
| `mare.plugins.diagnostics()` | Catalog and installed subplugin manifest problems. |
| `mare.plugins.doctor()` | `mare doctor` checks: git and its version, the lock, plugin names defined twice. |
| `mare.plugins.watched()` | Files whose change means "/reload to apply" (init.lua, plugins.json, a trusted .mare/init.lua). |
| `mare.plugins.set_enabled(name, on, source)` |  |
| `mare.plugins.fetch_catalog()` |  |
| `mare.plugins.mare_news()` |  |
| `mare.plugins.dirs(kind)` | The installed packages' `skills/`, `prompts/`, `themes/` or `types/` directories (or custom paths declared in `mare-plugin.toml`, or package roots when `kind` is omitted), in load order (`context` reads skills and prompts, `theme-default` reads themes). Empty under `--no-user-plugins` (`mare.no_user_plugins`): no package loads, nor its content. |

## mare.policy

Defined in `types/mare/config.lua`.

| | |
|---|---|
| `mare.policy.voter(point, fn, opts)` | Vote on a decision point (`tool/before`, `tool/approval`, …) from init.lua, as a plugin's `ctx.policy:voter`: return nil to abstain, `"allow"`, `"ask"`, `"deny"`, or a vote table. Dropped and re-registered on `/reload config`. |

## mare.proc

Defined in `types/mare/proc.lua`.

| | |
|---|---|
| `mare.proc.shell(opts)` | Resolve the shell for commands. Unix: `shell_path`, $SHELL (bash/zsh), /bin/bash, bash, sh. Windows (pi's order): `shell_path` → %ProgramFiles%\Git\bin\bash.exe → %ProgramFiles(x86)%\Git\bin\bash.exe → bash.exe on PATH; the error lists every location checked. `model = true`: the default for model commands under the current sandbox — PowerShell inside the Windows default-tier sandbox (Git Bash cannot start there) unless `shell_path` is set. |
| `mare.proc.powershell()` | PowerShell 7 (pwsh) or Windows PowerShell with `-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`; Windows only. |
| `mare.proc.wsl_path(p)` | `C:\x\y` → `/mnt/c/x/y` (default WSL mount root only); nil for other paths. |
| `mare.proc.windows_path(p)` | `/mnt/c/x/y` → `C:\x\y`; nil outside `/mnt/<drive>`. |
| `mare.proc.alive(pid, start_time)` | Whether a process with `pid` currently exists (and matches `start_time` if both are known). |
| `mare.proc.start_time(pid)` | Start time token of `pid` (defaults to the current process), or nil when unavailable. |
| `mare.proc.spill_write(prefix, path, data, append)` | Write or append to a spill file the caller keeps: `<state>/spill/<prefix>-*.log`, the caller naming its own prefix (`[a-z0-9-]+`, e.g. "bash"). Any other path or prefix is refused. |
| `mare.proc.spill_remove(prefix, path)` | Remove a spill file the caller keeps (`<state>/spill/<prefix>-*.log`). |
| `mare.proc.env(opts)` | Scrubbed parent environment inherited by spawned children. |
| `mare.proc.spawn(opts)` |  |
| `mare.proc.run(opts)` | `sandbox_failed`/`sandbox_marker` "[sandbox: failed to start (...)]" = the sandbox itself could not start (not a denial); `sandbox_denied`/`denial`/`sandbox_marker` = the sandbox refused the command. |
| `mare.proc.manager()` | Create an owner-scoped process manager backing `proc.*`. |
| `mare.proc.start(opts)` | Start a long-running process whose output goes to a bounded ring buffer. `spill` additionally receives the complete output. |
| `mare.proc.open(url)` |  |
| `mare.proc.which(name)` | The absolute path of an executable, or nil; starts no process (Neovim `exepath()`). A bare name is looked up in PATH (on Windows with the PATHEXT extensions, so `rg` finds `rg.exe`); a name containing a path separator is a path, relative to the working directory, and is returned only when it is an executable file. Plugins use it to offer a tool only when it is installed. |

## mare.proc.sandbox

Defined in `types/mare/proc.lua`.

| | |
|---|---|
| `mare.proc.sandbox.check(op, target, policy, cwd)` | Same rules as a sandboxed spawn. Inside a tool call core already enforces the current policy on `mare.fs`/`proc`/`http`/`net`; this is for asking ahead. |
| `mare.proc.sandbox.policy(spec, cwd)` | Normalized policy (resolved roots), nil when the spec means unsandboxed. |
| `mare.proc.sandbox.current()` | The session's current policy (enforced inside tool calls), nil = unconfined. |
| `mare.proc.sandbox.snapshot_current()` | Capture the current session policy after enforcing the inherited ceiling, revalidating roots and omitting only roots that have disappeared. Refused inside a tool call. |
| `mare.proc.sandbox.snapshot_policy(spec)` | Normalize and capture a policy against the host workspace and inherited ceiling, without applying it. Missing roots are reported separately; other validation errors are refused. Refused inside a tool call. |
| `mare.proc.sandbox.restore_snapshot(snapshot)` | Restore a captured native snapshot through the host setter. Revalidates the policy and inherited ceiling, reporting only genuinely missing roots; bypasses replaceable Lua setters. Also accepts a normal policy spec with the same normalization as set_current. Refused inside a tool call. The returned snapshot captures the applied policy. |
| `mare.proc.sandbox.root_missing(path)` | True only for a genuinely absent root. Permission errors and invalid roots are false; callers must not silently omit them. Refused inside a tool call. |
| `mare.proc.sandbox.preflight_protection(spec, cwd)` | Check whether per-workspace protected mounts can be prepared; may briefly create an owned empty mount point. |
| `mare.proc.sandbox.generation()` | Changes whenever the current policy changes (approvals are bound to it). |
| `mare.proc.sandbox.check_set(spec, cwd)` | Validate a policy spec against the active ceiling and persistent PTY fence without mutating the current session policy. Refused inside a tool call. |
| `mare.proc.sandbox.set_current(spec, cwd)` | Replace the current policy (takes effect from the next call); errors on an invalid spec. Prefer `ctx.session:set_sandbox_mode(spec)`, which also logs it in the session. |
| `mare.proc.sandbox.intersect(a, b, cwd)` | Per-dimension intersection of two policies (nil = the current policy): each dimension keeps only what both allow. An approval answer narrows the requested policy with this. |
| `mare.proc.sandbox.within(path, roots, opts)` | Whether `path` (resolved; a dangling symlink counts as outside) lies under one of `roots`. |
| `mare.proc.sandbox.marker(level)` | "[sandbox: file access denied under &lt;level&gt; mode]" |
| `mare.proc.sandbox.msys_failure(o)` | Set when a sandboxed command is an MSYS2/Cygwin program (Git Bash) and died with 0xC0000142 under a Windows mechanism: "SANDBOX_MSYS_UNSUPPORTED: …"; nil otherwise. |
| `mare.proc.sandbox.capabilities(cwd)` |  |

## mare.profile

Defined in `types/mare/profile.lua`.

| | |
|---|---|
| `mare.profile.mark(label)` | Add a point to the /profile timeline (e.g. a cache invalidation). |
| `mare.profile.report()` | The startup timeline and plugin load times. |
| `mare.profile.slow_callbacks()` | Callbacks over `slow_callback_record_ms`, newest last (128 kept). |
| `mare.profile.callback_timings(limit)` | Callback time per plugin and event, largest first. |

## mare.projection

Defined in `types/mare/text.lua`.

| | |
|---|---|
| `mare.projection.register(value)` | Register nonempty UTF-8 text; reject active duplicates and overlap with reserved references. Keep the lease alive; GC closes it. Close explicitly in a ctx:effect cleanup and on session switches. |
| `mare.projection.project(value, opts)` | Copy nested string VALUES, preserving table metatables. Cycles/depth&gt;64 and sensitive/reserved string keys fail closed. Functions/userdata are preserved opaque. Does not rewrite originals. Always returns a fresh copy. Non-UTF-8 string values fail unless opts.lossy (replace with U+FFFD, second result true when replaced) or opts.preserve_invalid (keep the bytes, projecting registered values inside their valid UTF-8 runs); the two are exclusive. Keys must be UTF-8. |
| `mare.projection.restore(value, revision)` | Restore only inside an execution context carrying a native projection_authorization, the approved approval_snapshot and matching projection_version. The capability permits only the exact projected arguments accepted by that human response, once. This is not a plaintext getter outside approved execution. Unknown/revoked/malformed references error. The tools service calls this only inside its approved execution context, after persistence and cancellation checks, then validates the restored arguments again. Custom execution services must implement equivalent approval/sandbox boundaries; this primitive alone grants no permission. |
| `mare.projection.revision()` |  |
| `mare.projection.active()` |  |
| `mare.projection.contains(value)` |  |
| `mare.projection.subscribe()` |  |
| `mare.projection.stream()` |  |

## mare.prompt

Defined in `types/mare/config.lua`.

| | |
|---|---|
| `mare.prompt.section(def)` | Add (or replace) a system-prompt section. |
| `mare.prompt.remove(name)` | Remove a system-prompt section by name (global and agent-specific variants). |

## mare.pty

Defined in `types/mare/proc.lua`.

| | |
|---|---|
| `mare.pty.capabilities()` | Discover enforced transport budgets and optional platform foreground capabilities. |
| `mare.pty.spawn(opts)` | Run a program in a pseudo-terminal (interactive tools). Per-call policy elevations are refused because their authority must not escape into later input calls. While a PTY tree is alive, sandbox policy changes raise PTY_POLICY_BUSY; close/stop terminals before switching policy. Inside a confined tool call (or with `sandbox`) the program is confined like `mare.proc`: Seatbelt / bubblewrap wrap it inside the PTY; Landlock (and bubblewrap's Unix-socket filter) apply in the terminal child before exec; on Windows the sandbox runner owns the ConPTY and bridges it. |

## mare.sandbox

Defined in `types/mare/sandbox.lua`.

| | |
|---|---|
| `mare.sandbox.complete()` | Return the opaque host-callback sentinel that ends the current sandbox run successfully. |
| `mare.sandbox.new(opts)` | Create a bounded Lua capability VM backed by a dedicated worker process. |
| `mare.sandbox.output_ledger(max_bytes)` | Create a bounded JSON output ledger with an exact `max_bytes` budget (4..16777216). |

## mare.schema

Defined in `types/mare/schema.lua`.

| | |
|---|---|
| `mare.schema.compile(schema)` | Compile an object-root JSON Schema into a reusable tool argument validator. |
| `mare.schema.compile_output(schema)` | Compile a tool output JSON Schema (or clone an existing `OutputSchema`). |
| `mare.schema.compile_value(schema)` | Canonical outputs may be any JSON type; does not relax argument-schema validation. |
| `mare.schema.redact(schema, value)` | Schema-directed removal, independent from key/pattern-based mare.text.redact. Structural JSON Schema subset: properties, additionalProperties schema, items, allOf/anyOf/oneOf. Annotate a secret node with ["x-mare-secret"]=true. Every union branch is visited conservatively; duplicate paths merge set=true. Object slots include missing ones; record/array slots include only existing entries. JSON null is set=true; Lua nil is unset. Array removals leave JSON null preserving zero-based string indexes in sidecar paths. Does not mutate input. Schema depth 64/nodes 10000; unsupported constructs (including $ref/conditionals) and malformed annotations error rather than silently exposing values. |

## mare.semver

Defined in `types/mare.lua`.

| | |
|---|---|
| `mare.semver.compare(a, b)` | Pure SemVer 2.0.0 precedence comparison (`-1`, `0`, `1`), ignoring build metadata; returns `nil` when either version string is malformed. |
| `mare.semver.is_newer(candidate, current)` | Pure SemVer 2.0.0 precedence check: whether `candidate` has strictly greater precedence than `current` (e.g. `1.0.1-rc.1` &gt; `1.0.0`). |
| `mare.semver.is_newer_release(candidate, current)` | Release-channel update policy: whether `candidate` should be offered as an update over `current` under SemVer 2.0.0 precedence (prerelease → stable notifies; stable never upgrades to a prerelease). |

## mare.snapshot

Defined in `types/mare/fs.lua`.

| | |
|---|---|
| `mare.snapshot.configure(opts)` |  |
| `mare.snapshot.begin_turn(id)` |  |
| `mare.snapshot.mark_shell()` | Record that the active turn ran a shell command (whose file changes cannot be snapshotted). |
| `mare.snapshot.turns()` |  |
| `mare.snapshot.content(hash)` |  |
| `mare.snapshot.restore(turn, opts)` | Restore files touched in `turn` or later to their content before it. Files changed outside mare since are `conflicts` (left alone unless `force`); links now are `refused`; trimmed backups are `missing`. |

## mare.spill

Defined in `types/mare/spill.lua`.

| | |
|---|---|
| `mare.spill.save(session_id, text, opts)` |  |
| `mare.spill.read(spill_id, opts)` | Bounded UTF-8 pagination (max 2000 lines/50KiB). next_byte continues a giant line exactly. Ownership is checked against the active execution context's session. Inside a confined call, `opts.session_id` may only equal the active context session; passing a different session fails with `denied`. |
| `mare.spill.delete(spill_id, opts)` | Delete a session-owned spill object. Ownership is checked against the active session. Inside a confined call, `opts.session_id` may only equal the active context session; passing a different session fails with `denied`. |
| `mare.spill.save_bytes(session_id, bytes, opts)` | Persist raw bytes (including NUL/non-UTF-8); at most 8 MiB per call. Same spill_ids, owner checks, permission policy, cancellation cleanup and explicit delete as text save. |
| `mare.spill.read_bytes(spill_id, opts)` | Read a byte page without text decoding. Cursor is zero-based, beyond EOF is refused. Default limit 1 MiB, maximum 8 MiB. EOF returns empty bytes, more=false. Text read still rejects non-UTF-8; byte read works for both text and binary output. No image/MIME inference. Inside a confined call, `opts.session_id` may only equal the active context session; passing a different session fails with `denied`. |
| `mare.spill.new_local(opts)` | Construct an owned local backend, using the same text/binary I/O and permissions. Relative root resolves against the host session cwd, fixed at construction. cleanup_period_days defaults to 30; integer 0 disables cleanup. A one-shot background sweep removes only strictly expired managed private regular files, never follows links, preserves active directories and reports failures as warnings without blocking saves. Maintenance freezes its originating policy and optional cancellation token; it never reads a later wider global policy. Supply the operation token when constructing inside cancelable work; an available execution-context cancel token is inherited by default. Reads never start maintenance. Call dispose from the owning context's ctx:effect cleanup; cached methods then reject calls. Reconstructing the same root permits reading its surviving session-owned spill objects. |

## mare.state

Defined in `types/mare/state.lua`.

| | |
|---|---|
| `mare.state.read()` | The whole state (`{}` when missing); nil + error when the file is corrupt. |
| `mare.state.update(fn)` | `fn(state)` edits the table in place or returns a replacement; must not yield. Existing JSON nulls are passed as `mare.json.null` so unchanged values survive an update. |
| `mare.state.set(key, value)` | Set (nil removes) one key. |
| `mare.state.snapshot(request)` | Read one isolated private namespace under &lt;state&gt;/domains, without modifying legacy state.json. Namespace names are 1..128 ASCII letters/digits/._-, first character alphanumeric, never paths or authority. Null and empty object/array identities are preserved; malformed/unknown versions reject. No callbacks, resources or handles survive this call. Caller read restrictions still apply. |
| `mare.state.compare_exchange(request, options)` | Atomically replace a namespace at its exact revision, with temp fsync + rename + directory fsync. Conflict performs no write. Deletion requires value=mare.json.null and retains a revision tombstone. Values are strict JSON; functions, arbitrary userdata/metatables, cycles and unsafe numbers reject. Ephemeral-read-only mutation and deny_read contexts reject; no namespace grants permissions. Cancellation/drop during lock wait or before publication rejects; the final check after temp fsync admits the irreversible publication phase (CAS visibility linearizes at rename). After it, cancellation cannot promise rollback. unknown_state means publication may have happened: reread/reopen and reconcile before continuing, never pretend the old value is intact. Plugin unload aborts the owner future; the worker retains only detached JSON and a child cancel token. |

## mare.status

Defined in `types/mare/events.lua`.

| | |
|---|---|
| `mare.status.hook(def)` | Run a command when the unified state changes ("status"), on session start/resume/fork/switch ("session") and on exit ("exit", awaited with a timeout). The state goes to stdin as JSON and into MARE_STATE, MARE_MESSAGE, MARE_SESSION_ID, MARE_SESSION_PATH, MARE_CWD, MARE_SESSION_MODEL. "status" runs are coalesced (at most one per throttle_ms, latest state wins) and skipped when state and message are unchanged; a run is killed after timeout_ms; failures are only logged. |

## mare.store

Defined in `types/mare/store.lua`.

| | |
|---|---|
| `mare.store.open(path, opts)` | Open a session for writing: a non-blocking flock on `<path>.lock` (re-checked against the inode at that path) first, then strict validation of the current format — all under the lock. Unsupported versions are refused unchanged. Fails with "session locked by another process", "session is already open in this process" (a second open here is refused until the first handle is closed) or "session file is corrupt" (file untouched). A torn final entry is cut before the first append. With a resume checkpoint in the session index (length + mtime), only the part after it is validated when it still holds — an optimization that assumes only mare appends; `mare doctor` parses every session in full. Appends are plain writes; `sync = true` (semantic boundaries) fsyncs, and a failed write or fsync truncates back to before it. `append_batch` writes several entries at once; `checkpoint()` records a checkpoint (also done on close); `close()` invalidates the handle immediately. An active snapshot read retains the session lock until that read and any deferred cleanup finish. `existing = true` (resume): a missing file fails with "session file not found" and nothing is created — a stale path never becomes a new, headerless session. The first ordinary handle becomes the active journal; after opening another ordinary handle, `activate()` selects it when the session switches. Switching or closing the active journal discards its pending acknowledgements; with ordinary handles open but none active, the gate refuses calls. Open unrelated/child transcripts with `auxiliary = true`: they neither activate nor gate execution and cannot acknowledge a call. On the active journal, `append(entry, { ack = tool_call_id })` (also supported by `append_batch`) requires a matching entry with `type = "tool_state"`, `state = "started"`, and `tool_call_id = tool_call_id`; it implies `sync`. The host admits that call into `mare.exec.run` once after the write and fsync succeed. `defer_activation = true` opens an ordinary journal without activating it, even when it is the first. `create_new = true` refuses an existing path and creates atomically. |
| `mare.store.read_bound(path, lenient)` | Read a path-bound snapshot (optionally lenient), retaining the identity of the observed source rather than reopening it after selection. Refused inside a tool call. |
| `mare.store.read_fork_snapshot(path, expected_session_id)` | Read a strict fork snapshot independent of the source writer/lock, optionally requiring the observed session header identity. Refused inside a tool call. |
| `mare.store.read_snapshot(file, revision)` | Read through an independently opened handle verified against the locked writer. An optional revision must belong to this handle and still match its stamp/generation. The native read retains the session lock through deferred cleanup. Refused in tool calls. |
| `mare.store.read_tail_snapshot(file)` | Capture a stable tail and revision from the same locked writer observation. A subsequent full read can use its revision to reject source mutation. Refused inside a tool call. |
| `mare.store.snapshot_matches(file, revision)` | Check both the exact handle identity and its current journal generation/file stamp. A revision from another file returns false. Refused inside a tool call. |
| `mare.store.reserve_staging_attachments(path)` | Exclusively reserve the attachment directory for an unpublished session path. The stage owns cleanup until its owner and every pending write release it. Refused in tool calls and ephemeral read-only sessions. No existing directory is reused or overwritten. |
| `mare.store.read_attachment(journal, name, max_bytes)` | Read a session attachment constrained to `<session>.attachments/` without following directory or file symlinks, bounded by `max_bytes` and verified against its SHA-256 name. |
| `mare.store.read(path, opts)` | Read all entries, lock-free, up to the end as of the call; an unterminated final entry (still being written, or torn) never counts. Strict: any other unparseable line is an error ("session file is corrupt"). `lenient = true` (read-only views of a damaged file) skips bad lines and returns how many. Never migrates or rewrites the file. The file is stat'ed (inode, length, mtime) before and after; if it changed it is read once more — `stable = false` means it still changed, so the result may be incomplete. |
| `mare.store.list(dir)` |  |
| `mare.store.files(path)` | Every file of a session: the session file, its lock file, attachments, snapshot journal and sub-agent sessions (`<dir>/agents/<run>/` naming it as parent). |
| `mare.store.delete(path)` | Delete a session and everything in `files(path)` under its lock: refused ("session locked by another process" / "already open in this process") while it is open. The index is the caller's: `index_append({session_id = session_id, path = path, deleted = true})` (never rebuild it). |
| `mare.store.dir(cwd)` |  |
| `mare.store.read_from_compaction(path)` | Reverse read back to the latest compaction (and the message it keeps from). Strict and read-only like `read`. |
| `mare.store.read_tail(path, n)` |  |
| `mare.store.search(path, query)` |  |
| `mare.store.index_append(entry)` | After appending to a session file, bring its index row up to date with the file: every field is derived from the file (a catch-up parses only what was appended), `cwd` records where a live writer runs. The row moves to the top when it gained messages; a `name`, `label` or `archive` entry changes its column only. `{session_id, path, deleted = true}`: the session was deleted, drop its row (and its sub-agents'). Multi-process safe. |
| `mare.store.index_read(opts)` | Sessions from the index, newest first (by `seq`); built from the session files on first use. When the index can't be opened the session files are scanned instead (slower, logged). `scope`: "all" (default), "cwd" (`cwd`, default `mare.cwd`), "project" (the same `project_root`); `archived`: false (default), true, "any"; `kind`: "session" (default: main sessions and forks), "main", "fork", "agent", "any"; `nonempty`: only sessions with messages; `session_id`, `session_id_prefix`, `parent_session_id`: exact filters; `limit`, `offset`: one page. Returns the page and how many rows match in all. |
| `mare.store.index_warn_unsupported(paths)` |  |
| `mare.store.index_reconcile()` | Make the index agree with the session files: unchanged files are only stat'ed, changed ones caught up, unknown ones added, rows of deleted files dropped (the session plugin runs it in the background at startup). |
| `mare.store.index_rebuild()` | Re-derive every row from the session files in one transaction (order and resume checkpoints kept). Returns the row count. |
| `mare.store.index_notice()` | Why the index had to be rebuilt in this process (a damaged file or a schema version change), once; nil when it wasn't or the reason was handed out. |
| `mare.store.file_contains(path, query)` |  |
| `mare.store.project_items(entries)` | Project session journal entries to message and tool-call items; control records are omitted. |
| `mare.store.search_sessions(opts, execution)` | Cwd-scoped policy-bound history, ONLY in tool execution context. Cwd comes from the call. Directory and each candidate must satisfy frozen read policy; denied metadata is omitted. No host index. Native session/agents-run containers only, never attachments or links. At most 10000 readable files/directories, nesting 32; headers &lt;=64 KiB. Selected raw snapshots &lt;=32 MiB total / 8 MiB each. Exceeding a budget is QUERY_BUDGET_EXCEEDED, never silent omission. Session/time filters run on authorized headers first, so a narrowed retry can fit the budget. Message text literal FTS5 phrase (unicode61), 1..512 chars (or title_query); query syntax remains inert data. Match-count descending, text-length ascending, time descending, stable id/seq tiebreaks; each session returns its strongest event. Pages default to 20 results. Opaque cursor binds corpus revision, query and filters; any observed corpus change rejects old cursors as stale. incomplete marks skipped sources. |
| `mare.store.search_events(opts, execution)` | Same authority/bounds/corpus revision as search_sessions; all matching events in one session. seq is the zero-based position in the strict complete-entry snapshot (header is zero). text_offset is a Unicode character offset in the original flattened message text. |
| `mare.store.search_items(opts, execution)` | Search semantic message and tool-call items in one session under the same history policy. |
| `mare.store.read_event(opts, execution)` | Exact raw event at an opaque event_id from a hit, plus bounded neighbor summaries. before/after each 0..10. Single event &gt;64 KiB is refused inline (QUERY_EVENT_TOO_LARGE); pass offset/max_bytes for UTF-8 bounded chunked reads. Corpus revision changes reject old event ids; missing/other-cwd/unreadable targets share errors. Does not load or switch sessions. No child-agent traversal. No persistent query cache. |
| `mare.store.read_item(opts, execution)` | Read a semantic item in one authorized session; event tokens are not item IDs. |
| `mare.store.trace_item(opts, execution)` | Trace the journal surface of a semantic item using its session-scoped ID. |
| `mare.store.trace_session(opts, execution)` | Genuine persisted/live lineage from authorized V3 header.parent_session_id, never directory inference. Only visible headers enter the projection. An unavailable parent is an anonymous boundary, no hidden parent id/title/path. complete describes the known parent chain, not a global corpus. Max 1000 nodes / descendant depth 32; over-budget/cycles/source changes fail explicitly. |
| `mare.store.trace_event(opts, execution)` | Event surface projection and lineage trace (compaction replacement chains, branch summaries, tool call/result links) for one event by event_id or (session_id, seq). |

## mare.stream

Defined in `types/mare/stream.lua`.

| | |
|---|---|
| `mare.stream.buffer()` |  |
| `mare.stream.coalescer()` |  |

## mare.subagent

Defined in `types/mare/subagent.lua`.

| | |
|---|---|
| `mare.subagent.start(opts)` | Low-level host primitive: start an independent child agent VM + thread directly. Distinct from the `subagent` contract service's `ctx.subagent:start`, which layers agent type lookup, tool ceilings, and session fork snapshots on top of this primitive. Root config `agents.max_active` (default 16) bounds all live descendant threads, including startup/shutdown; `agents.max_depth` (default 4) bounds edges from the root (zero disables children). Descendants cannot override these limits. Admission errors carry structured codes (`subagent.limit_reached`, `subagent.tree_closed`, `subagent.duplicate_name`, `invalid_arguments`). cancel() cancels this child and its descendants. Host shutdown closes admission and waits for all descendant cleanup; cancellation is cooperative, not forced OS-thread termination. The sub-agent's prompt (its VM's agent layer): `persona` replaces the identity (interpolated), `sections` adds or shadows sections ({name, order, text, tag?, raw?, interpolate?, complete?}, text a string) for this sub-agent only, `system` is the whole prompt (verbatim). `params` are the sub-agent's model-parameter selections (its agent layer, above its config and below its own session): on the parent's current model it starts with the parent session's values, `effort` = `params.effort`, `params` wins. The parent's parameter rules (`mare.params{}`, `mare.model{ params = }`) reach it with their Lua functions. In a saved, writable parent session, the host creates and writes a child transcript and assigns its identity. This is a message transcript, not a recoverable tool-state journal: child mode retains its no durable-start/ack gate and no crash-recovery semantics. `run_id` and `seq` are optional grouping metadata, not binding credentials. An ephemeral or read-only parent does not record a child transcript. A direct tool call against an as-yet-unsaved writable parent fails; an ordinary agent turn saves the parent before it starts its tools. |
| `mare.subagent.drain_session()` | Close admission for the current session and wait for its children and lifecycle events. |
| `mare.subagent.tool_allowed(name)` | Whether the host's inherited tool ceiling permits this tool name. |
| `mare.subagent.list()` | List subagents spawned by the current parent session. |
| `mare.subagent.get(id)` | Get a subagent by subagent_id or unique name in the current parent session. |
| `mare.subagent.wait(id, opts)` | Wait for a subagent to finish (non-consuming; timeout/cancel does not cancel the child). |
| `mare.subagent.steer(id, text)` | Steer a running subagent. |
| `mare.subagent.cancel(id)` | Cancel a subagent by subagent_id or name. |
| `mare.subagent.identity(subagent_id)` | A host-owned binding in the current parent session. For display association only. |
| `mare.subagent.uncarried(value)` | What of a config value would not reach a sub-agent's VM: "&lt;function site&gt;: &lt;reason&gt;" for each function inside it that can't be carried (it captures a userdata, a thread or a built-in the globals don't reach). `mare doctor` uses it for the parameter rules. |
| `mare.subagent.take_rule(token)` | The sub-agent rule the parent's approval dialog showed and the user chose to remember, once, by the `rule_token` of its `pipeline/approval-decided` event (the permissions plugin persists it). |
| `mare.subagent.rule_lift(call, info)` | In a sub-agent sharing its parent's permission configuration (the tools service calls it): lift the inherited ceiling for this allowed call when the parent's rules allow it too. |
| `mare.subagent.permission_lift(call)` | Apply host-owned scoped grants to a frozen call's ceiling. No caller-supplied grant is trusted. |
| `mare.subagent.permission_commit(call, permissions, scope)` | Commit the intersection of a pending host approval and the effective permissions. No matching pending approval means no grant; turn/session lifetime remains host-owned. |

## mare.task

Defined in `types/mare/task.lua`.

| | |
|---|---|
| `mare.task.spawn(fn, ...)` |  |
| `mare.task.sleep(ms, cancel)` |  |
| `mare.task.channel(capacity)` |  |
| `mare.task.token()` |  |
| `mare.task.select(...)` | Run functions concurrently; abort losing branches and wait for their Lua tasks to finish before returning the first branch's index and results. Cancelling select requests abort of every branch. A losing branch's cancellation or closing error does not replace the winner. Synchronous code and closing hooks can delay cleanup; separately spawned tasks, blocking work, processes and remote side effects may continue after the Lua task ends. |
| `mare.task.timeout(ms, fn, ...)` | On timeout, closes the callback's Lua scope before returning false, including in a tool context. Independently spawned work and blocking operations can still outlive the callback. |
| `mare.task.now()` |  |
| `mare.task.call_sync(fn, ...)` | Synchronous call in the same execution context; yielding is an error. |
| `mare.task.publish(fn, ...)` | Host-only synchronous publication. A thrown error or yield poisons readiness before returning. Does not roll back callbacks/IO; prepare everything before publishing. |
| `mare.task.fail_session(reason)` | Host-only unrecoverable publication diagnostic; blocks future admission until rebuilt. |
| `mare.task.session_error()` | Current unrecoverable session publication diagnostic, or nil when healthy. |
| `mare.task.spawn_completion(ctx, fn)` | Start a completion task owned by ctx. Registration failure poisons the Host synchronously. Refused in tool contexts, including full-access/inherited tools. |
| `mare.task.clock()` |  |
| `mare.task.id()` | Identifier of the running async task, or nil outside a Tokio task. |
| `mare.task.is_token(value)` |  |
| `mare.task.yield()` |  |

## mare.test

Defined in `types/mare/test.lua`.

| | |
|---|---|
| `mare.test.host(opts)` | Create an isolated real plugin host (Cordis tree, loader, event bus, and contract admission). Any test host not explicitly disposed is disposed automatically when its test case finishes. |
| `mare.test.describe(name, fn)` | Group related spec cases under a named suite (may be nested). |
| `mare.test.it(name, fn)` | Define a single spec test case. Cases within the same spec file share an isolated file-level Lua environment (use `before_each` / `after_each` to reset shared state). |
| `mare.test.before_each(fn)` | Register a hook that runs before each test case in the current suite and its nested suites. |
| `mare.test.after_each(fn)` | Register a hook that runs after each test case in the current suite and its nested suites. |

## mare.test.expect

Defined in `types/mare/test.lua`.

| | |
|---|---|
| `mare.test.expect.eq(actual, expected, msg)` | Assert that `actual` equals `expected` (deep structural equality for tables). |
| `mare.test.expect.ne(actual, unexpected, msg)` | Assert that `actual` does not equal `unexpected` (deep structural equality for tables). |
| `mare.test.expect.truthy(value, msg)` | Assert that `value` is truthy (neither `nil` nor `false`). |
| `mare.test.expect.falsy(value, msg)` | Assert that `value` is falsy (`nil` or `false`). |
| `mare.test.expect.error(fn, pattern, msg)` | Assert that calling `fn` raises an error, optionally matching Lua pattern `pattern`. |
| `mare.test.expect.match(str, pattern, msg)` | Assert that string `str` matches Lua pattern `pattern`. |
| `mare.test.expect.snapshot(value, name)` | Assert that `value` matches the stored snapshot beside the spec file (`snapshots/<spec_stem>__<name>.snap`). When `name` is omitted, it is derived from the current test case name. |
| `mare.test.expect.settled(host, msg)` | Assert that every loaded plugin in `host` has settled cleanly (all loaded plugins are `"ACTIVE"` with no failures or pending dependencies). |

## mare.text

Defined in `types/mare/text.lua`.

| | |
|---|---|
| `mare.text.redact(value)` | Redact credential-looking strings and sensitive keys in tables recursively. |
| `mare.text.regex(pattern)` |  |
| `mare.text.truncate(s, opts)` |  |
| `mare.text.cut(s, n, opts)` | The first (`keep = "head"`, default) or last `n` units of `s`, never splitting a UTF-8 sequence. `unit = "bytes"` (default): at most `n` bytes, the cut moved inward to a character boundary; `unit = "chars"`: `n` code points (raw bytes when `s` isn't valid UTF-8). |
| `mare.text.tokens(s)` | The one token estimate (compaction, /context, providers): a string is bytes / 4; a table is its JSON size with each inline image counted as `image_tokens`, not its base64 payload, plus any wire-declared estimation hints registered via `register_estimate_hint`. |
| `mare.text.register_estimate_hint(name, hint)` | Register namespaced (`<namespace>:<id>`) token-estimation hints for a wire format; returns an idempotent disposer function. |
| `mare.text.yaml(s)` | Parse the first document of a YAML 1.2 stream (yaml-rust2) into Lua values. |
| `mare.text.frontmatter(text)` | A leading `---` YAML block (SKILL.md, prompt templates). BOM and CRLF are normalised; a block that isn't valid YAML is read as `key: value` lines and `warning` says why. |
| `mare.text.width(s)` |  |
| `mare.text.trunc_width(s, w)` |  |
| `mare.text.sanitize(s)` |  |
| `mare.text.fuzzy(query, items, limit)` |  |
| `mare.text.closest(word, candidates)` | Nearest candidate to `word` within a small optimal-string-alignment edit distance, or nil. `candidates` may be a string list or a table whose string keys are candidate names. |
| `mare.text.replace(content, old, new, all)` |  |
| `mare.text.patch_paths(patch, opts)` | Parse patch paths without opening any of the named files. With `opts.resolve = true`, relative paths are resolved literally against cwd (matching `apply_patch`). |
| `mare.text.apply_patch(patch, opts)` |  |
| `mare.text.diff(old, new, opts)` | Unified diff of two texts (`context` lines around each hunk, default 3; `a/<path>` and `b/<path>` headers), with the added and removed line counts (as `diff_stats`) and the longer side's line count (`ui.Diff{unified=, lines=}`) from the same diff. |
| `mare.text.diff_stats(old, new)` |  |
| `mare.text.split_lines(s, from, to)` | Lines of `s` (`\n` or `\r\n` ends one; a final line break adds no empty line) and their count. With `from` only lines from..to (1-based, inclusive, `to` default the last) are returned, so a window of a big text builds only its own strings; `total` counts all. |
| `mare.text.is_utf8(s)` | Whether `s` is valid UTF-8 (what strict `utf8.len` accepts), checked in Rust. |
| `mare.text.shell_split(cmd)` | POSIX-shell word splitting (nil when quoting is malformed) and whether the command uses compound syntax outside quotes (`;` `&` `\|` `<` `>` newline, `$(…)`, backticks). Command rules must never match a compound command by its first words. |
| `mare.text.shell_parse(cmd, opts)` | Parse a shell command into plain-word segments joined by `&&` `\|\|` `;` `\|`. `analyzable = false` (with `reason`) for substitutions, expansions, subshells, redirections other than fixed sinks (`/dev/null`, `2>&1`), `eval`, nested shells beyond one `bash -c '…'` level, syntax errors, unknown syntax or an exhausted budget. `script` is the inner script of a `bash -lc '…'` / `pwsh -Command '…'` wrapper. PowerShell uses a conservative lexer (plain words, `\|`, `;` only). `safe_sinks`: extra fixed redirect targets that keep a command analyzable (e.g. "/dev/stderr"). Each segment's `unwrapped` is for conservative denial/prompt matching, never allow rules. `literal_paths` contains only statically identifiable argv words; it is not a complete inventory of the paths a script can access. |
| `mare.text.dangerous(cmd, opts)` | Built-in dangerous-command rules: the rule a command hits ("forced_rm": rm -f/--force or find -delete anywhere, through sudo/env/trap/ bash -c/heredocs/command/exec/nohup/timeout/xargs/find -exec; a script too long to parse or with a syntax error falls back to a plain word scan, so it fails closed; "windows_force_delete": Remove-Item -Force, del /f, rd /s /q — on Windows or for PowerShell), or nil; plus every literal command in the script (argv lists) so plugins can add their own. |
| `mare.text.retain(s, opts)` | UTF-8 safe head/tail preview; suffix is kept intact inside both budgets. Errors if it cannot fit. |
| `mare.text.decode(bytes, charset, opts)` | Strictly decode at most 5 MB using a declared WHATWG charset (default UTF-8). Unknown charsets and malformed sequences raise an error; no replacement characters. |
| `mare.text.html_to_markdown(html, opts)` | Convert HTML on a worker thread, skipping script/style/head/template/noscript and embedded content. Parsed hidden/aria-hidden, hidden inputs and inline display/visibility remove their subtrees. External stylesheets/computed CSS are not evaluated. Hidden/template nodes still count toward work limits. max_tags bounds parse complexity (100000); DOM traversal checks max_nodes (50000) and max_depth (128) before recursive conversion. Ordinary wide pages are supported. Input and output ceilings are 5 MB and 1 MB; truncation preserves UTF-8. |

## mare.time

Defined in `types/mare/time.lua`.

| | |
|---|---|
| `mare.time.query(query)` | Synchronous, deterministic, bounded queries. Instants are integer epoch milliseconds in years 0001..9999; explicit local instants reject gaps/folds. Calendar/Cron/RRULE recurrence skips gaps and chooses earlier folds. Exhausted next_ms is mare.json.null (JSON null), never false or zero. RRULE is the bounded two-line DTSTART/RRULE profile: six frequencies, COUNT &lt;=1000, one BYSECOND, &lt;=10080 candidates per period, &lt;=31 effective BYSETPOS selectors; unsupported/duplicate fields, invalid combinations and unsafe integers reject. normalize applies the stricter new-rule authoring profile; next/resolve retain supported historical ineffective BYSETPOS rules. Cron normalization preserves the Vixie star flag. Errors return nil, diagnostic; no work is scheduled. No resource handles to dispose. Synchronous bounded calculation does not accept a cancellation token or yield mid-query. |

## mare.tool

Defined in `types/mare/config.lua`.

| | |
|---|---|
| `mare.tool.command(def)` | Register a tool that runs a command template. |
| `mare.tool.describe(name, text)` | Override the model-facing description of a tool. |

## mare.tools

Defined in `types/mare/config.lua`.

| | |
|---|---|
| `mare.tools.disable(names)` |  |

## mare.webserver

Defined in `types/mare/webserver.lua`.

| | |
|---|---|
| `mare.webserver.get()` |  |
| `mare.webserver.list_routes()` |  |
| `mare.webserver.list_assets()` |  |
| `mare.webserver.register_route(owner_name, path, opts)` |  |
| `mare.webserver.register_assets(owner_name, dir, opts)` |  |

## mare.workspace

Defined in `types/mare/workspace.lua`.

| | |
|---|---|
| `mare.workspace.dirs()` | Return the active workspace directories (primary root first). |
| `mare.workspace.check(path, peers)` | Validate a candidate additional workspace directory (existing physical absolute directory, no protected-path or deny_read overlap, within the active sandbox ceiling). Returns the canonical path, or `nil, err`. |
| `mare.workspace.set_dirs(dirs)` | Atomically replace the additional workspace directories (the primary root stays first). Validates every candidate root (same rules as `add`) before mutating; rejects all-or-nothing. |
| `mare.workspace.add(dir)` | Add a workspace directory (resolved via `mare.fs.resolve`); returns true if newly added. |
| `mare.workspace.remove(dir)` | Remove an added workspace directory (the primary root cannot be removed). |
| `mare.workspace.contains(p)` | Whether path `p` lies inside any active workspace directory. |

## ui

Defined in `types/mare/ui.lua`. Also reachable as `mare.ui.*` (`mare.ui = ui`).

| | |
|---|---|
| `ui.Span(text, style)` |  |
| `ui.Line(spans)` |  |
| `ui.Paragraph(x, indent)` | Word-wrapped line; continuation lines get `indent`. |
| `ui.Lines(items)` |  |
| `ui.Markdown(text)` |  |
| `ui.Code(code, lang)` |  |
| `ui.Diff(t)` | `{old=, new=, path=}` diffs two texts (3 lines of context); `{unified=}` renders an existing unified diff, with `path` (highlighting for every row instead of the `+++` headers) and `lines` (the longer side's line count, for the gutter width) it renders a 3-line-context `mare.text.diff` exactly like `{old=, new=, path=}` of the same texts. |
| `ui.Indent(first, child, rest)` |  |
| `ui.Gap(n)` |  |
| `ui.Spinner(label)` |  |
| `ui.Frames(frames, ms, still)` | A frame animation the core plays by wall-clock time (`still` under reduce_motion). |
| `ui.Row(items)` | Nodes side by side on one row (the first line of each). |
| `ui.Clip(child, max, tail)` |  |
| `ui.Block(child, opts)` | Bordered panel: title in the top border, footer (`3 of 42`) in the bottom one. `border_style` is a named style (MareBorder / MareBorderActive / MareBorderSearch); the charset follows `mare.o.border` unless `border` is given. |
| `ui.List(items, opts)` | Virtually scrolled list: only `height` rows around `selected` (1-based) are drawn; inside a Block it gets a scrollbar. Selection = full-width MareSelection when focused, bold otherwise. |
| `ui.Split(left, right, opts)` | Two panes (list + preview); stacked when narrower than `stack_below` (default 84) columns. |
| `ui.Segments(list, side)` | Status capsules (Yazi): Powerline caps with a Nerd Font, padded background otherwise. |
| `ui.Gauge(frac, width)` |  |
| `ui.icon(name)` | Glyph for the active icon set (`mare.o.icons`): file, dir, command, branch, model, jobs, workflow, info, warn, error, sep_left, sep_right. Sets live in `ui.icons[set][name]`. |
| `ui.detect_icons(caps)` | The icon set for `icons = "auto"`; `caps` defaults to `mare.ui.caps()` (`emoji_wide = false` → "ascii"). |
| `ui.color_depth()` | Terminal color depth: "truecolor" \| "256" \| "16" \| "none" (env MARE_COLOR overrides; NO_COLOR gives "none"). |
| `ui.set_color_mode(option)` | Recompute and apply the terminal color depth from `option` (or `mare.o.terminal.color`), environment variables, and probed capabilities, returning the active depth. |
| `ui.KeyHints:render(hints)` | The status bar while an overlay is open. |
| `ui.Tabs:render(names, active, title)` | A panel's views as its border title (`Running │ All`). |
| `ui.Select:frame(s)` | The frame of a list overlay; the core adds the rows, the search text and the count. |
| `ui.Confirm:render(c)` | The yes / no dialog, drawn `c.width` cells wide (normally a ui.Block). |
| `ui.Logo:lines()` |  |
| `ui.Logo:render(info)` |  |
| `ui.Logo:compact(info)` |  |
| `ui.icon_set()` |  |
| `ui.set_theme_data(opts)` | Update interface-shell theme & glyph data (omitted fields stay unchanged; `glyphs = false` restores the built-in spinner glyphs). |
| `ui.Working:render(info)` |  |
| `ui.image(opts)` | An image node: the terminal's protocol (Kitty / iTerm2 / sixel) or a one-line placeholder; fitted into max_rows × max_cols cells (defaults 16 × 80). |
| `ui.filetype_style(path)` | Named style for a path by file type (Yazi filetype; rules in `ui.filetypes`). |
| `ui.file_icon(path)` |  |
| `ui.Checklist(items)` |  |
| `ui.connected()` |  |
| `ui.can_respond()` | Whether this frontend currently has an interactive responder (including RPC UI hosts). |
| `ui.display_path(path, max_width)` | A directory for display: home as `~`, longer than `max_width` cells → `…/last/components` (the rule the welcome line, the status bar and the trust question share). 0/nil = no limit. |
| `ui.width()` |  |
| `ui.height()` |  |
| `ui.push(kind, t)` | Push a transcript block; returns its id. kind: "user"\|"assistant"\|"reasoning"\|"tool"\|"info"\|"error"\|... |
| `ui.update(id, t)` |  |
| `ui.append(id, text)` | Streaming markdown append (fast path). |
| `ui.finish(id)` | Mark a block finished: it is committed to terminal scrollback in order. |
| `ui.remove(id)` |  |
| `ui.print(node)` |  |
| `ui.set_widget(id, slot, node, order)` |  |
| `ui.set_status(id, align, node, order, keep)` |  |
| `ui.working(label, detail, opts)` |  |
| `ui.notify(text, level, opts)` |  |
| `ui.notification_history()` | Recent TUI notifications, oldest first (up to 200 entries). |
| `ui.select(t)` | Items may carry a `preview` node (shown in a pane next to the list); `tabs` gives tabbed views (switch with Tab or [ ]); `kind = "confirm"` draws a centered [Y]es [N]o dialog. `body` (a string or a list of lines) is shown above the options (e.g. the trust overlay). `multi = true` returns an array of checked 1-based indices in the active tab: Space toggles the highlighted item and Enter confirms. While typing a search, Space is part of the filter; move to a result to resume toggling. Esc clears search before cancelling. `accept_label` changes the accept hint (an empty string hides it); `allow_search = false` disables searching. `keys` adds action keys (shown in the hint bar): pressed while not searching they return the highlighted item plus that key (e.g. `w` = write to file in /copy's picker); `"ctrl+d"` keys also work while searching (so letters can always search). An item with `header = true` is a group heading: shown, never selected, hidden while searching. The search is fuzzy; `re:<regex>` and `"exact phrase"` (case-insensitive) narrow it. `vim = true`: j/k (with a count prefix), g/G and h/l (tabs) move when typed before a search; ←/→ switch tabs anyway. `on_highlight(index, tab) -> node\|nil` builds the preview of the highlighted item lazily. `title_right` (a string, drawn muted, or a `ui.Line`) ends the top border, e.g. the current sort and filter; a narrow box shortens it, then leaves it out (the title always wins). `split = { ratio, min }`: on a wide terminal (84 columns and more) with a preview pane, the list takes `ratio` of the popup (default 0.4), at least `min` columns (default 24); the preview keeps at least 20. The 4th result is the search text then (re-open a picker after an action without losing it). A `filter` given up front opens in searching mode: the first Esc clears the search, the next one closes the select. `on_open(sel)` is called once the select is open with its handle: `sel:update(items)` replaces the items in place while the user is in it (an index that caught up, a list still loading), keeping the search text, the searching state and the highlighted item — found again by the item's `item_id`, else by its label. The index the select returns refers to the latest items. Dismissal returns `nil, "cancelled"`; Ctrl+C returns `nil, "interrupted"`. A `cancel` token returns `nil, "cancelled"`. When no UI responder is available, returns `nil, "unavailable"`. Ctrl+C cannot be declared in `keys`. |
| `ui.approve(t)` | Dedicated trusted human approval prompt: native code renders the title, bound arguments, and fixed choice/key mapping, returning the chosen decision and a one-time native receipt. |
| `ui.input(t)` | Returns `nil, "interrupted"` for Ctrl+C, `nil, "unavailable"` when no UI responder is available, or `nil, "cancelled"` for other dismissals and `cancel`. |
| `ui.editor(t)` | Returns `nil, "interrupted"` for Ctrl+C, `nil, "unavailable"` when no UI responder is available, or `nil, "cancelled"` for other dismissals and `cancel`. |
| `ui.questions(t)` | Returns `{ answers = {}, dismissed = true, reason = "cancelled"\|"interrupted" }` when dismissed, `nil, "cancelled"` when cancelled via `cancel`, or `nil, "unavailable"` when no UI responder is available. |
| `ui.overlay(title, node, opts)` |  |
| `ui.transcript(title, node, opts)` | Full-screen transcript view (Ctrl+T style) showing `node`. Calling it again with the same title while open refreshes it in place (keeps the scroll position, follows the end). `opts.keys`: the view's own keys for its header, e.g. `{ { action = "agents.back", label = "list" }, { action = { "agents.prev", "agents.next" }, label = "agent" } }` → `Esc list · Alt+←/Alt+→ agent · ↑/↓ PgUp/PgDn …`: the keys bound to those transcript-layer actions now (an action without a key is left out), so a rebind changes the header. |
| `ui.transcript_title()` | Title of the plugin transcript view while it's open, else nil. |
| `ui.transcript_close(title)` | Close the plugin transcript view (e.g. Esc in a child's view going back to a list); with `title`, only while it shows that title. The Ctrl+T transcript is left alone. Commands keep their order: `ui.transcript_close(t); ui.select{…}` opens the list on the main screen. |
| `ui.set_editor(text, opts)` | `keep_draft = true`: leave a non-empty input alone (the user's draft wins; /tree, /fork, /undo). `prepend = true` restores before any newer draft. `images` contains attachment blocks in marker order; colliding image markers are remapped while preserving the newer draft. |
| `ui.get_editor()` |  |
| `ui.editor_state()` |  |
| `ui.editor_cursor()` |  |
| `ui.set_editor_cursor(row, col)` |  |
| `ui.editor_selection()` |  |
| `ui.set_editor_selection(anchor, head)` |  |
| `ui.editor_insert(text)` |  |
| `ui.editor_replace(range, text)` |  |
| `ui.flag(name, value)` | Frontend display switch: "show_reasoning" (full reasoning instead of "Thought for N s"), "expand_all" (the Ctrl+O compact/expanded view). |
| `ui.set_queued(n)` | How many messages wait in the agent queue (with an empty input, ↑ emits `input/queue-select` and Enter emits `input/queue-menu`). |
| `ui.set_queue_mode(mode)` | The queue editing state the key handler routes by: 0 nothing selected, 1 a queued message selected (↑/↓ emit `input/queue-move`, Enter `input/queue-edit`, Delete `input/queue-remove`, Esc `input/queue-cancel`), 2 one being edited (Esc emits `input/queue-cancel`). |
| `ui.editor_hint(text)` |  |
| `ui.keymap()` | The input's key bindings, `{ key = action }` (each key's topmost binding; `mare.keymap.set` changes them). |
| `ui.normalize_key(key)` | The canonical name of a key or key sequence ("&lt;C-t&gt;" -&gt; "ctrl+t", "&lt;C-x&gt;g" -&gt; "ctrl+x g"). |
| `ui.theme(colors)` |  |
| `ui.clear()` |  |
| `ui.exit(opts)` | Leave mare. Without `force`, an exit guard (`ui.exit_guard`) gets to ask first. |
| `ui.exit_guard(on)` | While on, a user's exit (quit keys, /exit) emits `app/exit-requested` instead of quitting; the guard's owner calls `ui.exit{ force = true }` once confirmed (the jobs plugin does this while background jobs run). |
| `ui.render_text(node, width)` |  |
| `ui.style(name, spec)` | Set (or with one argument, read) a named style. |
| `ui.styles()` |  |
| `ui.theme_style(name, spec)` | Set a theme default (not recorded as an explicit override; themes use this). |
| `ui.style_overrides()` | Explicit `mare.ui.style` calls, which win over any theme. |
| `ui.reset_style(name, spec)` | Drop the explicit override of `name` and restore `spec` as its default. |
| `ui.override(name, fn)` | Wrap or replace `select`, `input`, `confirm`, or `notify`; layers stack (`next` delegates down) and `release()` removes this layer while preserving lower ones. |
| `ui.confirm(prompt, default, opts)` | Returns true for Yes, false for No, or nil and the dismissal reason (`"cancelled"`, `"interrupted"`, or `"unavailable"`). |
| `ui.editor_config(t)` |  |
| `ui.renderer(kind, r)` | Register a renderer. kind: "tool:&lt;name&gt;", "message:user", "message:assistant", "message:reasoning". |
| `ui.get_renderer(kind)` | The topmost renderer registered for `kind`, or nil. |
| `ui.entry_renderer(kind, r)` |  |
| `ui.get_entry_renderer(kind)` |  |
| `ui.markdown_transform(fn, opts)` |  |
| `ui.has_markdown_transforms(message_type)` |  |
| `ui.transform_markdown(text, context)` |  |
| `ui.UserMessage:render(text)` |  |
| `ui.TurnEnd:render(info)` |  |
| `ui.AssistantMessage:marker()` |  |
| `ui.Reasoning:render(text, secs)` |  |
| `ui.Reasoning:expanded(text, secs)` |  |
| `ui.Notice:render(text, level)` |  |
| `ui.ToolCall:verb(name, state)` | The tool's verb for `state`: present while running, past when done; nil when it has none. |
| `ui.ToolCall:header(summary, state, source)` |  |
| `ui.ToolCall:body(node)` |  |
| `ui.ToolCall:result_text(text, is_error, expanded)` |  |
| `ui.ToolCall:render(call, result, expanded, info)` |  |
| `ui.ToolCall:running(label)` | The head of a running call: a spinner, or a steady pending dot when `still` is set. |
| `ui.ToolCall:has_diff(node)` | Whether `node` is, or holds, a diff (edit / write / apply_patch results). |
| `ui.status(id, spec)` | Register or update a status bar segment; returns a release function. |
| `ui.status_redraw(id)` | Redraw one registered status segment (`id`) or all of them (`nil`). |
| `ui.Editor:apply()` |  |
| `ui.EditorChrome:render(s)` |  |
| `ui.NoticeCard:render(p)` |  |
| `ui.CompletionPopup:render(p)` |  |
| `ui.WhichKey:render(p)` |  |
| `ui.HelpModal:frame(s)` |  |
| `ui.InfoModal:frame(s)` |  |
| `ui.InputModal:frame(s)` | Secret inputs use a protected core frame and do not call this method. |
| `ui.SelectRow:render(item)` |  |
| `ui.notify_os(text, opts)` | Desktop notification (OSC 9 / 777 / kitty 99, tmux & screen passthrough, BEL fallback); returns false when suppressed. |
| `ui.set_title(text)` | Set the terminal window title (OSC 2); no-op without a TUI. |
| `ui.focused()` |  |
| `ui.caps()` | Terminal capabilities: static table + startup probe (DECRQM 2026/2027, XTGETTCAP, kitty keyboard, cursor position) + the `terminal_caps` option + `ui/caps` listeners. `width_method` is the width rule ("unicode" = grapheme clusters, "wcwidth" = code point by code point; set it through `terminal_caps` or `MARE_WIDTH_METHOD`); `emoji_wide = false` switches `icons = "auto"` to the ASCII set. |
| `ui.slow_frames()` | Frames that took longer than 16ms, newest last (128 kept): {at_ms, ms, what}. |
| `ui.syntax_theme(document, validate_only)` | Validate and select a tmTheme XML document; nil restores background-based syntax defaults. Invalid or &gt;1 MiB documents throw without changing the active document. |
| `ui.markdown_blocks(text)` | Native Markdown parser boundaries, shared by live/replayed code presentation. Indented code and inline code are not fenced blocks. This function does not invoke renderers or IO. |
| `ui.PresentedMarkdown(text, blocks)` | Pure cached presentation node. Invalid/duplicate block offsets are rejected; absent offsets render the original code card. Images use the existing frontend image protocol. The caller must retain source fallback when an artifact or image capability is absent. |
| `ui.validate_node(node)` | Validate conversion to a cached frontend node. Returns false for nil/unsupported empty descriptors, throws for invalid typed fields; performs no rendering or filesystem IO. |
| `ui.snapshot_node(node)` | Bounded immutable component-description copy, preserving binary image bytes. No callbacks, userdata or cycles; &lt;=64 nesting levels/50000 entries/32MiB content. Does not render or do IO. |
| `ui.node_to_wire(node)` | Existing component descriptions as JSON-safe wire values. Images require bytes (&lt;=8MiB each), become canonical padded base64 `data`, and omit local paths. Plain text must be UTF-8. Empty component arrays retain their array identity. No closures, handles or terminal IO. |
| `ui.node_from_wire(wire)` | Decode that same description into Lua component tables. Image data is strictly bounded canonical base64; incoming raw bytes/local image paths are refused. No image validity claim. |
| `ui.image_id()` | Allocate a nonzero native image occurrence id (fails on exhaustion). ui.image allocates once when omitted. Keep the id with that logical component across redraws; create a distinct id per occurrence even for identical pixels. Content encoding caches remain shared. Caller scope/document disposal removes components; ids are not authorization or durable artifact handles. Wire ids belong to the source frontend namespace; node_from_wire allocates fresh local ids, so keep the decoded description across redraws. |
| `ui.set_layout(layout, opts)` | Configure the stacked region layout (`ui.layout`). Errors on invalid region names, missing `editor`/`status`, duplicate regions, or invalid region options, keeping the previous layout. Pass `{ persist = false }` to apply the layout without writing `mare.o.ui.layout`. |
| `ui.set_viewport(width, height)` | Set the frontend viewport dimensions (each in `1..=10000`). Returns `true` if either dimension changed. |
| `ui.set_ambiwidth(mode)` | Set the width rule for East Asian ambiguous characters (`"single"` or `"double"`). |
| `ui.layout_revision()` | Opaque process layout revision; changes on theme/style/width-rule writes. Pure observation, not a session identity. Used to coalesce presentation invalidation without polling or IO. |
| `ui.image_resolver(namespace, load)` | Register a scoped, asynchronous image-byte resolver (64 namespaces maximum). Callback executes in a fresh ephemeral read-only execution. Native workers queue at most 16 pending requests per resolver; reads settle within 30s. Disposal cancels loads and evicts that registration's materialized bytes. Re-registration has a new identity. |
| `ui.image_resource(namespace, key, width, height)` | Create a small immutable resource description without reading storage. key must include the storage owner/session_epoch; the resolver must enforce actual storage authorization. Wire descriptions omit native resolver identity and rebind only to a local namespace. |
| `ui.image_cache_info()` |  |

## ui.Confirm

Defined in `types/mare/ui.lua`. Also reachable as `mare.ui.*` (`mare.ui = ui`).

| | |
|---|---|
| `ui.Confirm.label(s)` |  |

## ui.TurnEnd

Defined in `types/mare/ui.lua`. Also reachable as `mare.ui.*` (`mare.ui = ui`).

| | |
|---|---|
| `ui.TurnEnd.duration(secs)` | `secs` as "42s" or "3m 05s". |
| `ui.TurnEnd.tokens(n)` | A token count as "512", "1.2k" or "3.4M". |

## ui.completion

Defined in `types/mare/ui.lua`. Also reachable as `mare.ui.*` (`mare.ui = ui`).

| | |
|---|---|
| `ui.completion.register(spec)` |  |
| `ui.completion.collect(ev)` |  |
| `ui.completion.list()` |  |
