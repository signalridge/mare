-- Shared configuration sensitive-path redaction and metadata description helpers.
-- Installed on `mare` once at VM creation; reads live `mare.config`, `mare.o`, `mare.options`,
-- `mare.config_sources`, and `mare.config_sensitive_paths` dynamically at call time so reload
-- staging views and rollback stay consistent across coroutines.
local mare, core = ...
local cfg = core.config
local function S()
	return cfg.state or {}
end

local function hidden_key(key, value)
	local k = tostring(key):lower():gsub("[^%w]", "")
	if k:find("token", 1, true) ~= nil then
		if k:find("tokens", 1, true) ~= nil then
			-- Keep numeric/boolean token counts (e.g. max_tokens = 4096) while redacting
			-- credential collections and string values (e.g. auth_tokens = { ... } / "...").
			if value ~= nil then
				return type(value) == "string" or type(value) == "table"
			end
			return true
		end
		return true
	end
	return k:find("apikey", 1, true) ~= nil
		or k:find("secret", 1, true) ~= nil
		or k:find("password", 1, true) ~= nil
		or k:find("passwd", 1, true) ~= nil
		or k:find("credential", 1, true) ~= nil
		or k:find("privatekey", 1, true) ~= nil
		or k:find("authorization", 1, true) ~= nil
		or k:find("cookie", 1, true) ~= nil
		or k == "bearer"
		or k == "headers"
		or k == "envhttpheaders"
		or k == "query"
		or k == "queryparams"
		or k == "queryparameters"
		or k == "env"
end

