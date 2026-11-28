-- llm.validate: declaration validation for providers, wires, and user config `providers.<id>` entries.
local M = {}

local function check_kind(v, kind)
	if kind == "nonempty_string" then
		return type(v) == "string" and v ~= "", "a non-empty string"
	elseif kind == "string" then
		return type(v) == "string", "a string"
	elseif kind == "positive_number" then
		return type(v) == "number" and v > 0, "a positive number"
	elseif kind == "nonnegative_number" then
		return type(v) == "number" and v >= 0, "a non-negative number"
	elseif kind == "boolean" then
		return type(v) == "boolean", "a boolean"
	elseif kind == "tool_update" then
		return v == "addition_only" or v == false, '"addition_only" or false'
	elseif kind == "function" then
		return type(v) == "function", "a function"
	elseif kind == "table" then
		return type(v) == "table", "a table"
	elseif kind == "function_or_table" then
		return type(v) == "function" or type(v) == "table", "a function or table"
	elseif kind == "function_or_string" then
		return type(v) == "function" or type(v) == "string", "a function or string"
	elseif kind == "string_or_boolean" then
		return type(v) == "string" or type(v) == "boolean", "a string or boolean"
	elseif kind == "string_array" then
		if type(v) ~= "table" then
			return false, "an array of strings"
		end
		local n = #v
		for k, item in pairs(v) do
			if math.type(k) ~= "integer" or k < 1 or k > n or type(item) ~= "string" or item == "" then
				return false, "an array of non-empty strings"
			end
		end
		return true
	elseif type(kind) == "string" and kind:sub(1, 5) == "enum:" then
		local allowed = {}
		for choice in kind:sub(6):gmatch("[^|]+") do
			if v == choice then
				return true
			end
			allowed[#allowed + 1] = choice
		end
		return false, "one of " .. table.concat(allowed, ", ")
	end
	return true
end

local PROVIDER_FIELDS = {
	provider = "nonempty_string",
	label = "string",
	default_model = "string",
	default_context = "positive_number",
	default_max_output = "positive_number",
	default_reasoning = "boolean",
	tool_update = "tool_update",
	no_key = "boolean",
	models = "table",
	stream = "function",
	wire = "nonempty_string",
	endpoint = "function",
	warm_url = "function",
	login = "function_or_table",
	login_label = "string",
	auth_keys = "string_array",
	oauth_key = "nonempty_string",
	oauth_label = "string",
	revoke = "function",
	auth_mode = "function_or_string",
	catalog_target = "function",
	catalog_scope = "function",
	catalog = "function",
	list_models = "function",
	on_unauthorized = "function",
	is_configured = "function",
	prepare = "function",
	compact = "function",
	tool_id_policy = "table",
	route = "nonempty_string",
	routes = "table",
	model_routes = "table",
	models_dev_routes = "boolean",
	unavailable_for = "function",
	errors = "table",
	aliases = "table",
	model_defaults = "table",
	hosted_tools = "table",
	models_dev = "string_or_boolean",
	key_prefixes = "string_array",
	cache_retention = "string",
	replace = "boolean",
}

local MODEL_FIELDS = {
	model_id = "nonempty_string",
	name = "string",
	route = "nonempty_string",
	protocol = "string",
	edit_tool = "string",
	context_window = "positive_number",
	context_window_measured = "boolean",
	max_output = "positive_number",
	reasoning = "boolean",
	images = "boolean",
	hidden = "boolean",
	tool_update = "tool_update",
	cache_retention = "string",
	max_output_default = "boolean",
	efforts = "string_array",
	default_effort = "string",
	cost = "table",
	truncation = "table",
	input = "table",
	params = "table",
	effort_map = "table",
	unavailable = "string_or_boolean",
}

local WIRE_FIELDS = {
	name = "nonempty_string",
	route = "string",
	path = "string",
	auth = "string_array",
	config = "table",
	errors = "table",
	hosted_tools = "string_array",
	encode = "function",
	decoder = "function",
	finalize = "function",
	error = "function",
	retry = "function",
	estimate_hints = "table",
	provider = "function",
	provider_fields = "table",
	model_fields = "table",
	enrich_model = "function",
}

local WIRE_APIS = {
	chat = { route = "chat", wire = "openai-chat" },
	responses = { route = "responses", wire = "openai-responses" },
	anthropic = { route = "anthropic", wire = "anthropic-messages" },
	google = { route = "google", wire = "google-generate" },
}
M.WIRE_APIS = WIRE_APIS

local DATA_PROVIDER_FIELDS = {
	provider = "nonempty_string",
	label = "string",
	wire = "nonempty_string",
	wire_api = "nonempty_string",
	route = "nonempty_string",
	base_url = "nonempty_string",
	dialect = "string",
	default_model = "string",
	default_context = "positive_number",
	default_max_output = "positive_number",
	default_reasoning = "boolean",
	tool_update = "tool_update",
	no_key = "boolean",
	models = "table",
	models_dev = "string_or_boolean",
	models_dev_routes = "boolean",
	routes = "table",
	model_routes = "table",
	aliases = "table",
	model_defaults = "table",
	hosted_tools = "table",
	api_key = "any",
	api_key_env = "string",
	headers = "table",
	query_params = "table",
	proxy = "string",
	retries = "nonnegative_number",
	stream_idle_timeout_ms = "positive_number",
	connect_timeout_ms = "positive_number",
	login_hint = "string",
	login_label = "string",
	list_models = "boolean",
	drop_fields = "table",
	transport = "string",
	cache_retention = "string",
	key_prefixes = "string_array",
	replace = "boolean",
}

local function is_array(t)
	if type(t) ~= "table" then
		return false
	end
	local n = #t
	local count = 0
	for k in pairs(t) do
		if math.type(k) ~= "integer" or k < 1 or k > n then
			return false
		end
		count = count + 1
	end
	return count == n
end

local function check_fields(tbl, schema, scope)
	for field, kind in pairs(schema) do
		local val = tbl[field]
		if val ~= nil then
			local ok, expected = check_kind(val, kind)
			if not ok then
				local got = (type(kind) == "string" and kind:sub(1, 5) == "enum:" and type(val) == "string")
						and ("%q"):format(val)
					or type(val)
				return false, field, ("expected %s for field `%s`, got %s"):format(expected, field, got), scope
			end
		end
	end
	return true
end

local function is_plain_map(tbl)
	return type(tbl) == "table" and (#tbl == 0 or next(tbl, #tbl) ~= nil)
end

local function collect_wire_fields(wire_specs, key)
	if type(wire_specs) ~= "table" then
		return nil
	end
	local out
	local function add(ws)
		local fields = type(ws) == "table" and type(ws[key]) == "table" and ws[key] or nil
		if fields then
			out = out or {}
			for k, v in pairs(fields) do
				out[k] = v
			end
		end
	end
	if #wire_specs > 0 then
		for _, ws in ipairs(wire_specs) do
			add(ws)
		end
	else
		add(wire_specs)
	end
	return out
end

local function validate_models(models, pname, extra_model_fields)
	local n = #models
	for k in pairs(models) do
		if math.type(k) ~= "integer" or k < 1 or k > n then
			return false, "models", ("provider %q: field `models` must be a list of model tables"):format(pname)
		end
	end
	local unknown = {}
	for i = 1, n do
		local m = models[i]
		if type(m) ~= "table" or (#m > 0) then
			return false,
				("models[%d]"):format(i),
				("provider %q: field `models[%d]` must be a model table"):format(pname, i)
		end
		if type(m.model_id) ~= "string" or m.model_id == "" then
			return false,
				("models[%d].model_id"):format(i),
				("provider %q: field `models[%d].model_id` must be a non-empty string"):format(pname, i)
		end
		local ok, field, why = check_fields(m, MODEL_FIELDS)
		if not ok then
			return false,
				("models[%d].%s"):format(i, field),
				("provider %q (%s): %s"):format(pname, ("models[%d]"):format(i), why)
		end
		if extra_model_fields then
			local eok, efield, ewhy = check_fields(m, extra_model_fields)
			if not eok then
				return false,
					("models[%d].%s"):format(i, efield),
					("provider %q (%s): %s"):format(pname, ("models[%d]"):format(i), ewhy)
			end
		end
		if m.id ~= nil then
			return false,
				("models[%d].id"):format(i),
				("provider %q: model field `id` was renamed to `model_id`"):format(pname)
		end
		if m.vision ~= nil then
			return false,
				("models[%d].vision"):format(i),
				("provider %q: model field `vision` was replaced by `images`"):format(pname)
		end
		if type(m.cost) == "table" then
			for _, ck in ipairs({ "input", "output", "cache_read", "cache_write" }) do
				if m.cost[ck] ~= nil and type(m.cost[ck]) ~= "number" then
					return false,
						("models[%d].cost.%s"):format(i, ck),
						("provider %q: field `models[%d].cost.%s` must be a number"):format(pname, i, ck)
				end
			end
		end
		for k in pairs(m) do
			if MODEL_FIELDS[k] == nil and (not extra_model_fields or extra_model_fields[k] == nil) then
				unknown[#unknown + 1] = ("models[%d].%s"):format(i, tostring(k))
			end
		end
	end
	return true, nil, nil, unknown
end

--- Validate a provider declaration passed to `llm:register_provider`.
--- Returns `ok, key, reason, unknown_fields`.
---@param p unknown
---@param existing_providers? table<string, table>
---@param wire_specs? table|table[]
---@return boolean, string, string, string[]|nil
function M.provider(p, existing_providers, wire_specs)
	if not is_plain_map(p) then
		return false,
			"provider:unnamed",
			("provider `<unnamed>`: field `<root>` — expected a provider table, got %s"):format(type(p))
	end
	if type(p.provider) ~= "string" or p.provider == "" then
		return false, "provider:unnamed", "provider `<unnamed>`: field `provider` — must be a non-empty string"
	end
	local pname = p.provider
	if existing_providers and existing_providers[pname] ~= nil and p.replace ~= true then
		return false,
			"provider:" .. pname,
			("provider %q: field `provider` — duplicate provider name (keeping first declaration)"):format(pname)
	end
	local extra_p = collect_wire_fields(wire_specs, "provider_fields")
	local extra_m = collect_wire_fields(wire_specs, "model_fields")
	local ok, field, why = check_fields(p, PROVIDER_FIELDS)
	if not ok then
		return false, "provider:" .. pname, ("provider %q: field `%s` — %s"):format(pname, field, why)
	end
	if extra_p then
		local eok, efield, ewhy = check_fields(p, extra_p)
		if not eok then
			return false, "provider:" .. pname, ("provider %q: field `%s` — %s"):format(pname, efield, ewhy)
		end
	end
	if type(p.login) == "table" then
		for k in pairs(p.login) do
			if k ~= "methods" and k ~= "run" then
				return false,
					"provider:" .. pname,
					("provider %q: field `login.%s` — unknown login field (expected methods, run)"):format(
						pname,
						tostring(k)
					)
			end
		end
		if type(p.login.run) ~= "function" then
			return false, "provider:" .. pname, ("provider %q: field `login.run` — must be a function"):format(pname)
		end
		local methods = p.login.methods
		if type(methods) ~= "table" or #methods == 0 then
			return false,
				"provider:" .. pname,
				("provider %q: field `login.methods` — must be a non-empty array of login methods"):format(pname)
		end
		local valid_methods = { oauth = true, device = true, api_key = true }
		local n = #methods
		for i = 1, n do
			if not valid_methods[methods[i]] then
				return false,
					"provider:" .. pname,
					("provider %q: field `login.methods` — unsupported method %q (expected oauth, device, api_key)"):format(
						pname,
						tostring(methods[i])
					)
			end
		end
		for k, item in pairs(methods) do
			if math.type(k) ~= "integer" or k < 1 or k > n or not valid_methods[item] then
				return false,
					"provider:" .. pname,
					("provider %q: field `login.methods` — unsupported method %q (expected oauth, device, api_key)"):format(
						pname,
						tostring(item)
					)
			end
		end
	end
	local unknown_models
	if p.models ~= nil then
		local mok, mfield, mwhy, munknown = validate_models(p.models, pname, extra_m)
		if not mok then
			return false, "provider:" .. pname, mwhy .. (" (field `%s`)"):format(mfield)
		end
		unknown_models = munknown
	end
	local unknown = {}
	for k in pairs(p) do
		if PROVIDER_FIELDS[k] == nil and (not extra_p or extra_p[k] == nil) then
			unknown[#unknown + 1] = tostring(k)
		end
	end
	for _, k in ipairs(unknown_models or {}) do
		unknown[#unknown + 1] = k
	end
	table.sort(unknown)
	return true, "", "", unknown
end

--- Normalize a service caller to a defined external plugin name, else `nil` (host/eval/RPC/self).
---@param caller table|nil
---@param service_ctx table|nil
---@param name string|nil
---@return string|nil
function M.plugin_caller(caller, service_ctx, name)
	if caller == nil or (service_ctx ~= nil and rawequal(caller, service_ctx)) then
		return nil
	end
	if type(name) ~= "string" or name == "" or name == "root" or name == "<root>" then
		return nil
	end
	if mare.plugins and mare.plugins.status and not mare.plugins.status(name).defined then
		return nil
	end
	return name
end

--- Report a declaration problem on `plugin` when `mare.plugins.report_problem` is installed.
---@param plugin string|nil
---@param key string
---@param message string
function M.report_problem(plugin, key, message)
	if plugin and mare.plugins and mare.plugins.report_problem then
		mare.plugins.report_problem(plugin, key, message)
	end
end

--- Clear a declaration problem on `plugin` when `mare.plugins.clear_problem` is installed.
---@param plugin string|nil
---@param key string
function M.clear_problem(plugin, key)
	if plugin and mare.plugins and mare.plugins.clear_problem then
		mare.plugins.clear_problem(plugin, key)
	end
end

--- Whether `caller_plugin` declared provider `pname` in its effective definition `providers = { ... }`.
function M.is_declared_provider(caller_plugin, pname)
	if not (mare.plugins and mare.plugins.declaring) then
		return true
	end
	for _, owner in ipairs(mare.plugins.declaring("providers", pname) or {}) do
		if owner == caller_plugin then
			return true
		end
	end
	return false
end

local ESTIMATE_HINT_FIELDS = {
	image_types = true,
	image_mime_keys = true,
	capped_fields = true,
}

local function validate_estimate_hints(hints, wire_name)
	if not is_plain_map(hints) then
		return false, ("register_wire(%s): field `estimate_hints` — expected a hint table"):format(wire_name)
	end
	for k in pairs(hints) do
		if type(k) ~= "string" or not ESTIMATE_HINT_FIELDS[k] then
			return false,
				("register_wire(%s): field `estimate_hints.%s` — unknown field (expected image_types, image_mime_keys, capped_fields)"):format(
					wire_name,
					tostring(k)
				)
		end
	end
	for _, list_field in ipairs({ "image_types", "image_mime_keys" }) do
		local list = hints[list_field]
		if list ~= nil then
			if type(list) ~= "table" then
				return false,
					("register_wire(%s): field `estimate_hints.%s` — expected a list of non-empty strings, got %s"):format(
						wire_name,
						list_field,
						type(list)
					)
			end
			local n = #list
			for k in pairs(list) do
				if math.type(k) ~= "integer" or k < 1 or k > n then
					return false,
						("register_wire(%s): field `estimate_hints.%s` — must be a dense list of non-empty strings"):format(
							wire_name,
							list_field
						)
				end
			end
			for i = 1, n do
				if type(list[i]) ~= "string" or list[i] == "" then
					return false,
						("register_wire(%s): field `estimate_hints.%s[%d]` — must be a non-empty string"):format(
							wire_name,
							list_field,
							i
						)
				end
			end
		end
	end
	if hints.capped_fields ~= nil then
		if not is_plain_map(hints.capped_fields) then
			return false,
				("register_wire(%s): field `estimate_hints.capped_fields` — expected a map of field name to non-negative integer"):format(
					wire_name
				)
		end
		for k, cap in pairs(hints.capped_fields) do
			if type(k) ~= "string" or k == "" then
				return false,
					("register_wire(%s): field `estimate_hints.capped_fields` — keys must be non-empty strings"):format(
						wire_name
					)
			end
			if math.type(cap) ~= "integer" or cap < 0 then
				return false,
					("register_wire(%s): field `estimate_hints.capped_fields.%s` — must be a non-negative integer"):format(
						wire_name,
						k
					)
			end
		end
	end
	return true
end

--- Resolve wire name and route from a data/config provider entry (`wire` / `wire_api`).
--- Returns `wire_name, route_name` or `nil, nil, err`.
function M.resolve_wire(id, entry)
	local wire_field = entry.wire
	local api_field = entry.wire_api
	if wire_field ~= nil and (type(wire_field) ~= "string" or wire_field == "") then
		return nil, nil, ("provider %q field `wire` must be a non-empty string"):format(id)
	end
	if api_field ~= nil and (type(api_field) ~= "string" or api_field == "") then
		return nil, nil, ("provider %q field `wire_api` must be a non-empty string"):format(id)
	end
	if api_field ~= nil then
		local info = WIRE_APIS[api_field]
		if not info then
			return nil,
				nil,
				("provider %q field `wire_api` has unknown value %q (expected chat, responses, anthropic, or google)"):format(
					id,
					api_field
				)
		end
		if wire_field ~= nil and wire_field ~= info.wire then
			return nil,
				nil,
				("provider %q specifies conflicting `wire` (%q) and `wire_api` (%q)"):format(id, wire_field, api_field)
		end
		return info.wire, info.route
	end
	if wire_field ~= nil then
		return wire_field, entry.route
	end
	return WIRE_APIS.chat.wire, WIRE_APIS.chat.route
end

--- Validate a data-tier provider entry against common data fields and optional wire `provider_fields`.
--- Returns `ok, err, unknown_fields`.
function M.data_entry(id, entry, wire_spec)
	if type(id) ~= "string" or id == "" then
		return false, "provider data entry needs a non-empty provider id"
	end
	if type(entry) ~= "table" then
		return false, ("provider %q data entry must be a table"):format(id)
	end
	if type(entry.base_url) ~= "string" or entry.base_url == "" then
		return false, ("provider %q config needs a non-empty `base_url` string"):format(id), nil, "base_url"
	end
	if entry.models ~= nil and not is_array(entry.models) then
		return false, ("provider %q config field `models` must be an array of model tables"):format(id), nil, "models"
	end
	local extra = collect_wire_fields(wire_spec, "provider_fields")
	local extra_m = collect_wire_fields(wire_spec, "model_fields")
	local ok, field, why = check_fields(entry, DATA_PROVIDER_FIELDS)
	if not ok then
		return false, ("provider %q: %s"):format(id, why), nil, field
	end
	if extra then
		local eok, efield, ewhy = check_fields(entry, extra)
		if not eok then
			return false, ("provider %q: %s"):format(id, ewhy), nil, efield
		end
	end
	local unknown_models
	if entry.models ~= nil then
		local mok, mfield, mwhy, munknown = validate_models(entry.models, id, extra_m)
		if not mok then
			return false, mwhy, nil, mfield
		end
		unknown_models = munknown
	end
	local unknown = {}
	for k in pairs(entry) do
		if type(k) == "string" and DATA_PROVIDER_FIELDS[k] == nil and (not extra or extra[k] == nil) then
			unknown[#unknown + 1] = k
		end
	end
	for _, k in ipairs(unknown_models or {}) do
		unknown[#unknown + 1] = k
	end
	table.sort(unknown)
	return true, nil, unknown
end

--- Validate a wire spec passed to `llm:register_wire`.
--- Returns `ok, key, message`.
---@param name unknown
---@param spec table
---@return boolean, string, string
function M.wire(name, spec)
	if type(name) ~= "string" or name == "" then
		return false, "wire:unnamed", "register_wire needs a wire name"
	end
	local key = "wire:" .. name
	if name:find(".", 1, true) ~= nil then
		return false, key, ("register_wire(%s): wire name must not contain `.`"):format(name)
	end
	if type(spec) ~= "table" then
		return false, key, ("register_wire(%s): spec must be a table"):format(name)
	end
	if type(spec.encode) ~= "function" then
		return false, key, ("register_wire(%s) needs spec.encode"):format(name)
	end
	if type(spec.decoder) ~= "function" then
		return false, key, ("register_wire(%s) needs spec.decoder"):format(name)
	end
	local ok, field, why = check_fields(spec, WIRE_FIELDS)
	if not ok then
		return false, key, ("register_wire(%s): field `%s` — %s"):format(name, field, why)
	end
	if spec.estimate_hints ~= nil then
		local hok, hwhy = validate_estimate_hints(spec.estimate_hints, name)
		if not hok then
			return false, key, hwhy
		end
	end
	for _, field_map_name in ipairs({ "provider_fields", "model_fields" }) do
		if spec[field_map_name] ~= nil then
			for k, v in pairs(spec[field_map_name]) do
				if type(k) ~= "string" or k == "" or type(v) ~= "string" or v == "" then
					return false,
						key,
						("register_wire(%s): `%s` entries must map non-empty field names to type strings"):format(
							name,
							field_map_name
						)
				end
			end
		end
	end
	return true, key, ""
end

--- Convert a user config `providers.<id>` table into a validated `mare.Provider` declaration.
--- Optional `opts` supplies runtime hooks (`stream`, `endpoint`, `login`, `is_configured`, `list_models`, etc.).
function M.provider_from_config(id, pc, opts, wire_spec)
	if type(id) ~= "string" or id == "" then
		return nil, "provider config entry needs a non-empty provider id"
	end
	if not is_plain_map(pc) then
		return nil, ("provider %q config must be a table"):format(id)
	end
	opts = opts or {}
	local base_url = pc.base_url or opts.base_url
	if type(base_url) ~= "string" or base_url == "" then
		return nil, ("provider %q config needs a non-empty `base_url` string"):format(id)
	end
	local api = pc.wire_api or pc.route or opts.route or "chat"
	if type(api) ~= "string" or api == "" then
		return nil, ("provider %q config field `wire_api` must be a non-empty string"):format(id)
	end
	local api_info = WIRE_APIS[api]
	local route = (api_info and api_info.route) or api
	if pc.wire ~= nil and opts.wire ~= nil and pc.wire ~= opts.wire then
		return nil,
			("provider %q config specifies wire %q, expected %q"):format(id, tostring(pc.wire), tostring(opts.wire))
	end
	local wire = pc.wire or opts.wire
	local models_copy
	if pc.models ~= nil then
		if not is_array(pc.models) then
			return nil, ("provider %q config field `models` must be an array of model tables"):format(id)
		end
		models_copy = pc.models
	elseif opts.models ~= nil then
		models_copy = opts.models
	else
		models_copy = {}
	end

	local function pick(a, b)
		if a ~= nil then
			return a
		end
		return b
	end

	local default_reasoning = pick(pc.default_reasoning, opts.default_reasoning)

	local decl = {
		provider = id,
		label = pc.label or opts.label or id,
		route = route,
		wire = wire,
		routes = pc.routes or opts.routes,
		model_routes = pc.model_routes or opts.model_routes,
		models_dev_routes = pick(pc.models_dev_routes, opts.models_dev_routes),
		default_model = pc.default_model or opts.default_model,
		default_context = pc.default_context or opts.default_context or 128000,
		default_max_output = pc.default_max_output or opts.default_max_output or 16384,
		default_reasoning = default_reasoning,
		tool_update = pick(pc.tool_update, opts.tool_update),
		no_key = pick(pc.no_key, opts.no_key),
		login_label = pc.login_label or opts.login_label,
		models_dev = pick(pc.models_dev, opts.models_dev),
		key_prefixes = pc.key_prefixes or opts.key_prefixes,
		tool_id_policy = pc.tool_id_policy or opts.tool_id_policy,
		errors = pc.errors or opts.errors,
		aliases = pc.aliases or opts.aliases,
		model_defaults = pc.model_defaults or opts.model_defaults,
		hosted_tools = pc.hosted_tools or opts.hosted_tools,
		cache_retention = pc.cache_retention or opts.cache_retention,
		models = models_copy,
		stream = opts.stream,
		endpoint = opts.endpoint,
		warm_url = opts.warm_url,
		login = opts.login,
		is_configured = opts.is_configured,
		catalog_target = opts.catalog_target,
		catalog_scope = opts.catalog_scope,
		catalog = opts.catalog,
		list_models = opts.list_models,
		on_unauthorized = opts.on_unauthorized,
		prepare = opts.prepare,
		compact = opts.compact,
		unavailable_for = opts.unavailable_for,
	}
	local extra_p = collect_wire_fields(wire_spec, "provider_fields")
	if extra_p then
		for k in pairs(extra_p) do
			if decl[k] == nil then
				decl[k] = pick(pc[k], opts[k])
			end
		end
	end
	local ok, _, reason = M.provider(decl, nil, wire_spec)
	if not ok then
		return nil, reason
	end
	return decl
end

return M
