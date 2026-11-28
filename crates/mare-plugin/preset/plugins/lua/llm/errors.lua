-- llm.errors: provider error classification (limits / retryable / fatal kinds, status codes);
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
-- ---- error classification ------------------------------------------------
-- One classifier for every provider, decided in order: user cancel → non-retryable limits →
-- retryable → context (compaction). Providers map their own error codes onto `kind`; anything
-- unclassified falls back to the status and the message wording below.

-- non-retryable account limits: structured codes or billing wording, never the bare word "quota"
-- (a per-minute token quota is a rate limit)
local LIMIT_WORDS = {
	"insufficient_quota",
	"exceeded_current_quota_error",
	"exceeded your current quota",
	"out of budget",
	"quota exceeded",
	"credit balance",
	"available balance",
	"monthly usage limit reached",
	-- Chinese account balance / quota exhaustion
	"余额不足",
	"欠费",
	"账户余额",
	"额度已用完",
	"额度不足",
	"配额不足",
	"配额已用完",
	"套餐额度",
	"资源包已用完",
}
local RETRY_WORDS = {
	"overloaded",
	"server_busy",
	"servers are currently busy",
	"currently experiencing high demand",
	"model is at capacity",
	"rate limit",
	"rate_limit",
	"ratelimit",
	"rate increased too quickly",
	"slow_down",
	"too many requests",
	"request rate increased too quickly",
	"allocated quota exceeded",
	"temporarily unavailable",
	"service unavailable",
	"server error",
	"server_error",
	"internal error",
	"internal_error",
	"provider returned error",
	"upstream request failed",
	"upstream_stream_read_error",
	"stream was interrupted",
	"http2 stream failed",
	"http/2 stream failed",
	"network error",
	"network_error",
	"connection error",
	"connection refused",
	"connection reset",
	"connection lost",
	"connection closed",
	"other side closed",
	"try your request again",
	"fetch failed",
	-- Chinese transient rate limits
	"限流",
	"请求过于频繁",
	"访问过于频繁",
	"频率限制",
	"速率限制",
	"并发超限",
	"并发数超限",
	"超出tpm",
	"超出tps",
	"超出rpm",
	"超出qps",
}
-- status codes as whole words only
local RETRY_STATUS_WORDS = { "408", "409", "429", "500", "502", "503", "504", "520", "524", "529" }

