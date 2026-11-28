-- Provider-dialect JSON Schema sanitizer for tool declarations and structured output_schema.
-- Behaviorally aligned with official provider validators gateway schema normalizers,
-- and Vercel AI SDK schema sanitizers; returns a fresh copy when modified without mutating inputs.

local M = {}

local function is_null(v)
	return v == nil or (mare and mare.json and v == mare.json.null)
end

local function contains_ci(haystack, needle)
	if type(haystack) ~= "string" then
		return false
	end
	return haystack:lower():find(needle, 1, true) ~= nil
end

local function host_of(url)
	if type(url) ~= "string" then
		return ""
	end
	local h = url:match("^https?://([^/:?#]+)") or ""
	return h:lower()
end

local function host_matches(host, domain)
	if type(host) ~= "string" or host == "" then
		return false
	end
	if host == domain then
		return true
	end
	return #host > #domain and host:sub(-(#domain + 1)) == "." .. domain
end

local function id_matches(s, name)
	if type(s) ~= "string" or s == "" then
		return false
	end
	if s == name then
		return true
	end
	return s:match("^" .. name .. "[-_/.]") ~= nil
end

--- Detect target schema dialects from an explicit dialect id, endpoint base URL, and model id.
function M.detect_dialect(dialect, base_url, model_id, opts)
	opts = opts or {}
	local host = host_of(base_url)
	local d = type(dialect) == "string" and dialect:lower() or ""
	local m = type(model_id) == "string" and model_id:lower() or ""

	return {
		deepseek = id_matches(d, "deepseek") or host_matches(host, "deepseek.com") or contains_ci(m, "deepseek"),
		kimi = id_matches(d, "kimi") or id_matches(d, "moonshot") or host_matches(host, "kimi.com") or host_matches(
			host,
			"moonshot.cn"
		) or host_matches(host, "moonshot.ai") or id_matches(m, "kimi") or id_matches(m, "moonshot"),
		grok = id_matches(d, "xai") or id_matches(d, "grok") or host_matches(host, "x.ai") or id_matches(m, "grok"),
		openrouter_xgrammar = id_matches(d, "openrouter")
			or id_matches(d, "cline")
			or id_matches(d, "kilo")
			or id_matches(d, "kilocode")
			or host_matches(host, "openrouter.ai")
			or host_matches(host, "api.cline.bot")
			or host_matches(host, "kilo.ai")
			or host_matches(host, "kilocode.ai"),
		google_vertex = d == "google"
			or id_matches(d, "google-vertex")
			or host_matches(host, "googleapis.com")
			or id_matches(m, "gemini")
			or opts.route == "google",
		anthropic = d == "anthropic" or host_matches(host, "anthropic.com") or opts.route == "anthropic",
		openai_strict = opts.strict == true
			or (opts.output_schema == true and (d == "openai" or host_matches(host, "openai.com"))),
		output_schema = opts.output_schema == true,
	}
end

