-- llm: provider registry, model catalog, model parameters (effort …), request retry.
--
-- Message format (provider-agnostic-style):
--   user: {role="user", content={ {type="text", text=...}, {type="image", data=, mime=} }}
--   assistant: {role="assistant", content={ {type="reasoning", text=, redacted=, native=}, {type="text", text=, citations=, native=},
--                  {type="tool_call", id=, name=, arguments={...}, hosted=, result=, native=} }, usage={input,output,cache_read,cache_write},
--                  stop_reason="stop"|"tool_use"|"length"|"error"|"aborted", error=, model_id=, provider_id=, native=}
--   tool_result: {role="tool_result", tool_call_id=, tool_name=, content={ {type="text", text=} }, is_error=}
--
-- Provider contract: provider.stream(req, sink) -> assistant message
--   req = {model, system, messages, tools, effort, cancel, session_id, max_tokens, native}
--   sink = function(ev) with ev.type in text_delta | reasoning_delta | tool_call_start | tool_call_delta | hosted_tool_*
--   errors: error({message=, retryable=bool, status=}) (a plain string is non-retryable)

-- `max` is above xhigh (DeepSeek V4: low|high|max); a request is clamped to the model's own
-- `efforts`, so a model that declares max gets max, never high
local EFFORTS = { "off", "minimal", "low", "medium", "high", "xhigh", "max" }

-- idle budget multiplier per effort: high-effort
-- turns think silently for minutes
local IDLE_MULT = { high = 2, xhigh = 3, max = 4 }
local EFFORT_INDEX = {}
for i, e in ipairs(EFFORTS) do
	EFFORT_INDEX[e] = i
end