local FATAL_KINDS = {
	quota = true,
	usage_limit = true,
	invalid = true,
	refusal = true,
	fatal = true,
	model_not_found = true,
	auth = true,
	malformed_response = true,
	payload_too_large = true,
	retry_delay_too_long = true,
	-- too many / too large images: not retried as is — the agent offloads images
	-- to files first (the compact plugin's offload stage), then sends again
	image_limit = true,
}
local RETRY_KINDS = {
	rate_limit = true,
	overloaded = true,
	server = true,
	transport = true,
	stream_closed = true,
}

local function has_word(m, w)
	return m:find("%f[%w]" .. w .. "%f[%W]") ~= nil
end

local function is_stream_stalled(m)
	return m:find("%f[%w]stream stall%f[%W]") ~= nil
		or m:find("%f[%w]stream stalled%f[%W]") ~= nil
		or m:find("%f[%w]stream stalls%f[%W]") ~= nil
		or m:find("%f[%w]stream stalling%f[%W]") ~= nil
end

-- OpenRouter 402 in-flight credit reservation: transient concurrency reservation rather than
-- permanent billing exhaustion, so wait out the window and retry instead of failing as quota.
local function is_in_flight_credit_reservation(m)
	return m:find("%f[%w]in%-flight requests%f[%W]") ~= nil
end

local function is_billing_error_code(s)
	return s == "billing_error" or s == "billing_not_configured"
end

local function is_402_billing_cap(msg, code_str, type_str)
	local m = string.lower(tostring(msg or ""))
	if is_in_flight_credit_reservation(m) then
		return false
	end
	local trimmed = m:match("^%s*(.-)%s*$") or ""
	if trimmed == "" or trimmed == "402" or trimmed:match("^http%s+402$") then
		return true
	end
	local code_l = string.lower(tostring(code_str or ""))
	local type_l = string.lower(tostring(type_str or ""))
	if is_billing_error_code(code_l) or is_billing_error_code(type_l) then
		return true
	end
	if has_word(m, "billing_error") or has_word(m, "billing_not_configured") then
		return true
	end
	if
		m:find("payment required", 1, true)
		or m:find("payment_required", 1, true)
		or m:find("payment is required", 1, true)
		or m:find("deactivated_workspace", 1, true)
		or m:find("can only afford", 1, true)
		or m:find("out of credit", 1, true)
		or m:find("add more credit", 1, true)
		or m:find("add credit", 1, true)
		or m:find("top up", 1, true)
		or m:find("topup", 1, true)
		or m:find("requires?%s+more%s+credits")
		or m:find("billing[%s_%-%.]+verification")
		or m:find("billing[%s_%-%.]+error")
		or m:find("billing[%s_%-%.]+issue")
		or m:find("insufficient.?balance")
		or m:find("insufficient.?credits?")
		or m:find("insufficient.?account.?funds")
	then
		return true
	end
	return false
end

local function status_kind(status)
	if status == 429 then
		return "rate_limit"
	elseif status == 529 then
		return "overloaded"
	elseif status == 408 or status == 409 then
		return "transport"
	elseif status and status >= 500 then
		return "server"
	end
end

local LONG_WINDOW_WORDS = {
	"per hour",
	"per_hour",
	"per day",
	"per_day",
	"per week",
	"per_week",
	"per month",
	"per_month",
	"hourly",
	"daily",
	"weekly",
	"monthly",
	"每小时",
	"每天",
	"每日",
	"每周",
	"每月",
	"本月",
}
local BROKE_WORDS = {
	"balance",
	"credit",
	"欠费",
	"余额",
	"充值",
}

local function has_long_window(m)
	for _, w in ipairs(LONG_WINDOW_WORDS) do
		if m:find(w, 1, true) then
			return true
		end
	end
	return m:find("in %d+%s*hours?") ~= nil or m:find("in %d+%s*days?") ~= nil
end

local function is_minute_rate(m)
	if has_long_window(m) then
		return false
	end
	for _, w in ipairs(BROKE_WORDS) do
		if m:find(w, 1, true) then
			return false
		end
	end
	for _, w in ipairs({
		"per minute",
		"per_minute",
		"perminute",
		"per second",
		"per_second",
		"每分钟",
		"每秒",
		"allocated quota exceeded",
	}) do
		if m:find(w, 1, true) then
			return true
		end
	end
	for _, w in ipairs({ "tpm", "rpm", "tps", "rps" }) do
		if has_word(m, w) then
			return true
		end
	end
	return m:find("in %d+%s*minutes?") ~= nil or m:find("in %d+%s*seconds?") ~= nil
end

local NON_OVERFLOW_PATTERNS = {
	"^throttling error:",
	"^service unavailable:",
	"rate limit",
	"rate_limit",
	"ratelimit",
	"rate increased too quickly",
	"slow_down",
	"too many requests",
	"max[_ ]tokens must be less than",
	"too many tokens.*please wait",
}

local OVERFLOW_PATTERNS = {
	"prompt is too long",
	"prompt too long",
	"prompt exceeds max length",
	"prompt 超长",
	"input is too long for",
	"too large for the model'?s configured input limit",
	"exceeded this model context window limit",
	"exceeds the context window",
	"context window",
	"maximum context",
	"input token count",
	"number of input tokens exceeds maximum length",
	"tokens in request more than max tokens allowed",
	"maximum prompt length is %d+",
	"reduce the length of the messages",
	"requested input length %d+ exceeds maximum input length",
	"exceeds .*maximum allowed input length",
	"is longer than the model'?s context length",
	"exceeds the limit of %d+",
	"exceeds the available context size",
	"greater than the context length",
	"`inputs` tokens %+ `max_new_tokens` must be <= %d+",
	"exceeded model token limit",
	"too large for model with %d+ maximum context length",
	"prompt has [%d,]+ tokens?, but the configured context size is [%d,]+ tokens?",
	"model_context_window_exceeded",
	"range of input length should be",
	"context length is only %d+ tokens",
	"input length.*exceeds.*context length",
	"context[_ ]length[_ ]exceeded",
	"too many tokens",
	"token limit exceeded",
	"exceeds the maximum number of tokens",
	"exceed context limit",
	-- Chinese overflow messages
	"输入过长",
	"输入太长",
	"输入超长",
	"输入长度超过",
	"上下文过长",
	"上下文超长",
	"上下文长度超过",
	"上下文长度超出",
	"超出最大上下文",
	"超过最大上下文",
	"超出上下文长度",
	"超过上下文长度",
	"超过模型最大",
}

local function matches_limit_word(msg, code_str, type_str)
	local m = string.lower(tostring(msg or ""))
	if is_minute_rate(m) then
		return false
	end
	local code_l = string.lower(tostring(code_str or ""))
	local type_l = string.lower(tostring(type_str or ""))
	for _, w in ipairs(LIMIT_WORDS) do
		local lw = string.lower(w)
		if m:find(lw, 1, true) or code_l:find(lw, 1, true) or type_l:find(lw, 1, true) then
			return true
		end
	end
	return false
end

local NON_OVERFLOW_CODES = {
	rate_limit_exceeded = true,
	rate_limit = true,
	insufficient_quota = true,
	exceeded_current_quota_error = true,
	usage_limit_reached = true,
}

local QUOTA_CODES = {
	insufficient_quota = true,
	exceeded_current_quota_error = true,
}

local function is_overflow(msg, opts)
	opts = opts or {}
	if opts.code == "context_length_exceeded" then
		return true
	end
	if opts.status == 429 or NON_OVERFLOW_CODES[opts.code or ""] or NON_OVERFLOW_CODES[opts.type or ""] then
		return false
	end
	local raw = tostring(msg or "")
	local m = string.lower(raw)
	for _, pat in ipairs(NON_OVERFLOW_PATTERNS) do
		if m:find(pat) then
			return false
		end
	end
	if is_minute_rate(m) then
		return false
	end
	for _, pat in ipairs(OVERFLOW_PATTERNS) do
		if m:find(pat) then
			return true
		end
	end
	if opts.extra and opts.extra(msg, opts.code) then
		return true
	end
	return false
end

--- classify(err, cancelled) -> kind, retryable. `err` is a provider error table
--- ({message, status, kind, code, should_retry}) or any other error value.
local function classify(err, cancelled)
	if cancelled then
		return "aborted", false
	end
	local t = type(err) == "table" and err or { message = tostring(err) }
	if t.aborted or t.kind == "aborted" then
		return "aborted", false
	end
	local m = string.lower(tostring(t.message or ""))
	if t.kind == "context_overflow" then
		return "context_overflow", false
	end
	-- the server says so
	if t.should_retry == false then
		return t.kind or "fatal", false
	end
	if t.should_retry == true then
		return t.kind or status_kind(t.status) or "server", true
	end
	if FATAL_KINDS[t.kind or ""] then
		return t.kind, false
	end
	if QUOTA_CODES[t.code or ""] or QUOTA_CODES[t.type or ""] then
		return "quota", false
	end
	-- a per-minute / per-second quota is a rate limit, not an exhausted account,
	-- unless a longer window (hours/days) or account balance wording is also present
	if is_minute_rate(m) then
		return "rate_limit", true
	end
	if t.status == 402 then
		if is_in_flight_credit_reservation(m) then
			return "rate_limit", true
		end
		if is_402_billing_cap(m, t.code, t.type) then
			return "quota", false
		end
	end
	if is_in_flight_credit_reservation(m) then
		return "rate_limit", true
	end
	-- a 429 naming a longer window (hours/days/weekly/monthly) with quota/limit/reset is
	-- an exhausted window, not a per-minute rate limit
	if
		t.status == 429
		and has_long_window(m)
		and (
			m:find("quota", 1, true)
			or m:find("limit", 1, true)
			or m:find("reset", 1, true)
			or m:find("额度", 1, true)
			or m:find("配额", 1, true)
			or m:find("用量", 1, true)
		)
	then
		return "quota", false
	end
	local code_l = string.lower(tostring(t.code or ""))
	local type_l = string.lower(tostring(t.type or ""))
	for _, w in ipairs(LIMIT_WORDS) do
		local lw = string.lower(w)
		if m:find(lw, 1, true) or code_l:find(lw, 1, true) or type_l:find(lw, 1, true) then
			return "quota", false
		end
	end
	if RETRY_KINDS[t.kind or ""] then
		return t.kind, true
	end
	-- core primitives prefix their errors (mare.http): sending / reading / unparseable streams
	if m:find("malformed_response:", 1, true) then
		return "malformed_response", false
	end
	if m:find("transport:", 1, true) then
		return "transport", true
	end
	local sk = status_kind(t.status)
	if sk then
		return sk, true
	end
	if t.status then
		return t.kind or "invalid", false
	end
	if t.retryable == true then
		return t.kind or "server", true
	end
	if is_stream_stalled(m) then
		return "transport", true
	end
	for _, w in ipairs(RETRY_WORDS) do
		if m:find(w, 1, true) then
			return (
				w:find("overload", 1, true)
				or w:find("capacity", 1, true)
				or w:find("busy", 1, true)
				or w == "slow_down"
			)
					and "overloaded"
				or (
					(
							w:find("rate", 1, true)
							or w:find("many", 1, true)
							or w:find("allocated quota", 1, true)
							or w:find("限流", 1, true)
							or w:find("频繁", 1, true)
							or w:find("频率", 1, true)
							or w:find("速率", 1, true)
							or w:find("并发", 1, true)
							or w:find("超出tp", 1, true)
							or w:find("超出rp", 1, true)
							or w:find("超出qp", 1, true)
						)
						and "rate_limit"
					or "transport"
				),
				true
		end
	end
	for _, code in ipairs(RETRY_STATUS_WORDS) do
		if has_word(m, code) then
			return status_kind(tonumber(code)) or "server", true
		end
	end
	return t.kind or "fatal", false
end

local function append_hint(msg, hint)
	local s = tostring(msg or "")
	if type(hint) == "string" and hint ~= "" and not s:find(hint, 1, true) then
		return s .. " (" .. hint .. ")"
	end
	return s
end

local function lookup_code(codes, code_val, code_str, type_str)
	if type(codes) ~= "table" then
		return nil
	end
	if code_val ~= nil and codes[code_val] ~= nil then
		return codes[code_val]
	end
	if code_str ~= nil and codes[code_str] ~= nil then
		return codes[code_str]
	end
	if type_str ~= nil and codes[type_str] ~= nil then
		return codes[type_str]
	end
	return nil
end

local function apply_code_entry(entry, err, allow_weak_invalid)
	if type(entry) == "string" then
		local k = entry:gsub("^llm%.", "")
		if allow_weak_invalid or k ~= "invalid" then
			err.kind = k
		end
		return k
	elseif type(entry) == "table" then
		local k = entry.kind or entry.code
		if type(k) == "string" then
			k = k:gsub("^llm%.", "")
		end
		if k and (allow_weak_invalid or k ~= "invalid") then
			err.kind = k
		end
		if entry.retryable ~= nil then
			err.should_retry = entry.retryable
		end
		if entry.hint then
			err.message = append_hint(err.message, entry.hint)
		end
		return k or (entry.retryable ~= nil and "__retryable" or nil)
	end
	return nil
end

-- Detect vendor 400 effort refusals and extract the vendor's "Supported values are …" clause
-- (adapted from magpie internal/gateway/effort_accounts.go, MIT). No automatic learning.
local EFFORT_DENIED_PATTERNS = {
	"not supported",
	"unsupported",
	"not available",
	"isn't available",
	"isnt available",
	"not allowed",
	"invalid value",
	"upgrade",
	"your plan",
}

local EFFORT_LEVEL_WORDS = {
	none = true,
	off = true,
	minimal = true,
	low = true,
	medium = true,
	high = true,
	xhigh = true,
	max = true,
}

local function matches_effort_refusal(text, param)
	local param_is_effort = false
	if type(param) == "string" and param ~= "" then
		if not param:lower():match("reasoning[._ ]?effort") then
			return false
		end
		param_is_effort = true
	end
	if type(text) ~= "string" or text == "" then
		return false
	end
	local low = text:lower()
	local text_is_effort = low:match("reasoning[._ ]?effort") ~= nil
		or low:match("reasoning%s+effort") ~= nil
		or low:match("thinking[._ ]?effort") ~= nil
		or low:match("effort%s+level") ~= nil
	if not param_is_effort and not text_is_effort then
		return false
	end
	local unsupported_val = low:match("unsupported value:?%s*['\"`]?([a-z_]+)['\"`]?%s+is not supported")
	if unsupported_val and (param_is_effort or EFFORT_LEVEL_WORDS[unsupported_val]) then
		return true
	end
	if low:find("invalid", 1, true) or low:find("supported values are", 1, true) then
		return true
	end
	for _, pat in ipairs(EFFORT_DENIED_PATTERNS) do
		if low:find(pat, 1, true) then
			return true
		end
	end
	return false
end

local function extract_supported_values_clause(text)
	if type(text) ~= "string" or text == "" then
		return nil
	end
	local low = text:lower()
	local s, e = low:find("supported values are:?%s*")
	if not s then
		return nil
	end
	local tail = text:sub(e + 1)
	local clause = tail:match("^([^.\r\n}]+)")
	if not clause then
		return nil
	end
	clause = clause:gsub('["\\]+$', ""):match("^%s*(.-)%s*$")
	if not clause or clause == "" then
		return nil
	end
	return "Supported values are: " .. clause
end

local function effort_refusal_hint(err)
	if err.status ~= 400 then
		return nil
	end
	local raw_msg = type(err.raw_message) == "string" and err.raw_message or ""
	local body = type(err.body) == "string" and err.body or ""
	local matched = matches_effort_refusal(raw_msg, err.param) or matches_effort_refusal(body, err.param)
	if not matched then
		return nil
	end
	local supported = extract_supported_values_clause(raw_msg) or extract_supported_values_clause(body)
	local cur = tostring(err.message or "")
	if supported and not cur:lower():find("supported values are", 1, true) then
		return supported .. " — change reasoning effort with /effort"
	end
	return "change reasoning effort with /effort"
end

local function header_ci(headers, target)
	if type(headers) ~= "table" then
		return nil
	end
	if headers[target] ~= nil then
		return headers[target]
	end
	for k, v in pairs(headers) do
		if type(k) == "string" and string.lower(k) == target then
			return v
		end
	end
	return nil
end

local CODE_OVERRIDES = {
	overloaded = "llm.provider_overloaded",
}

local KIND_ALIASES = {
	provider_overloaded = "overloaded",
}

local function canonical_kind(kind)
	if type(kind) ~= "string" then
		return nil
	end
	local canonical = kind:gsub("^llm%.", "")
	if canonical == "" then
		return nil
	end
	return KIND_ALIASES[canonical] or canonical
end

local SUMMARY_FIELDS = {
	"kind",
	"error_code",
	"status",
	"raw_code",
	"code",
	"raw_type",
	"type",
	"provider",
	"request_id",
	"source",
}

local function summarize_error(err)
	if type(err) == "table" then
		if type(err.message) == "string" and err.message ~= "" then
			return err.message
		end
		local parts = {}
		for _, field in ipairs(SUMMARY_FIELDS) do
			local v = err[field]
			if type(v) == "string" or type(v) == "number" or type(v) == "boolean" then
				parts[#parts + 1] = field .. "=" .. tostring(v)
			end
		end
		if #parts > 0 then
			return table.concat(parts, " ")
		end
		return "provider error"
	end
	return tostring(err)
end

local function mare_code(kind)
	local canonical = canonical_kind(kind)
	if canonical then
		if CODE_OVERRIDES[canonical] then
			return CODE_OVERRIDES[canonical]
		end
		return "llm." .. canonical
	end
	return "llm.fatal"
end

--- classify_raw(raw, opts?, providers?) -> err table (with `kind` and `retryable` populated).
--- Single classifier across all providers:
---   cancel -> provider.errors.map -> provider.errors.codes -> wire.errors.codes ->
---   image_limit / context_overflow -> status -> generic wording.
local function classify_raw(raw, opts, providers)
	opts = opts or {}
	local lib = require("mare_lib")
	local err = type(raw) == "table" and raw or { message = tostring(raw) }
	if err.code == nil and err.api_code ~= nil then
		err.code = tostring(err.api_code)
	end
	if err.type == nil and err.api_type ~= nil then
		err.type = err.api_type
	end
	if err._classified then
		local k, r
		if opts.cancelled then
			k, r = "aborted", false
		else
			k = canonical_kind(err.kind) or "fatal"
			r = err.retryable == true
		end
		local code = mare_code(k)
		err.kind, err.retryable, err.error_code = k, r, code
		if type(err.data) == "table" then
			err.data.kind = k
			err.data.error_code = code
		end
		return err, k, r
	end

	local prov_name = err.provider or opts.provider
	local prov = providers and prov_name and providers[prov_name] or nil
	local prov_errors = opts.provider_errors or (prov and prov.errors) or nil
	local wire_errors = opts.wire_errors or nil

	-- 1. User cancellation wins above everything.
	if opts.cancelled then
		err.kind, err.retryable, err._classified = "aborted", false, true
		return err, "aborted", false
	end

	-- 2. Provider `errors.map(raw)`: text mapping, per-request retry override, extra hints.
	local mapped_kind, mapped_retryable
	if prov_errors and type(prov_errors.map) == "function" then
		local prev_kind, prev_sr = err.kind, err.should_retry
		local mapped = prov_errors.map(err)
		if type(mapped) == "table" then
			if mapped.message ~= nil then
				err.message = mapped.message
			end
			if mapped.hint then
				err.message = append_hint(err.message, mapped.hint)
			end
			local k = mapped.kind or mapped.code
			if type(k) == "string" then
				k = canonical_kind(k)
			end
			if k ~= nil then
				mapped_kind = k
				err.kind = k
			end
			if mapped.retryable ~= nil then
				mapped_retryable = mapped.retryable
				err.should_retry = mapped.retryable
			end
			if mapped.reply_left ~= nil then
				err.reply_left = mapped.reply_left
			end
			if mapped.plan_type ~= nil then
				err.plan_type = mapped.plan_type
			end
			if mapped.resets_at ~= nil then
				err.resets_at = mapped.resets_at
			end
		end
		if not mapped_kind and err.kind ~= prev_kind then
			mapped_kind = err.kind
		end
		if mapped_retryable == nil and err.should_retry ~= prev_sr then
			mapped_retryable = err.should_retry
		end
	end

	-- 3. Provider `errors.codes` -> Wire `errors.codes`.
	local code_val = err.api_code ~= nil and err.api_code or err.code
	local code_str = code_val ~= nil and tostring(code_val) or nil
	local type_str = err.api_type ~= nil and tostring(err.api_type) or (err.type ~= nil and tostring(err.type) or nil)
	local matched_code
	if not mapped_kind then
		local entry = lookup_code(prov_errors and prov_errors.codes, code_val, code_str, type_str)
		if entry == nil then
			entry = lookup_code(wire_errors and wire_errors.codes, code_val, code_str, type_str)
		end
		if entry == nil then
			entry = lookup_code(opts.codes, code_val, code_str, type_str)
		end
		if entry ~= nil then
			matched_code = apply_code_entry(entry, err, false)
		end
	end

	-- 4. Image limits and context overflow (beat a generic `"invalid"` code or 4xx status).
	local msg_text = err.raw_message ~= nil and err.raw_message or err.message
	local img_fn = (prov_errors and prov_errors.image_limit)
		or (wire_errors and wire_errors.image_limit)
		or opts.image_limit
	if
		not mapped_kind
		and (not matched_code or matched_code == "invalid")
		and img_fn
		and img_fn(msg_text, code_str)
	then
		err.kind = "image_limit"
	else
		local status = err.status
		if not opts.no_reply_left and (status == 400 or status == 422 or status == nil) and err.reply_left == nil then
			local left = lib.parse_reply_left(msg_text)
			if left then
				err.reply_left = left
			end
		end
		local extra_ov = (prov_errors and prov_errors.overflow)
			or (wire_errors and wire_errors.overflow)
			or opts.overflow
		if
			not mapped_kind
			and (not matched_code or matched_code == "invalid")
			and err.kind == nil
			and is_overflow(msg_text, {
				code = code_str,
				type = type_str,
				status = status,
				provider = prov_name or err.label,
				extra = extra_ov,
			})
		then
			err.kind = "context_overflow"
		elseif
			not mapped_kind
			and err.kind == nil
			and status == 402
			and is_in_flight_credit_reservation(string.lower(tostring(msg_text or "")))
		then
			err.kind = "rate_limit"
		elseif
			not mapped_kind
			and err.kind == nil
			and status == 402
			and (matched_code == nil or matched_code == "invalid")
			and is_402_billing_cap(msg_text, code_str, type_str)
		then
			err.kind = "quota"
		elseif
			not mapped_kind
			and err.kind == nil
			and (matched_code == nil or matched_code == "invalid")
			and matches_limit_word(msg_text, code_str, type_str)
		then
			err.kind = "quota"
		elseif not mapped_kind and err.kind == nil and matched_code == "invalid" then
			err.kind = "invalid"
		elseif not mapped_kind and err.kind == nil and status == 413 then
			err.kind = "payload_too_large"
			err.message = tostring(err.message or "")
				.. " (the request body is too large — remove large images or attachments and retry)"
		elseif not mapped_kind and err.kind == nil and (status == 401 or status == 403) then
			err.kind = "auth"
		end
	end

	if opts.map then
		opts.map(err, opts.api_err or {}, opts.body)
	end

	-- 5. Per-request `x-should-retry` header unless provider.errors.map already forced retryable.
	if mapped_retryable == nil and err.should_retry == nil then
		local xs = header_ci(err.headers, "x-should-retry")
		if xs == "true" then
			err.should_retry = true
		elseif xs == "false" then
			err.should_retry = false
		end
	end

	if not err.kind and err.status and err.status >= 400 and err.status < 500 and not status_kind(err.status) then
		err.kind = "invalid"
	end

	local effort_hint = effort_refusal_hint(err)
	if effort_hint then
		err.message = append_hint(err.message, effort_hint)
	end

	if err.request_id and not tostring(err.message or ""):find(err.request_id, 1, true) then
		err.message = ("%s [request-id: %s]"):format(tostring(err.message or ""), err.request_id)
	end

	local kind, retryable
	if mapped_retryable ~= nil then
		kind = mapped_kind or err.kind or status_kind(err.status) or "fatal"
		retryable = mapped_retryable
	else
		kind, retryable = classify(err, opts.cancelled)
	end
	local raw_code = err.raw_code
	if raw_code == nil then
		local vc = err.api_code ~= nil and err.api_code or err.code
		if vc ~= nil and not (type(vc) == "string" and vc:match("^llm%.")) then
			raw_code = tostring(vc)
		end
	end
	local raw_type = err.raw_type
	if raw_type == nil then
		local vt = err.api_type ~= nil and err.api_type or err.type
		if vt ~= nil then
			raw_type = tostring(vt)
		end
	end
	local error_code = mare_code(kind)
	local data = {
		kind = kind,
		error_code = error_code,
		raw_code = raw_code,
		raw_type = raw_type,
		status = err.status,
		provider = err.provider,
		request_id = err.request_id,
		source = err.source,
	}
	err.raw_code = raw_code
	err.raw_type = raw_type
	err.error_code = error_code
	err.data = data
	err.kind, err.retryable, err._classified = kind, retryable, true
	return err, kind, retryable
end

return {
	classify = classify,
	classify_raw = classify_raw,
	status_kind = status_kind,
	is_overflow = is_overflow,
	mare_code = mare_code,
	canonical_kind = canonical_kind,
	summarize_error = summarize_error,
}
