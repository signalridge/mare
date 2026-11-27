---@meta _
-- The `compact` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean, integer = S.object, S.string, S.boolean, S.integer
local structured_error = object({
	code = { type = "string", description = "Machine-readable error code." },
	message = { type = "string", description = "Human-readable error message." },
	data = { description = "Optional structured diagnostic details." },
}, { "code", "message" })
local run_result = object({
	status = {
		type = "string",
		enum = { "compacted", "nothing_to_compact", "unchanged", "summarized" },
	},
	tokens_before = integer,
	tokens_after = integer,
	summary = string,
	dropped = integer,
}, { "status", "tokens_before", "tokens_after" })
return {
	["compact"] = {
		doc = "Context compaction: the staged pipeline, its measurement and its stages.",
		errors = {
			not_compacted = {
				doc = "compaction failed (busy, cancelled, invalid history, or a failing stage); `kind` distinguishes `cancelled` or `invalid_history` when known",
				data = object({ kind = string }),
			},
		},
		methods = {
			prepare_request = {
				doc = "Measure a prepared request and compact its uniquely mapped session history when needed. "
					.. "The budget uses the protocol-independent request and the output cap from its resolved parameter mappings; "
					.. "later `provider/request` changes to the output cap are not included.",
				expose = "lua",
				optional = true,
				lua_unchecked = true,
				history_result = true,
				params = object({
					request = { type = "object", additionalProperties = true },
					history = { type = "array", items = { type = "object", additionalProperties = true } },
				}, { "history" }),
				lua = { request = "mare.LlmRequest" },
				order = { "request", "history" },
				result = object({
					messages = { type = "array", items = { type = "object", additionalProperties = true } },
					compacted = boolean,
				}, { "messages", "compacted" }),
			},
			run = {
				doc = "Compact the session's context now (`reason`: manual by default); "
					.. "`instructions` adds a focus to the summary. Returns `{status, tokens_before, tokens_after}` "
					.. '(or `{status = "summarized", tokens_before = 0, tokens_after = 0, summary, dropped}` with `summarize_only`). '
					.. "A failure raises `compact.not_compacted`.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({
					instructions = string,
					reason = { type = "string", enum = { "manual" } },
					summarize_only = boolean,
					messages = { type = "array", items = { type = "object", additionalProperties = true } },
				}),
				order = { "instructions", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil" },
				errors = { "not_compacted" },
				result = run_result,
			},
			register_stage = {
				doc = "Add a stage or replace a built-in one; order comes from the `stages` option. "
					.. "Withdrawn when the caller is disposed or the returned disposer is called.",
				registration = true,
				lua_unchecked = true,
				params = object({ name = string, summary = boolean }, { "name" }),
				lua = { fn = "async fun(state: table): boolean, string|nil" },
				order = { "name", "fn", "..." },
			},
			measure = {
				doc = "Current usage: the larger of the local estimate and provider-reported usage "
					.. "(or a per-source breakdown table when `{ by_source = true }`).",
				lua_unchecked = true,
				params = object({ by_source = boolean }, nil, true),
				order = { "..." },
				result = { oneOf = { integer, { type = "object", additionalProperties = true } } },
				lua_result = "integer|table",
			},
			cancel = {
				doc = "Stop a running compaction.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = object({}),
			},
			threshold = {
				doc = "Tokens at which auto compaction triggers for a model (default: current); nil when `auto` is off or budgets are invalid.",
				optional = true,
				lua_unchecked = true,
				params = object({}),
				lua = { model = "mare.Model?" },
				order = { "model" },
				result = { type = { "integer", "null" } },
			},
			busy = {
				doc = "Whether compaction or a branch summary is currently running.",
				optional = true,
				params = object({}),
				order = {},
				result = boolean,
			},
			offload_images = {
				doc = "The offload stage on its own, for a provider's image_limit error.",
				optional = true,
				lua_unchecked = true,
				params = object({ keep = integer }),
				order = { "..." },
				result = { type = { "integer", "null" } },
				lua_result = { "integer|nil", "string|nil" },
			},
		},
		events = {
			["approaching"] = {
				doc = "Context token usage crossed the near-threshold warning watermark.",
				mode = "emit",
				expose = "lua",
				data = object({
					tokens = { type = { "integer", "number" } },
					threshold = { type = { "integer", "number" } },
				}, { "tokens", "threshold" }),
			},
			["before"] = {
				doc = "Pre-compaction bail hook before starting a compaction pass.",
				mode = "bail",
				expose = "process",
				data = object({
					reason = string,
					tokens = { type = { "integer", "number" } },
					threshold = { type = { "integer", "number" } },
				}, { "reason", "tokens" }),
			},
			["idle"] = {
				doc = "Compaction became idle after clearing or settling state.",
				mode = "emit",
				expose = "process",
				data = object({}),
			},
			["overflow"] = {
				doc = "Request overflow compaction recovery after a context-overflow response.",
				mode = "waterfall",
				expose = "process",
				emitters = { "agent" },
				data = object({}, nil, true),
				result = object({
					handled = boolean,
					succeeded = boolean,
				}, nil, true),
			},
			["stage"] = {
				doc = "Best-effort compaction pipeline stage diagnostic.",
				mode = "emit",
				expose = "both",
				delivery = "lossy",
				ignorable = true,
				data = object({
					stage = string,
					phase = string,
					tokens = { type = "number" },
					changed = boolean,
					tokens_before = { type = "number" },
					tokens_after = { type = "number" },
					reason = string,
				}, { "stage", "phase" }, true),
				example = { stage = "prune", phase = "before", tokens = 100.0 },
			},
			["summary-error"] = {
				doc = "Compaction summarization failed; listeners may supply an alternate summary.",
				mode = "waterfall",
				expose = "process",
				data = object({
					error = string,
					kind = string,
					messages = { type = "array", items = { type = "object", additionalProperties = true } },
				}, { "error", "kind", "messages" }, true),
				result = object({ summary = string }, nil, true),
			},
			started = {
				doc = "Context compaction started.",
				mode = "emit",
				expose = "both",
				scope = "session",
				delivery = "required",
				ignorable = false,
				on_error = "continue",
				data = {
					type = "object",
					properties = {
						compact_id = { type = "string" },
						reason = { type = "string" },
						messages = { type = "integer" },
					},
					required = { "reason" },
				},
				example = { compact_id = "c-1", reason = "manual", messages = 4 },
			},
			completed = {
				doc = "Context compaction completed (succeeded, cancelled, failed, or stalled).",
				mode = "emit",
				expose = "both",
				scope = "session",
				delivery = "required",
				ignorable = false,
				on_error = "continue",
				data = {
					type = "object",
					properties = {
						compact_id = { type = "string" },
						reason = { type = "string" },
						status = { type = "string", enum = { "completed", "cancelled", "failed", "stalled" } },
						stalled = { type = { "boolean", "null" } },
						pruned_count = { type = { "integer", "null" } },
						offloaded_count = { type = { "integer", "null" } },
						tokens_before = { type = "number" },
						tokens_after = { type = "number" },
						changed = { type = "boolean" },
						error = structured_error,
						cancelled = { type = { "boolean", "null" } },
						compactions = { type = { "integer", "null" } },
						message = { type = { "string", "null" } },
					},
					required = { "reason", "status", "tokens_before", "tokens_after", "changed" },
				},
				example = {
					compact_id = "c-1",
					reason = "auto",
					status = "completed",
					tokens_before = 100,
					tokens_after = 60,
					changed = true,
				},
			},
		},
	},
}
