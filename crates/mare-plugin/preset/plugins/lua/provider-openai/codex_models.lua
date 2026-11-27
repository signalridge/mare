-- provider-openai.codex_models: ChatGPT Codex `/codex/models` URL validation and entry mapping.
local M = {}

local EFFORT_WIRE_TO_MARE = { none = "off" }

function M.valid_base(raw)
	if type(raw) ~= "string" or raw == "" then
		return nil, "invalid codex_base_url"
	end
	local base = raw:gsub("/+$", "")
	local ok, norm = pcall(mare.http.build_url, base)
	if not ok or type(norm) ~= "string" then
		return nil, "invalid codex_base_url"
	end
	local scheme, authority = norm:match("^(https?)://([^/?#]+)")
	if not scheme or not authority or authority:find("@", 1, true) then
		return nil, "invalid codex_base_url"
	end
	if scheme == "https" then
		return base
	end
	local host = authority:match("^(%[[^%]]+%])") or authority:match("^([^:]+)")
	if host == "127.0.0.1" or host == "localhost" or host == "[::1]" then
		return base
	end
	return nil, "codex_base_url must use https (or loopback http)"
end

function M.origin(raw)
	if type(raw) ~= "string" or raw == "" then
		return nil
	end
	local ok, norm = pcall(mare.http.build_url, raw)
	if not ok or type(norm) ~= "string" then
		return nil
	end
	local scheme, authority = norm:match("^(https?)://([^/?#]+)")
	if not scheme or not authority or authority:find("@", 1, true) then
		return nil
	end
	local host, port_str
	if authority:sub(1, 1) == "[" then
		local bracketed, rest = authority:match("^(%[[^%]]+%])(.*)$")
		if not bracketed or (rest ~= "" and not rest:match("^:%d+$")) then
			return nil
		end
		host = bracketed
		port_str = rest:match("^:(%d+)$")
	else
		local h, rest = authority:match("^([^:]+)(.*)$")
		if not h or h == "" or (rest ~= "" and not rest:match("^:%d+$")) then
			return nil
		end
		host = h
		port_str = rest:match("^:(%d+)$")
	end
	local port = port_str and tonumber(port_str) or (scheme == "https" and 443 or 80)
	if not port or port < 1 or port > 65535 then
		return nil
	end
	return ("%s://%s:%d"):format(scheme, host, port)
end

M.is_array = require("mare_lib").is_json_array
M.has_error_field = require("mare_lib").has_error_field

function M.is_valid_model_item(m)
	if type(m) ~= "table" or M.is_array(m) or M.has_error_field(m) then
		return false
	end
	local has_slug = type(m.slug) == "string" and m.slug ~= ""
	local has_id = type(m.id) == "string" and m.id ~= ""
	return has_slug or has_id
end

function M.validate_models_payload(models)
	if not M.is_array(models) then
		return false
	end
	for _, m in ipairs(models) do
		if not M.is_valid_model_item(m) then
			return false
		end
	end
	return true
end

function M.client_version()
	local v = type(mare.version) == "string" and mare.version or ""
	return v:match("^(%d+%.%d+%.%d+)") or (v ~= "" and v or "0.1.0")
end

function M.entry(m)
	if type(m) ~= "table" then
		return nil
	end
	local id = type(m.slug) == "string" and m.slug ~= "" and m.slug or (type(m.id) == "string" and m.id ~= "" and m.id)
	if not id or m.visibility == "none" then
		return nil
	end
	local efforts
	if type(m.supported_reasoning_levels) == "table" then
		efforts = {}
		for _, lvl in ipairs(m.supported_reasoning_levels) do
			local e = type(lvl) == "table" and lvl.effort or lvl
			if type(e) == "string" and e ~= "" then
				efforts[#efforts + 1] = EFFORT_WIRE_TO_MARE[e] or e
			end
		end
		if #efforts == 0 then
			efforts = nil
		end
	end
	local default_effort = type(m.default_reasoning_level) == "string"
			and m.default_reasoning_level ~= ""
			and (EFFORT_WIRE_TO_MARE[m.default_reasoning_level] or m.default_reasoning_level)
		or nil
	local has_levels_table = type(m.supported_reasoning_levels) == "table"
	local reasoning
	if has_levels_table and #m.supported_reasoning_levels == 0 then
		reasoning = false
		default_effort = nil
	elseif efforts ~= nil or default_effort ~= nil then
		reasoning = true
	end
	local reasoning_summary
	if reasoning == false or m.supports_reasoning_summary_parameter == false then
		reasoning_summary = false
	elseif m.supports_reasoning_summary_parameter == true then
		reasoning_summary = true
	elseif reasoning == true then
		reasoning_summary = true
	end
	local input
	if type(m.input_modalities) == "table" then
		input = {}
		for _, mod in ipairs(m.input_modalities) do
			if mod == "text" or mod == "image" then
				input[#input + 1] = mod
			end
		end
		if #input == 0 then
			input = nil
		end
	end
	local trunc = type(m.truncation_policy) == "table"
			and type(m.truncation_policy.mode) == "string"
			and tonumber(m.truncation_policy.limit)
			and { mode = m.truncation_policy.mode, limit = tonumber(m.truncation_policy.limit) }
		or nil
	local tool_update
	if m.supports_search_tool == true then
		tool_update = "addition_only"
	elseif m.supports_search_tool == false then
		tool_update = false
	end
	return {
		id = id,
		name = (type(m.display_name) == "string" and m.display_name ~= "" and m.display_name)
			or (type(m.name) == "string" and m.name ~= "" and m.name)
			or nil,
		hidden = m.visibility == "hide" or nil,
		context_window = tonumber(m.context_window),
		max_output = tonumber(m.max_output),
		reasoning = reasoning,
		reasoning_summary = reasoning_summary,
		verbosity = m.support_verbosity == true and type(m.default_verbosity) == "string" and m.default_verbosity
			or nil,
		truncation = trunc,
		efforts = efforts,
		default_effort = default_effort,
		edit_tool = m.apply_patch_tool_type ~= nil and "apply_patch" or nil,
		tool_update = tool_update,
		input = input,
	}
end

return M