local function is_marked(path, sensitive)
	if type(path) ~= "string" or path == "" then
		return false
	end
	for prefix in pairs(sensitive) do
		if path == prefix or path:sub(1, #prefix + 1) == prefix .. "." then
			return true
		end
	end
	return false
end

-- a plugin `config_schema` entry, or anything under it, declared `sensitive` along `parts` from
-- index `i` (an object descends by `fields`, a map by `values` for any key, a list by `items` for
-- any index, a union through any variant that leads on)
local function schema_sensitive(e, parts, i)
	if type(e) ~= "table" then
		return false
	end
	if e.sensitive == true then
		return true
	end
	if i > #parts then
		return false
	end
	if e.type == "union" then
		for _, v in ipairs(e.variants or {}) do
			if schema_sensitive(v, parts, i) then
				return true
			end
		end
		return false
	end
	if e.type == "map" then
		-- a map key may itself contain dots (`srv["api.example.com"]`): any reading of the
		-- following segments as one key that reaches a `sensitive` declaration counts (lazy#46 #1)
		for j = i, #parts do
			if schema_sensitive(e.values, parts, j + 1) then
				return true
			end
		end
		return false
	end
	local next_entry = nil
	if type(e.fields) == "table" then
		next_entry = e.fields[parts[i]]
	elseif e.type == "list" then
		next_entry = e.items
	end
	return schema_sensitive(next_entry, parts, i + 1)
end

-- declared sensitive at `path` or a parent of it: a core option (`mare.options[..].sensitive`)
-- or a plugin option in its `config_schema` (b41 CFG #2)
local function is_option_sensitive(path)
	if type(path) ~= "string" or path == "" then
		return false
	end
	local cur = ""
	for part in string.gmatch(path, "[^%.]+") do
		if cur == "" then
			cur = part
		else
			cur = cur .. "." .. part
		end
		local def = mare.options and mare.options[cur]
		if def and def.sensitive then
			return true
		end
	end
	local plugin, rest = path:match("^plugins%.([^%.]+)%.(.+)$")
	local schemas = S().plugin_schemas
	local schema = plugin and schemas and schemas[plugin]
	if type(schema) ~= "table" then
		return false
	end
	local parts = {}
	for part in rest:gmatch("[^%.]+") do
		parts[#parts + 1] = part
	end
	return schema_sensitive(schema[parts[1]], parts, 2)
end

local function is_numeric_or_bool_option(path)
	local def = mare.options and mare.options[path]
	return def and (def.type == "number" or def.type == "integer" or def.type == "boolean")
end

local function is_sensitive_path(path, sensitive, value)
	if type(path) ~= "string" or path == "" then
		return false
	end
	sensitive = sensitive or (mare.config_sensitive_paths and mare.config_sensitive_paths()) or {}
	if is_marked(path, sensitive) or is_option_sensitive(path) then
		return true
	end
	local parts = {}
	for part in string.gmatch(path, "[^%.]+") do
		parts[#parts + 1] = part
	end
	for i, part in ipairs(parts) do
		if i < #parts then
			if hidden_key(part) then
				return true
			end
		else
			if value ~= nil then
				if hidden_key(part, value) then
					return true
				end
			elseif not is_numeric_or_bool_option(path) and hidden_key(part) then
				return true
			end
		end
	end
	return #parts == 0 and hidden_key(path, value)
end

local function redact_walk(path, v, force, sensitive, seen)
	if v == nil then
		return nil, false
	end
	if
		force
		or is_marked(path, sensitive)
		or is_option_sensitive(path)
		or hidden_key(path:match("[^%.]+$") or path, v)
	then
		return "[REDACTED]", true
	end
	if type(v) == "string" then
		-- URLs may carry credentials in arbitrary query keys or userinfo.
		if
			v:match("%a[%w+%.%-]*://")
			and (v:find("?", 1, true) or v:find("#", 1, true) or v:match("%a[%w+%.%-]*://[^/%s]*@"))
		then
			return "[REDACTED]", true
		end
		local out = (mare.text and mare.text.redact) and mare.text.redact(v) or v
		return out, out ~= v
	end
	if type(v) ~= "table" then
		return v, false
	end
	if next(v) == nil then
		local mt = getmetatable(v)
		if mt and mt.__pairs then
			return {}, false
		end
		return v, false
	end
	seen = seen or {}
	if seen[v] then
		return "[recursive]", false
	end
	seen[v] = true
	local out, any_redacted = {}, false
	local prefix = (type(path) == "string" and path ~= "") and (path .. ".") or ""
	for key, value in pairs(v) do
		local safe, child_redacted = redact_walk(prefix .. tostring(key), value, false, sensitive, seen)
		out[key] = safe
		if child_redacted then
			any_redacted = true
		end
	end
	seen[v] = nil
	return out, any_redacted
end

function mare.config_is_sensitive(path)
	return is_sensitive_path(path)
end

function mare.config_redact(path, value)
	local sensitive = (mare.config_sensitive_paths and mare.config_sensitive_paths()) or {}
	local p = type(path) == "string" and path or tostring(path or "")
	-- every sensitivity rule lives here (marks, core and plugin declarations, keywords): callers
	-- pass no flag they could forget
	local force = is_sensitive_path(p, sensitive, value)
	return redact_walk(p, value, force, sensitive)
end

local function split_dots(s)
	local out = {}
	for seg in string.gmatch(s, "[^%.]+") do
		out[#out + 1] = seg
	end
	return out
end

local function walk_path(root, parts, start_idx)
	local cur = root
	for i = start_idx, #parts do
		if type(cur) ~= "table" then
			return nil
		end
		cur = cur[parts[i]]
	end
	return cur
end

local function config_at(tbl, key)
	if type(tbl) ~= "table" then
		return nil
	end
	if tbl[key] ~= nil then
		return tbl[key]
	end
	if type(key) == "string" and key:find(".", 1, true) then
		return walk_path(tbl, split_dots(key), 1)
	end
	return nil
end

local function deep_equal(a, b)
	if a == b then
		return true
	end
	if type(a) ~= "table" or type(b) ~= "table" then
		return false
	end
	for k, v in pairs(a) do
		if not deep_equal(v, b[k]) then
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

local slot_preset_enabled

local function resolve_value(k, overrides)
	if overrides and overrides[k] ~= nil then
		return overrides[k], true
	end
	-- a path below an overridden key reads inside that value, so a leaf and its whole option
	-- report the same live state
	if overrides and type(k) == "string" then
		local parts = split_dots(k)
		for n = #parts - 1, 1, -1 do
			local base = overrides[table.concat(parts, ".", 1, n)]
			if base ~= nil then
				local v = base
				for i = n + 1, #parts do
					if type(v) == "table" then
						v = v[parts[i]] -- may be false
					else
						v = nil
					end
				end
				return v, true
			end
		end
	end
	local o = rawget(mare, "o")
	local C = mare.config or {}
	if k == "plugins" then
		-- The option proxy merges defaults lazily; materialize this known config table so
		-- the shared redactor sees its actual fields rather than an empty proxy shell.
		local value = {}
		local defaults = mare.options and mare.options.plugins and mare.options.plugins.default
		for key, item in pairs(type(defaults) == "table" and defaults or {}) do
			value[key] = item
		end
		for key, item in pairs(type(C.plugins) == "table" and C.plugins or {}) do
			value[key] = item
		end
		return value, false
	end
	if o then
		local ok, v = pcall(function()
			return o[k]
		end)
		if ok and v ~= nil then
			return v, false
		end
	end
	if C[k] ~= nil then
		return C[k], false
	end
	if type(k) == "string" and k:find(".", 1, true) then
		local parts = split_dots(k)
		if #parts >= 2 then
			local root = nil
			local overridden = false
			if overrides and overrides[parts[1]] ~= nil then
				root = overrides[parts[1]]
				overridden = true
			elseif parts[1] == "plugins" and type(C.plugins) == "table" then
				root = C.plugins
			elseif o then
				local ok, v = pcall(function()
					return o[parts[1]]
				end)
				if ok and v ~= nil then
					root = v
				end
			end
			if root == nil then
				root = C[parts[1]]
			end
			local v = walk_path(root, parts, 2)
			if v == nil and not overridden and parts[1] == "plugins" and o then
				local ok, proxy = pcall(function()
					return o.plugins
				end)
				if ok then
					v = walk_path(proxy, parts, 2)
				end
			end
			if v ~= nil then
				return v, overridden
			end
			if parts[1] == "plugins" and #parts >= 3 then
				local pdefs = S().plugin_defaults
				local pdef = pdefs and pdefs[parts[2]]
				local dv = walk_path(pdef, parts, 3)
				if dv ~= nil then
					return dv, false
				end
				if #parts == 3 and parts[3] == "enabled" then
					local slots = S().plugin_slots
					if type(slots) == "table" and slots[parts[2]] then
						local pv = slot_preset_enabled(parts[2], overrides)
						if pv ~= nil then
							return pv, false
						end
						return true, false
					end
				end
			end
		end
	end
	return nil, false
end

cfg.resolve_config_key = resolve_value

slot_preset_enabled = function(name, overrides)
	local slots = S().plugin_slots
	local slot = type(slots) == "table" and slots[name] or nil
	if type(slot) ~= "table" or not slot.builtin then
		return nil, nil
	end
	local preset = resolve_value("plugins.preset", overrides)
	local presets = cfg.plugin_presets
	local spec = type(presets) == "table" and type(preset) == "string" and presets[preset] or nil
	if type(spec) ~= "table" or type(spec.include_tags) ~= "table" then
		return nil, nil
	end
	local include = {}
	for _, tag in ipairs(spec.include_tags) do
		include[tag] = true
	end
	for _, tag in ipairs(slot.tags or {}) do
		if include[tag] then
			return nil, nil
		end
	end
	return false, "preset " .. preset
end

local function plugin_meta(k)
	if type(k) ~= "string" then
		return nil, nil
	end
	if k == "plugins.preset" then
		local entry = cfg.plugin_preset_entry
		local ty = (type(entry) == "table" and entry.choices) and table.concat(entry.choices, "|") or "enum"
		return nil, ty, (type(entry) == "table" and entry.desc) or "Plugin-set preset."
	end
	local name, rest = k:match("^plugins%.([^%.]+)%.(.+)$")
	if not name then
		return nil, nil
	end
	if rest == "enabled" then
		local slots = S().plugin_slots
		if type(slots) == "table" and slots[name] then
			return true, "boolean", "Whether this plugin slot is active."
		end
	end
	local parts = split_dots(rest)
	local s = S()
	local def = walk_path(s.plugin_defaults and s.plugin_defaults[name], parts, 1)
	-- the declaration at this path (`config_schema`; an object's `fields` below it)
	local entry = s.plugin_schemas and s.plugin_schemas[name] and s.plugin_schemas[name][parts[1]]
	for i = 2, #parts do
		entry = (type(entry) == "table" and type(entry.fields) == "table") and entry.fields[parts[i]] or nil
	end
	local function label(e)
		if type(e.type) == "table" then
			return table.concat(e.type, "|")
		elseif e.type == "union" then
			-- the forms: an enum shows its words, anything else its type
			local forms = {}
			for _, v in ipairs(e.variants or {}) do
				forms[#forms + 1] = (v.type == "enum" and v.choices) and table.concat(v.choices, "|") or label(v)
			end
			return table.concat(forms, "|")
		end
		return e.type
	end
	local ty = entry and label(entry) or nil
	if ty and entry and entry.nullable then
		ty = ty .. "|null"
	end
	if ty == nil and def ~= nil then
		ty = type(def)
	end
	return def, ty, entry and entry.desc or nil
end

--- Every declared plugin option, leaf by leaf (`plugins.<plugin>.<path>`; an object's fields are
--- its leaves, a list, map or union is one), sorted: `/config` and `config.describe` list these
--- whether or not anything set them.
function mare.config_declared()
	local out, seen = {}, {}
	local function add(k)
		if not seen[k] then
			seen[k] = true
			out[#out + 1] = k
		end
	end
	local function walk(prefix, entry)
		if type(entry) ~= "table" then
			return
		end
		if entry.type == "object" and type(entry.fields) == "table" and next(entry.fields) ~= nil then
			for k, sub in pairs(entry.fields) do
				walk(prefix .. "." .. k, sub)
			end
		else
			add(prefix)
		end
	end
	add("plugins.preset")
	local s = S()
	local slots = s.plugin_slots
	for name in pairs(type(slots) == "table" and slots or {}) do
		add("plugins." .. name .. ".enabled")
	end
	for name, schema in pairs(s.plugin_schemas or {}) do
		if type(schema) == "table" then
			for k, entry in pairs(schema) do
				walk("plugins." .. name .. "." .. k, entry)
			end
		end
	end
	table.sort(out)
	return out
end

function mare.config_describe(keys, opts)
	local overrides = type(opts) == "table" and opts.overrides or nil
	local sources = (mare.config_sources and mare.config_sources()) or S().sources or {}
	local C = mare.config or {}
	local target_keys
	if type(keys) == "table" and #keys > 0 then
		target_keys = keys
	else
		local seen = {}
		target_keys = {}
		local function add_keys(tbl)
			if type(tbl) ~= "table" then
				return
			end
			for k in pairs(tbl) do
				if type(k) == "string" and not seen[k] then
					seen[k] = true
					target_keys[#target_keys + 1] = k
				end
			end
		end
		add_keys(mare.options)
		add_keys(C)
		add_keys(sources)
		add_keys(overrides)
		add_keys(mare.config_sensitive_paths and mare.config_sensitive_paths() or nil)
		for _, k in ipairs(mare.config_declared()) do
			if not seen[k] then
				seen[k] = true
				target_keys[#target_keys + 1] = k
			end
		end
		table.sort(target_keys)
	end
	local out = {}
	for _, k in ipairs(target_keys) do
		local def = mare.options and mare.options[k]
		local pdef, ptype, pdesc = plugin_meta(k)
		-- a false default is a default: never `x and false or y`
		local def_raw = pdef
		if def and def.default ~= nil then
			def_raw = def.default
		end
		local eff_raw, overridden = resolve_value(k, overrides)
		local eff_safe, eff_redacted = mare.config_redact(k, eff_raw)
		local def_safe, def_redacted = mare.config_redact(k, def_raw)
		local cfg_val = config_at(C, k)
		local s = sources[k]
		local src = nil
		if overridden and (not s or not deep_equal(cfg_val, eff_raw)) then
			src = { layer = "runtime" }
		elseif s then
			src = { layer = s.layer, where = s.where }
		elseif cfg_val ~= nil then
			src = { layer = "runtime" }
		elseif def_raw ~= nil and eff_raw ~= nil then
			local pname = type(k) == "string" and k:match("^plugins%.([^%.]+)%.enabled$") or nil
			local preset_layer = nil
			if pname then
				local pv, layer = slot_preset_enabled(pname, overrides)
				if pv ~= nil and pv == eff_raw then
					preset_layer = layer
				end
			end
			src = { layer = preset_layer or "default" }
		end
		local ty = (def and def.type) or ptype
		if type(ty) == "table" then
			ty = table.concat(ty, "|")
		end
		out[k] = {
			effective = eff_safe,
			default = def_safe,
			source = src,
			description = (def and def.desc) or pdesc,
			type = ty,
			sensitive = (def and def.sensitive == true) or mare.config_is_sensitive(k),
			redacted = eff_redacted or def_redacted,
		}
	end
	return out
end
