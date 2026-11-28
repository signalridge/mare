-- compact.budget: lightweight config validation and model budget calculation shared between
-- `plugins/compact.lua` (startup/reload validation) and `compact.measure` (runtime compaction).
local M = {}

local mare_lib = require("mare_lib")
local DEFAULT_HEADROOM_RATIO = 0.2

function M.number(value, label, minimum, maximum, integer)
	if
		type(value) ~= "number"
		or value ~= value
		or value == math.huge
		or value == -math.huge
		or value < minimum
		or (maximum and value > maximum)
		or (integer and value % 1 ~= 0)
	then
		return nil, "compaction: invalid " .. label
	end
	return value
end

local function check_thresholds(values, label)
	for _, spec in ipairs({
		{ "ratio", 0, 1, false },
		{ "percent", 0, 100, false },
		{ "tokens", 1, nil, true },
		{ "headroom_tokens", 0, nil, true },
	}) do
		local key = spec[1]
		local value = values[key]
		if value ~= nil then
			local valid, err = M.number(value, label .. key, spec[2], spec[3], spec[4])
			if not valid or ((key == "ratio" or key == "percent") and value == 0) then
				return nil, err or ("compaction: invalid " .. label .. key)
			end
		end
	end
	return true
end

function M.validate_config(cfg)
	if cfg.summary_max_tokens ~= nil then
		local _, err = M.number(cfg.summary_max_tokens, "compact.summary_max_tokens", 1, nil, true)
		if err then
			return nil, err
		end
	end
	local ok, err = check_thresholds({
		ratio = cfg.threshold_ratio,
		percent = cfg.threshold_percent,
		tokens = cfg.threshold_tokens,
		headroom_tokens = cfg.headroom_tokens,
	}, "compact.")
	if not ok then
		return nil, err
	end
	for ref, per in pairs(cfg.model_thresholds or {}) do
		if type(per) ~= "table" then
			return nil, "compaction: invalid model threshold for " .. tostring(ref)
		end
		ok, err = check_thresholds(per, "compact.model_thresholds." .. tostring(ref) .. ".")
		if not ok then
			return nil, err
		end
	end
	return true
end

--- Compute `{ limit, window, reserved, message, pressure, headroom }` for `model` and optional
--- `request_max_tokens`, or `nil, error_message` when the configuration or model budget is invalid.
--- When `headroom_explicit` is `false` and the model has no per-model `headroom_tokens` override,
--- the default headroom scales down to `DEFAULT_HEADROOM_RATIO` of the message budget on small
--- windows so default config does not exhaust the pressure budget.
function M.compute(cfg, model, request_max_tokens, headroom_explicit)
	local valid, config_error = M.validate_config(cfg)
	if not valid then
		return nil, config_error
	end
	local window = model and model.context_window
	if window == nil then
		window = 128000
	end
	local name = model and (model.ref or model.id) or "the model"
	local per = model and cfg.model_thresholds and cfg.model_thresholds[model.ref or ""] or {}
	local headroom = per.headroom_tokens or cfg.headroom_tokens
	local explicit_headroom = per.headroom_tokens ~= nil or headroom_explicit ~= false
	local reserved = mare_lib.output_limit(request_max_tokens, model and model.max_output)
	local _, window_error = M.number(window, tostring(name) .. " context_window", 1, nil, true)
	local reserved_error = not reserved and ("compaction: invalid " .. tostring(name) .. " max_output/max_tokens")
	if window_error or reserved_error then
		return nil, window_error or reserved_error
	end
	local message = window - reserved
	if message <= 0 then
		return nil,
			("compaction: %s reserves %d output tokens of its %d-token window, leaving no message budget; "):format(
				name,
				reserved,
				window
			) .. "configure a larger context_window or a smaller max_output for it"
	end
	if not explicit_headroom then
		local scaled = math.max(1, math.floor(message * DEFAULT_HEADROOM_RATIO))
		if headroom >= message - scaled then
			headroom = scaled
		end
	end
	local pressure = message - headroom
	if pressure <= 0 then
		return nil,
			("compaction: %s reserves %d output tokens and %d headroom tokens of its %d-token window, "):format(
				name,
				reserved,
				headroom,
				window
			) .. "leaving no pressure budget; lower compact.headroom_tokens (or the model's), or its max_output"
	end
	local limit
	if per.tokens or cfg.threshold_tokens then
		limit = per.tokens or cfg.threshold_tokens
	elseif per.percent or cfg.threshold_percent then
		limit = math.floor(window * (per.percent or cfg.threshold_percent) / 100)
	else
		limit = math.floor(math.min(window * (per.ratio or cfg.threshold_ratio), pressure))
	end
	return {
		limit = math.min(limit, pressure),
		window = window,
		reserved = reserved,
		message = message,
		pressure = pressure,
		headroom = headroom,
	}
end

return M