--- DeepSeek rejects ECMAScript `\0` escapes in regex patterns; replace unescaped `\0`
--- (not preceded by an odd backslash run and not followed by an ASCII digit) with `\u0000`.
function M.unicode_null_escape(pattern)
	if type(pattern) ~= "string" or not pattern:find("\\0", 1, true) then
		return pattern
	end
	local out, i, n = {}, 1, #pattern
	while i <= n do
		local b = pattern:byte(i)
		if b == 92 then -- '\'
			local run_start = i
			while i <= n and pattern:byte(i) == 92 do
				i = i + 1
			end
			local run_len = i - run_start
			if run_len % 2 == 1 and i <= n and pattern:byte(i) == 48 then -- '0'
				local next_b = i + 1 <= n and pattern:byte(i + 1) or nil
				if not next_b or next_b < 48 or next_b > 57 then
					if run_len > 1 then
						out[#out + 1] = pattern:sub(run_start, run_start + run_len - 2)
					end
					out[#out + 1] = "\\u0000"
					i = i + 1
				else
					out[#out + 1] = pattern:sub(run_start, i - 1)
				end
			else
				out[#out + 1] = pattern:sub(run_start, i - 1)
			end
		else
			out[#out + 1] = string.char(b)
			i = i + 1
		end
	end
	return table.concat(out)
end

--- xAI/Grok uses Rust's `regex` crate, which rejects unescaped `[` and set operators
--- (`&&`, `--`, `~~`) inside character classes `[...]`.
function M.ecma_class_for_rust(pattern)
	if type(pattern) ~= "string" then
		return pattern
	end
	local out, i, n = {}, 1, #pattern
	local in_class = false
	local class_start = 0
	while i <= n do
		local ch = pattern:sub(i, i)
		if ch == "\\" then
			local nxt = i + 1 <= n and pattern:sub(i + 1, i + 1) or ""
			out[#out + 1] = "\\" .. nxt
			i = i + 2
		elseif not in_class then
			if pattern:sub(i, i + 2) == "[^]" then
				out[#out + 1] = "[\\s\\S]"
				i = i + 3
			elseif pattern:sub(i, i + 1) == "[]" then
				out[#out + 1] = "[^\\s\\S]"
				i = i + 2
			else
				out[#out + 1] = ch
				if ch == "[" then
					in_class = true
					class_start = i
					if i + 1 <= n and pattern:sub(i + 1, i + 1) == "^" then
						out[#out + 1] = "^"
						i = i + 2
					else
						i = i + 1
					end
				else
					i = i + 1
				end
			end
		else
			local is_leading_close = (
				ch == "]"
				and (
					i == class_start + 1
					or (i == class_start + 2 and pattern:sub(class_start + 1, class_start + 1) == "^")
				)
			)
			if ch == "]" and not is_leading_close then
				out[#out + 1] = "]"
				in_class = false
				i = i + 1
			elseif ch == "[" then
				out[#out + 1] = "\\["
				i = i + 1
			elseif (ch == "&" or ch == "-" or ch == "~") and i + 1 <= n and pattern:sub(i + 1, i + 1) == ch then
				out[#out + 1] = ch .. "\\" .. ch
				i = i + 2
			else
				out[#out + 1] = ch
				i = i + 1
			end
		end
	end
	return table.concat(out)
end

local function is_array_table(t)
	if type(t) ~= "table" then
		return false
	end
	if mare and mare.json and mare.json.is_array and mare.json.is_array(t) then
		return true
	end
	local n = #t
	if n == 0 then
		return false
	end
	for k in pairs(t) do
		if type(k) ~= "number" or k < 1 or k > n or k % 1 ~= 0 then
			return false
		end
	end
	return true
end

local function value_json_type(v)
	local tv = type(v)
	if tv == "string" then
		return "string"
	elseif tv == "boolean" then
		return "boolean"
	elseif tv == "number" then
		return (v % 1 == 0) and "integer" or "number"
	elseif tv == "table" then
		return is_array_table(v) and "array" or "object"
	elseif is_null(v) then
		return "null"
	end
	return nil
end

local function json_eq(a, b)
	if is_null(a) and is_null(b) then
		return true
	end
	if is_null(a) or is_null(b) then
		return false
	end
	if type(a) ~= type(b) then
		return false
	end
	if type(a) ~= "table" then
		return a == b
	end
	local a_arr, b_arr = is_array_table(a), is_array_table(b)
	if a_arr ~= b_arr then
		return false
	end
	if a_arr then
		if #a ~= #b then
			return false
		end
		for i = 1, #a do
			if not json_eq(a[i], b[i]) then
				return false
			end
		end
		return true
	end
	for k, v in pairs(a) do
		if not json_eq(v, b[k]) then
			return false
		end
	end
	for k in pairs(b) do
		if a[k] == nil then
			return false
		end
	end
	return true
end

local function infer_enum_type(values)
	if type(values) ~= "table" or #values == 0 then
		return nil
	end
	local seen_non_null, saw_null = nil, false
	local saw_integer, saw_float = false, false
	for _, v in ipairs(values) do
		local t = value_json_type(v)
		if t == nil then
			return nil
		elseif t == "null" then
			saw_null = true
		else
			if t == "integer" then
				saw_integer = true
				t = "number"
			elseif t == "number" then
				saw_float = true
			end
			if seen_non_null == nil then
				seen_non_null = t
			elseif seen_non_null ~= t then
				return nil
			end
		end
	end
	if seen_non_null == "number" then
		seen_non_null = saw_float and "number" or (saw_integer and "integer" or "number")
	end
	if not seen_non_null then
		return saw_null and "null" or nil
	end
	if saw_null then
		return { seen_non_null, "null" }
	end
	return seen_non_null
end

local function infer_branch_common_type(branches, is_intersection)
	if type(branches) ~= "table" or #branches == 0 then
		return nil
	end
	local common = nil
	for _, b in ipairs(branches) do
		if type(b) ~= "table" then
			return nil
		end
		if type(b.type) == "string" then
			if common == nil then
				common = b.type
			elseif common ~= b.type then
				return nil
			end
		elseif not is_intersection then
			return nil
		end
	end
	return common
end

local function infer_vertex_type(s)
	if s.properties ~= nil or s.required ~= nil or s.additionalProperties ~= nil then
		return "object"
	end
	if s.items ~= nil or s.prefixItems ~= nil or s.minItems ~= nil or s.maxItems ~= nil then
		return "array"
	end
	if s.enum ~= nil then
		return infer_enum_type(s.enum)
	end
	if s.const ~= nil then
		return value_json_type(s.const)
	end
	if s.pattern ~= nil or s.format ~= nil or s.minLength ~= nil or s.maxLength ~= nil then
		return "string"
	end
	if
		s.minimum ~= nil
		or s.maximum ~= nil
		or s.exclusiveMinimum ~= nil
		or s.exclusiveMaximum ~= nil
		or s.multipleOf ~= nil
	then
		return "number"
	end
	return infer_branch_common_type(s.anyOf, false)
		or infer_branch_common_type(s.oneOf, false)
		or infer_branch_common_type(s.allOf, true)
end

local function shallow_copy(t)
	local out = {}
	for k, v in pairs(t) do
		out[k] = v
	end
	return out
end

local ANTHROPIC_NUM_CONSTRAINTS = {
	{ "minimum", "minimum" },
	{ "maximum", "maximum" },
	{ "exclusiveMinimum", "exclusive minimum" },
	{ "exclusiveMaximum", "exclusive maximum" },
	{ "multipleOf", "multiple of" },
}

local ANTHROPIC_STR_CONSTRAINTS = {
	{ "minLength", "min length" },
	{ "maxLength", "max length" },
	{ "pattern", "pattern" },
	{ "format", "format" },
}

local ANTHROPIC_SUPPORTED_FORMATS = {
	["date-time"] = true,
	time = true,
	date = true,
	duration = true,
	email = true,
	hostname = true,
	ipv4 = true,
	ipv6 = true,
	uuid = true,
	uri = true,
}

local function anthropic_pattern_supported(pat)
	if type(pat) ~= "string" then
		return false
	end
	if pat:find("%(%?[=!<]") ~= nil or pat:find("\\[1-9]") ~= nil then
		return false
	end
	return true
end

local function schema_has_type(t, want)
	if type(t) == "string" then
		return t == want
	end
	if type(t) == "table" then
		for _, v in ipairs(t) do
			if v == want then
				return true
			end
		end
	end
	return false
end

local function branch_type_set(b)
	if type(b) ~= "table" then
		return nil
	end
	local t = b.type
	if t == nil then
		if b.const ~= nil then
			t = value_json_type(b.const)
		elseif type(b.enum) == "table" and #b.enum > 0 then
			t = infer_enum_type(b.enum)
		end
	end
	if type(t) == "string" then
		return { [t == "integer" and "number" or t] = true }
	end
	if type(t) == "table" and #t > 0 then
		local set = {}
		for _, v in ipairs(t) do
			if type(v) ~= "string" then
				return nil
			end
			set[v == "integer" and "number" or v] = true
		end
		return set
	end
	return nil
end

local function branch_const_values(b)
	if type(b) ~= "table" then
		return nil
	end
	if b.const ~= nil then
		return { b.const }
	end
	if type(b.enum) == "table" then
		return b.enum
	end
	return nil
end

local function values_disjoint(a_vals, b_vals)
	for _, av in ipairs(a_vals) do
		for _, bv in ipairs(b_vals) do
			if json_eq(av, bv) then
				return false
			end
		end
	end
	return true
end

local function has_required_key(b, key)
	if type(b.required) ~= "table" then
		return false
	end
	for _, k in ipairs(b.required) do
		if k == key then
			return true
		end
	end
	return false
end

local function pair_provably_disjoint(a, b)
	if type(a) ~= "table" or type(b) ~= "table" then
		return false
	end
	local ta, tb = branch_type_set(a), branch_type_set(b)
	if ta and tb then
		local overlap = false
		for k in pairs(ta) do
			if tb[k] then
				overlap = true
				break
			end
		end
		if not overlap then
			return true
		end
	end
	local va, vb = branch_const_values(a), branch_const_values(b)
	if va and vb and values_disjoint(va, vb) then
		return true
	end
	local a_is_obj = ta and ta["object"] and next(ta, next(ta)) == nil
	local b_is_obj = tb and tb["object"] and next(tb, next(tb)) == nil
	if a_is_obj and b_is_obj and type(a.properties) == "table" and type(b.properties) == "table" then
		for k, pa in pairs(a.properties) do
			local pb = b.properties[k]
			if pb ~= nil and has_required_key(a, k) and has_required_key(b, k) then
				local pva, pvb = branch_const_values(pa), branch_const_values(pb)
				if pva and pvb and values_disjoint(pva, pvb) then
					return true
				end
			end
		end
	end
	return false
end

local function branches_provably_disjoint(branches)
	if type(branches) ~= "table" then
		return false
	end
	for i = 1, #branches do
		for j = i + 1, #branches do
			if not pair_provably_disjoint(branches[i], branches[j]) then
				return false
			end
		end
	end
	return true
end

local function append_constraint_notes(s, notes)
	if #notes == 0 then
		return
	end
	local suffix = table.concat(notes, "; ") .. "."
	if type(s.description) == "string" and s.description ~= "" then
		s.description = s.description .. "\n" .. suffix
	else
		s.description = suffix
	end
end

local sanitize_node

local function sanitize_schema_map(map, d)
	if type(map) ~= "table" then
		return map
	end
	local out = {}
	for k, v in pairs(map) do
		out[k] = type(v) == "table" and sanitize_node(v, d) or v
	end
	return out
end

local function sanitize_schema_list(list, d)
	if type(list) ~= "table" then
		return list
	end
	local out = {}
	for i, v in ipairs(list) do
		out[i] = type(v) == "table" and sanitize_node(v, d) or v
	end
	return out
end

sanitize_node = function(node, d)
	if type(node) ~= "table" then
		return node
	end
	local s = shallow_copy(node)

	-- 1. Recurse into schema-valued children only (never into default/const/enum/examples).
	if type(s.properties) == "table" then
		s.properties = sanitize_schema_map(s.properties, d)
	end
	if type(s.patternProperties) == "table" then
		s.patternProperties = sanitize_schema_map(s.patternProperties, d)
	end
	if type(s["$defs"]) == "table" then
		s["$defs"] = sanitize_schema_map(s["$defs"], d)
	end
	if type(s.definitions) == "table" then
		s.definitions = sanitize_schema_map(s.definitions, d)
	end
	if type(s.items) == "table" then
		s.items = is_array_table(s.items) and sanitize_schema_list(s.items, d) or sanitize_node(s.items, d)
	end
	if type(s.prefixItems) == "table" then
		s.prefixItems = sanitize_schema_list(s.prefixItems, d)
	end
	if type(s.additionalProperties) == "table" then
		s.additionalProperties = sanitize_node(s.additionalProperties, d)
	end
	if type(s.allOf) == "table" then
		s.allOf = sanitize_schema_list(s.allOf, d)
	end
	if type(s.anyOf) == "table" then
		s.anyOf = sanitize_schema_list(s.anyOf, d)
	end
	if type(s.oneOf) == "table" then
		s.oneOf = sanitize_schema_list(s.oneOf, d)
	end
	for _, key in ipairs({ "not", "if", "then", "else", "propertyNames", "contains" }) do
		if type(s[key]) == "table" then
			s[key] = sanitize_node(s[key], d)
		end
	end

	-- 2. Apply dialect-specific schema normalizations on this node.
	if d.openrouter_xgrammar and s.oneOf ~= nil and s.anyOf == nil then
		s.anyOf = s.oneOf
		s.oneOf = nil
	end

	if type(s.pattern) == "string" then
		if d.deepseek then
			s.pattern = M.unicode_null_escape(s.pattern)
		end
		if d.grok then
			s.pattern = M.ecma_class_for_rust(s.pattern)
		end
	end

	if d.kimi and s.type == nil then
		if s.enum ~= nil then
			local inferred = infer_enum_type(s.enum)
			if inferred ~= nil then
				s.type = inferred
			end
		elseif s.const ~= nil then
			local inferred = value_json_type(s.const)
			if inferred ~= nil then
				s.type = inferred
			end
		end
	end

	if d.google_vertex then
		if s.type == nil then
			local inferred = infer_vertex_type(s)
			if inferred ~= nil then
				s.type = inferred
			end
		end
		if s.const ~= nil then
			if s.enum ~= nil then
				local found = false
				for _, v in ipairs(s.enum) do
					if json_eq(v, s.const) then
						found = true
						break
					end
				end
				if not found then
					error("unsatisfiable schema: const value does not match enum")
				end
			end
			s.enum = { s.const }
			s.const = nil
		end
	end

	if d.anthropic and d.output_schema then
		local notes = {}
		if schema_has_type(s.type, "number") or schema_has_type(s.type, "integer") then
			for _, spec in ipairs(ANTHROPIC_NUM_CONSTRAINTS) do
				local k, label = spec[1], spec[2]
				if s[k] ~= nil then
					notes[#notes + 1] = label .. ": " .. tostring(s[k])
					s[k] = nil
				end
			end
		end
		if schema_has_type(s.type, "string") then
			for _, spec in ipairs(ANTHROPIC_STR_CONSTRAINTS) do
				local k, label = spec[1], spec[2]
				local unsupported = (k == "format" and not ANTHROPIC_SUPPORTED_FORMATS[s[k]])
					or (k == "pattern" and not anthropic_pattern_supported(s[k]))
					or (k ~= "format" and k ~= "pattern")
				if s[k] ~= nil and unsupported then
					notes[#notes + 1] = label .. ": " .. tostring(s[k])
					s[k] = nil
				end
			end
		end
		if schema_has_type(s.type, "object") and s.additionalProperties == nil then
			s.additionalProperties = false
		end
		append_constraint_notes(s, notes)
	end

	if d.openai_strict and d.output_schema then
		if s.propertyNames ~= nil then
			s.propertyNames = nil
		end
		if s.type == "object" and s.additionalProperties == nil then
			s.additionalProperties = false
		end
	end

	return s
end

local function is_flattenable_object_branch(b)
	if type(b) ~= "table" then
		return false
	end
	if b.type ~= nil and b.type ~= "object" then
		return false
	end
	if b.oneOf ~= nil or b.anyOf ~= nil or b.allOf ~= nil or b["not"] ~= nil or b["$ref"] ~= nil then
		return false
	end
	return true
end

local function adapt_anthropic_root_combinator(s, keyword)
	local branches = s[keyword]
	if branches == nil then
		return
	end
	if (s.type ~= nil and s.type ~= "object") or type(branches) ~= "table" or #branches == 0 then
		error("Anthropic Messages does not support top-level " .. keyword .. " on non-object or empty tool schemas")
	end
	for _, b in ipairs(branches) do
		if not is_flattenable_object_branch(b) then
			error(
				"Anthropic Messages does not support top-level "
					.. keyword
					.. " with non-object or nested combinator branches"
			)
		end
	end
	local merged_props = type(s.properties) == "table" and shallow_copy(s.properties) or {}
	local synthetic_anyof = {}
	for _, b in ipairs(branches) do
		if type(b.properties) == "table" then
			for k, prop in pairs(b.properties) do
				local existing = merged_props[k]
				if existing == nil then
					merged_props[k] = prop
				elseif not json_eq(existing, prop) then
					if synthetic_anyof[k] then
						local dup = false
						for _, v in ipairs(existing.anyOf) do
							if json_eq(v, prop) then
								dup = true
								break
							end
						end
						if not dup then
							existing.anyOf[#existing.anyOf + 1] = prop
						end
					else
						merged_props[k] = { anyOf = { existing, prop } }
						synthetic_anyof[k] = true
					end
				end
			end
		end
	end
	local req_out, req_seen = {}, {}
	if type(s.required) == "table" then
		for _, k in ipairs(s.required) do
			if type(k) == "string" and not req_seen[k] then
				req_seen[k] = true
				req_out[#req_out + 1] = k
			end
		end
	end
	if keyword == "allOf" then
		for _, b in ipairs(branches) do
			if type(b.required) == "table" then
				for _, k in ipairs(b.required) do
					if type(k) == "string" and not req_seen[k] then
						req_seen[k] = true
						req_out[#req_out + 1] = k
					end
				end
			end
		end
	else
		local first_req = branches[1].required
		if type(first_req) == "table" then
			for _, k in ipairs(first_req) do
				if type(k) == "string" and not req_seen[k] then
					local in_all = true
					for i = 2, #branches do
						if not has_required_key(branches[i], k) then
							in_all = false
							break
						end
					end
					if in_all then
						req_seen[k] = true
						req_out[#req_out + 1] = k
					end
				end
			end
		end
	end
	if s.additionalProperties == nil then
		local all_false = true
		for _, b in ipairs(branches) do
			if b.additionalProperties ~= false then
				all_false = false
				break
			end
		end
		if all_false then
			s.additionalProperties = false
		end
	end
	if keyword ~= "allOf" then
		local summaries = {}
		for _, b in ipairs(branches) do
			if type(b.required) == "table" and #b.required > 0 then
				summaries[#summaries + 1] = "required: " .. table.concat(b.required, ", ")
			elseif type(b.properties) == "table" and next(b.properties) ~= nil then
				local keys = {}
				for k in pairs(b.properties) do
					keys[#keys + 1] = tostring(k)
				end
				table.sort(keys)
				summaries[#summaries + 1] = "fields: " .. table.concat(keys, ", ")
			else
				summaries[#summaries + 1] = "empty object"
			end
		end
		local prefix = keyword == "oneOf" and "Must match exactly one variant: ("
			or "Must match at least one variant: ("
		append_constraint_notes(s, { prefix .. table.concat(summaries, ") | (") .. ")" })
	end
	s.type = "object"
	s.properties = merged_props
	s.required = #req_out > 0 and req_out or nil
	s[keyword] = nil
end

--- Sanitize one JSON schema for the given dialect (table or dialect/base_url/model descriptor).
--- Returns a fresh sanitized table without mutating `schema`.
function M.sanitize(schema, dialect, base_url, model_id, opts)
	if type(schema) ~= "table" then
		return schema
	end
	local d = type(dialect) == "table" and dialect or M.detect_dialect(dialect, base_url, model_id, opts)
	local out = sanitize_node(schema, d)
	if d.anthropic and not d.output_schema then
		adapt_anthropic_root_combinator(out, "allOf")
		adapt_anthropic_root_combinator(out, "oneOf")
		adapt_anthropic_root_combinator(out, "anyOf")
	end
	mare.json.copy_metadata(schema, out)
	return out
end

return M