return {
	name = "llm",
	-- in the minimal plugin set
	tags = { "minimal" },
	config_api = {
		--- mare.model { provider = "openai", id = "my-model", context_window = ..., cost = {...} }
		--- `alias = "fast"` also names it: `/model fast`, `--model fast`, `fallback_models`
		---
		model = function(C, def)
			assert(type(def) == "table" and def.provider and def.id, "mare.model needs provider and id")
			C.models = C.models or {}
			local ref = def.provider .. "/" .. def.id
			C.models[ref] = C.models[ref] or {}
			for k, v in pairs(def) do
				if k ~= "provider" and k ~= "id" and k ~= "alias" then
					C.models[ref][k] = v
				end
			end
			if type(def.alias) == "string" and def.alias ~= "" then
				C.model_aliases = C.model_aliases or {}
				C.model_aliases[def.alias] = ref
				-- an alias with plain parameter values pins them: `/model glm-deep` = the model with
				-- effort deep
				local pinned
				for k, v in pairs(type(def.params) == "table" and def.params or {}) do
					if type(v) ~= "table" then
						pinned = pinned or {}
						pinned[k] = v
					end
				end
				if pinned then
					C.model_alias_params = C.model_alias_params or {}
					C.model_alias_params[def.alias] = pinned
					local defs = {}
					for k, v in pairs(def.params) do
						if type(v) == "table" then
							defs[k] = v
						end
					end
					C.models[ref].params = next(defs) and defs or nil
				end
			end
		end,
		--- mare.params { match = "provider/glob" | provider = | family = | route =, define = {...},
		---   remove = {...}, on_unsupported = "clamp" | "error" | "as_is" } — model parameters and
		--- their wire mappings.
		--- A trusted project's .mare/init.lua rules are the project layer.
		params = function(C, rule)
			assert(type(rule) == "table", "mare.params needs a table")
			local r = {}
			for k, v in pairs(rule) do
				r[k] = v
			end
			r._layer = mare.config_origin().layer == "project" and "project" or "user"
			r._where = mare.caller_site and mare.caller_site(1) or nil
			C.param_rules = C.param_rules or {}
			C.param_rules[#C.param_rules + 1] = r
		end,
		--- mare.provider(name, { base_url = ..., api_key = "ENV_NAME" | { cmd = {...} } | "literal",
		---   headers = { name = value (same forms) }, query_params = {...}, retries = n,
		---   stream_idle_timeout_ms = n, connect_timeout_ms = n, wire_api = "responses" | "chat",
		---   login_hint = "...", proxy = "http://..." }) —. A name that isn't a builtin
		--- provider (with a base_url) defines a new one: two accounts of one service are two names.
		provider = function(C, name, opts)
			C.providers = C.providers or {}
			C.providers[name] = C.providers[name] or {}
			for k, v in pairs(opts or {}) do
				C.providers[name][k] = v
			end
		end,
	},
	provide = { "llm" },
	-- `task` gives ctx:spawn, `bus` the mare events,
	-- `policy` its request/ready voter; session is read when present (mare_lib.follow)
	inject = { "task", "bus", "policy" },
	config_schema = {
		retries = { type = "integer", default = 5, desc = "Retries of a failed request." },
		-- backoff 0.5s doubling to an 8s cap; a server asking to wait
		-- longer than retry_after_max_ms fails at once (clamping only hits the 429 again)
		retry_base_ms = { type = "integer", default = 500, desc = "First retry backoff." },
		retry_max_ms = { type = "integer", default = 8000, desc = "Largest retry backoff." },
		retry_after_max_ms = {
			type = "integer",
			default = 60000,
			desc = "A server asking to wait longer than this fails at once.",
		},
		-- 529 / overloaded: retried, but fewer times (Anthropic usually recovers quickly)
		overloaded_retries = { type = "integer", default = 3, desc = "Retries of a 529 / overloaded response." },
		-- no bytes at all for this long (heartbeats count) → transport error; scaled by effort.
		-- The same budget bounds the wait for response headers
		stream_idle_timeout_ms = {
			type = "integer",
			default = 300000,
			desc = "No bytes at all for this long (heartbeats count) is a transport error.",
		},
		-- size caps on SSE streams: a line, or an event's lines together, longer than this is a
		-- malformed_response (not retried); the idle watchdog alone never ends an endless line
		sse_max_line_bytes = {
			type = "integer",
			default = 16 * 1024 * 1024,
			desc = "Longest SSE line; longer is a malformed_response.",
		},
		sse_max_event_bytes = {
			type = "integer",
			default = 16 * 1024 * 1024,
			desc = "Longest SSE event (its lines together); longer is a malformed_response.",
		},
		crash_limit = {
			type = "integer",
			default = 3,
			desc = "A request that crashed mare this many times is not sent again.",
		},
		-- model catalogue: providers with `list_models` have their listed models cached
		-- in <state>/models-cache/<provider>.json; read at registration, refreshed in the
		-- background after ui/ready when older than this; the static table stays the fallback
		catalog_refresh_ms = {
			type = "integer",
			default = 24 * 3600 * 1000,
			desc = "Age after which a provider's cached model list is refreshed in the background.",
		},
		catalog_open_refresh_ms = {
			type = "integer",
			default = 10 * 60 * 1000,
			desc = "Opening /model refreshes the configured providers' lists older than this.",
		},
		-- model metadata (price, input modalities, reasoning, limits) merged into listed models
		-- fetched with the catalogue, trimmed to the registered providers and cached
		-- in <state>/models-cache/models.dev.json; false turns it off
		models_dev = {
			type = "boolean",
			default = true,
			desc = "Merge models.dev metadata (price, modalities, reasoning, limits) into listed models.",
		},
		models_dev_url = {
			type = "string",
			default = "https://models.dev/api.json",
			desc = "Where the models.dev metadata is fetched from.",
		},
	},
	apply = function(ctx, cfg)
		local providers = {}
		local models = {}
		local overrides = (mare.config.models or {})
		local state = {
			model = mare.config.model,
			default_model = mare.config.model,
			-- parameters chosen in this session, per model ref (`llm:set_param`)
			params = {},
			-- state.json `model_params` (remembered per model, below init.lua's values)
			remembered = type(mare.config.model_params) == "table" and mare.config.model_params or nil,
			-- named parameter override scopes (e.g. mode-specific reasoning effort)
			active_scope = nil,
			scoped_params = {},
			scoped_cleared = {},
			scoped_remembered = type(mare.config.scoped_model_params) == "table" and mare.config.scoped_model_params
				or nil,
			-- models picked with set_model, most recent first (state.json `recent_models`)
			recent = type(mare.config.recent_models) == "table" and mare.config.recent_models or {},
		}

		-- shared built-in helpers; runtime choices (model, effort) persist to state.json through
		-- lib.save_state, never written back to init.lua
		local lib = require("mare_lib")

		local llm = {}
		function llm.efforts(_, _)
			return table.copy(EFFORTS)
		end

		-- structural submodules (plugins/lua/llm/), loaded here in dependency order (llm.prepare
		-- and llm.readiness stay lazy)
		local STARTUP_DONE_KEY = "llm.startup_done"
		local D = {
			ctx = ctx,
			cfg = cfg,
			providers = providers,
			models = models,
			overrides = overrides,
			state = state,
			lib = lib,
			llm = llm,
			EFFORT_INDEX = EFFORT_INDEX,
			EFFORTS = EFFORTS,
			IDLE_MULT = IDLE_MULT,
			startup_done = mare.g[STARTUP_DONE_KEY] == true,
		}
		-- optional services read on each call
		D.follow = { session = lib.follow(ctx, "session") }
		local ensure_errors = lib.lazy(function()
			local mod = require("llm.errors")
			D.errors = mod
			return mod
		end)
		D.errors = setmetatable({}, {
			__index = function(_, k)
				return ensure_errors()[k]
			end,
		})
		local ensure_history = lib.lazy(function()
			local mod = require("llm.history")
			D.history = mod
			return mod
		end)
		D.history = setmetatable({}, {
			__index = function(_, k)
				return ensure_history()[k]
			end,
		})
		--- Compile a simple `*`/`?` glob pattern into an anchored Lua pattern.
		local function compile_glob(p)
			return "^" .. p:gsub("[%^%$%(%)%%%.%[%]%+%-]", "%%%0"):gsub("%*", ".*"):gsub("%?", ".") .. "$"
		end
		--- Matching `{ pat, value }` entries in `map` for `model`, ordered from shortest to longest
		--- pattern (lexicographical tie-break so equal lengths choose deterministically).
		function D.matching_model_globs(map, model, case_insensitive)
			if type(map) ~= "table" or type(model) ~= "table" then
				return {}
			end
			local ref = model.ref or (model.provider and model.id and (model.provider .. "/" .. model.id)) or ""
			local id = model.id or ""
			local out = {}
			for pat, v in pairs(map) do
				if type(pat) == "string" then
					local target = tostring(pat:find("/", 1, true) and ref or id)
					local pat_cmp = case_insensitive and string.lower(pat) or pat
					local tgt_cmp = case_insensitive and string.lower(target) or target
					if tgt_cmp:match(compile_glob(pat_cmp)) then
						out[#out + 1] = { pat = pat, value = v }
					end
				end
			end
			table.sort(out, function(a, b)
				if #a.pat ~= #b.pat then
					return #a.pat < #b.pat
				end
				return a.pat > b.pat
			end)
			return out
		end
		D.side_calls = {}
		D.catalog = require("llm.catalog")(D)
		local ensure_send = lib.lazy(function()
			return require("llm.send")(D)
		end)
		function D.send(spec, endpoint, req, sink)
			return ensure_send()(spec, endpoint, req, sink)
		end
		D.routes = require("llm.routes")(D)
		D.params = require("llm.params")(D)
		D.directory = require("llm.directory")(D)
		D.models = require("llm.models")(D)
		D.directory.init()
		D.lazy_params_api = true
		function llm.define_params(_, caller, rule, layer)
			return D.params.define(caller, rule, layer)
		end
		local ensure_params_api = lib.lazy_group(llm, {
			"param_list",
			"resolve_params",
			"apply_params",
			"remember_param",
			"set_param",
			"cycle_param",
			"explain_params",
			"explain_lines",
			"note_param",
			"params_log",
			"subagent_params",
		}, function()
			return require("llm.params_api")(D)
		end, {
			extra_target = D,
			extra_fields = { "remember_scoped_param" },
			fast_unloaded = {
				params_log = function()
					return D.params.log()
				end,
			},
		})
		ctx:on("kernel/doctor", function(checks)
			return ensure_params_api().on_doctor(checks)
		end)
		lib.lazy_group(llm, {
			"classify",
			"transform_messages",
			"close_response",
			"http_error",
			"stream_error",
		}, function()
			ensure_errors()
			ensure_history()
			require("llm.api_errors")(D)
		end, { extra_target = D, extra_fields = { "with_response" } })
		lib.lazy_group(llm, { "constrain_tools" }, function()
			D.constrain_tools = require("llm.tool_constraints")(D)
		end, { extra_target = D, extra_fields = { "constrain_tools" } })
		function llm.constrain_request(_, _, req)
			return D.constrain_tools(req)
		end
		lib.lazy_group(llm, {
			"cancel_side_requests",
			"side_requests_pending",
			"fingerprint",
			"stream",
			"supports_remote_compact",
			"compact_remote",
			"complete",
			"text_of",
		}, function()
			ensure_errors()
			ensure_history()
			require("llm.stream")(D)
		end, {
			fast_unloaded = {
				cancel_side_requests = function(_, _, owner)
					for entry in pairs(D.side_calls) do
						if entry.owner == owner then
							entry.token:cancel()
						end
					end
				end,
				side_requests_pending = function(_, _, owner)
					for entry in pairs(D.side_calls) do
						if entry.owner == owner then
							return true
						end
					end
					return false
				end,
			},
		})
		local ensure_auth_store = lib.lazy_group(llm, { "auth_get", "auth_set" }, function()
			return require("llm.auth_store")(D)
		end)
		D.auth_store = {
			read_auth = function(...)
				return ensure_auth_store().read_auth(...)
			end,
			check_mode = function()
				return ensure_auth_store().check_mode()
			end,
			same = function(...)
				return ensure_auth_store().same(...)
			end,
		}
		D.api_keys = require("llm.api_keys")(D)
		local ensure_credentials = lib.lazy_group(llm, {
			"refresh_credential",
			"credential_sources",
			"active_credential",
			"status_lines",
			"has_credentials",
			"auth_status",
			"list_providers",
			"list_auth_keys",
			"auth_keys",
		}, function()
			return require("llm.credentials")(D)
		end, { extra_target = D, extra_fields = { "credential_scope", "how_to_login" } })
		D.credentials = {
			auth_keys = function(...)
				return ensure_credentials().auth_keys(...)
			end,
		}
		D.lazy_login = true
		local ensure_login = lib.lazy_group(llm, {
			"no_browser",
			"login_io",
			"identify_key_provider",
			"login_api_key",
			"login",
			"logout",
			"logged_in",
		}, function()
			return require("llm.login")(D)
		end)
		ctx:on("login/cli", function(req)
			return ensure_login().on_cli(req)
		end)
		local provider_failure = D.models.provider_failure

		-- Keep the warm request on the provider's route and out of the awaited ready event.
		-- The task belongs to this plugin and is cancelled when it is disposed.
		ctx:on("ui/ready", function()
			if mare.mode == "child" then
				return
			end
			ctx:spawn(function()
				pcall(function()
					local m = llm.current(llm, ctx)
					local p = m and providers[m.provider]
					local pc = m and D.api_keys.provider_cfg(m.provider) or {}
					local url = p and p.warm_url and p.warm_url()
					if url then
						-- Only the origin determines the pool route; userinfo, paths and queries
						-- may contain credentials and do not belong in a warmup request.
						local base_url = type(pc.base_url) == "string" and pc.base_url ~= "" and pc.base_url or url
						local scheme, authority = mare.http.build_url(base_url):match("^(https?)://([^/?#]+)")
						local host = authority and authority:match("([^@]+)$")
						if not scheme or not host then
							return
						end
						local response = mare.http.request({
							method = "HEAD",
							url = scheme .. "://" .. host,
							timeout_ms = 5000,
							proxy = pc.proxy,
							connect_timeout_ms = pc.connect_timeout_ms,
							redirect = "same-origin",
						})
						if response and response.close then
							response:close()
						end
					end
				end)
			end)
		end)

		local function mark_startup_done()
			mare.g[STARTUP_DONE_KEY] = true
			D.startup_done = true
		end

		ctx:on("app/ready", mark_startup_done)

		ctx:on("kernel/reloaded", mark_startup_done)

		ctx:on("ui/ready", function()
			mark_startup_done()
			if mare.mode == "child" then
				return
			end
			ctx:spawn(function()
				D.catalog.ensure_loaded()
				llm.refresh_catalog(llm, ctx, {})
			end)
		end)

		-- the selected model's readiness (the checks and their order are in
		-- llm/readiness): request/prepare lets its provider refresh credentials, request/ready
		-- refuses a prompt the model can't take. The steps' order is the readiness chain's
		-- (mare_lib.READY_ORDER).
		local ready_state
		local function readiness()
			ready_state = ready_state
				or require("llm.readiness").new({
					-- single slot: while this plugin runs, the llm service is its own
					active = function()
						return true
					end,
					current = function()
						return llm.current(llm, ctx)
					end,
					selected_model = function()
						return state.model
					end,
					unresolved_model = function()
						return D.models.unresolved_model()
					end,
					resolve = function(ref)
						return llm.resolve(llm, ctx, ref)
					end,
					ensure_loaded = function(name)
						return D.catalog.ensure_loaded(name)
					end,
					provider = function(name)
						return providers[name]
					end,
					provider_failure = provider_failure,
					warn_unlisted = function(m)
						return D.models.warn_unlisted(m)
					end,
					how_to_login = function(name)
						return D.how_to_login(name)
					end,
				})
			return ready_state
		end
		function llm.ready(_, _, model)
			local refusal = readiness().ready({ model = model })
			return refusal == nil, refusal and refusal.reason
		end
		-- The same provider preparation owner serves primary requests and side-model admission.
		-- This method owns its callback scope; a provider ignoring cancel cannot outlive it.
		function llm.prepare(_, _, model, opts)
			opts = opts or {}
			local timeout = opts.timeout_ms or 60000
			assert(
				math.type(timeout) == "integer" and timeout > 0 and timeout <= 3600000,
				"prepare timeout_ms must be a positive integer at most one hour"
			)
			local cancel = opts.cancel and opts.cancel:child() or mare.task.token()
			if cancel:is_cancelled() then
				return false, "model preparation cancelled"
			end
			local _, ready, reason = mare.task.select(function()
				readiness().prepare({ model = model, cancel = cancel })
				if cancel:is_cancelled() then
					return false, "model preparation cancelled"
				end
				return llm.ready(llm, ctx, model)
			end, function()
				local elapsed = mare.task.sleep(timeout, cancel)
				cancel:cancel()
				return false, elapsed and "model preparation timed out" or "model preparation cancelled"
			end)
			return ready, reason
		end
		-- the ordered readiness steps: llm owns the list;
		-- agent's preflight runs prepare_request, the one request/ready voter below runs the
		-- ready steps. llm's own step is the first (a token refresh waits up to 30 s for the shared
		-- auth lock, then 15 s for the server).
		local chain = require("llm.readiness_chain").new()
		function llm.readiness(_, caller, name, def)
			return chain.register(caller, name, def)
		end
		function llm.prepare_request(_, _, args)
			chain.prepare(args)
		end
		function llm.request_ready(_, _, args)
			return chain.ready(args)
		end
		chain.register(ctx, "llm", {
			order = lib.READY_ORDER.llm,
			timeout_ms = 60000,
			prepare = function(args)
				readiness().prepare(args)
			end,
			ready = function(args)
				return readiness().ready(args)
			end,
		})
		-- the one voter for the whole chain: first among the voters without a band, after the
		-- host's vote, ahead of every other plugin's and across reloads (prepend)
		-- (on mare.Ctx `policy` is still typed as the transition layer's method)
		local policy = ctx.policy --[[@as mare.PolicyService]]
		policy:voter("request/ready", function(req)
			return chain.ready(req)
		end, { prepend = true })

		-- mare.o.model =... takes effect immediately
		ctx:on("option/changed", function(ev)
			if ev.name == "model" and type(ev.value) == "string" and ev.value ~= state.model then
				local ok, err = pcall(llm.set_model, llm, ctx, ev.value)
				if not ok then
					mare.log("warn", "mare.o.model: " .. tostring(err))
				end
			end
		end)

		-- a mare.Service
		require("mare_lib").service(ctx, "llm", llm)
	end,
}
