return function(mare, deps)
	local cfg = deps.cfg
	local state = deps.state
	local C = deps.C or {}
	local STRUCT = deps.STRUCT
	local luatype = deps.luatype
	local where = deps.where
	local in_config_scope = deps.in_config_scope
	local merge = deps.merge
	local deepcopy = deps.deepcopy
	local validate_ui_config = deps.validate_ui_config
	local validate_working = deps.validate_working
	local validate_update_check = deps.validate_update_check
	local clear_sources = deps.clear_sources
	local closest = deps.closest
	local set_preset_state = deps.set_preset_state
	local check_options
	local function run_custom_validate(plugin, entry, v, path, say)
		if type(entry.validate) == "function" then
			return entry.validate(v, path, say)
		end
		if not plugin and path == "ui.working" and type(validate_working) == "function" then
			return validate_working(v, path, say)
		end
		if not plugin and path == "update_check" and type(validate_update_check) == "function" then
			return validate_update_check(v, path, say)
		end
		return true
	end

	local validation_layer
	-- while set, problems are collected here instead of becoming warnings (state.validate_options)
	local collecting
	local function warn(msg)
		if mare.text and mare.text.redact then
			msg = mare.text.redact(msg)
		end
		if collecting then
			table.insert(collecting, msg)
			return
		end
		local at = validation_layer or where()
		if at then
			msg = msg .. " (" .. at .. ")"
		end
		table.insert(state.warnings, msg)
		if mare.log then
			pcall(mare.log, "warn", "config: " .. msg)
		end
	end
	--- a notice that never refuses an invocation-only option write
	local function notice(msg)
		local previous = collecting
		collecting = nil
		warn(msg)
		collecting = previous
	end
	state.warn = warn

	-- declared below (one validator for plugin and core options); check_option uses it
	local check_entry

	local RETIRED_OPTIONS = {
		permission = "plugins.permissions.preset",
		working_indicator = "ui.working",
	}

	local RETIRED_PLUGIN_OPTIONS = {
		commands = {
			model_picker_key = { kind = "keymap", action = "model.picker", key = "<M-p>" },
			model_cycle_key = { kind = "keymap", action = "model.cycle", key = "<M-m>" },
			effort_down_key = { kind = "keymap", action = "effort.down", key = "<M-,>" },
			effort_up_key = { kind = "keymap", action = "effort.up", key = "<M-.>" },
		},
		agents = {
			keys = {
				kind = "keymap",
				action = "agents",
				key = "<Left>",
				hint = "rebind `agents` (`editor`, `<Left>`, `editor_empty`), `agents.back` (`transcript`, `<Esc>`), `agents.prev` (`transcript`, `<M-Left>`), or `agents.next` (`transcript`, `<M-Right>`) via `mare.keymap.set(...)` or pass `false` to disable",
			},
		},
		review = {
			key = { kind = "keymap", action = "review.viewer", key = "<C-r>" },
		},
		ui = {
			working_interval_ms = { kind = "option", target = "ui.working.text_throttle_ms" },
		},
		["provider-openai-compat"] = {
			endpoints = {
				hint = "define each endpoint with `mare.provider(name, {...})`",
			},
		},
	}

	local function retired_plugin_option_warning(plugin, key)
		local p = RETIRED_PLUGIN_OPTIONS[plugin]
		local spec = p and type(key) == "string" and p[key] or nil
		if not spec then
			return nil
		end
		if spec.kind == "option" then
			return ("plugin `%s` option `%s` has been retired; use `%s` instead"):format(plugin, key, spec.target)
		end
		if spec.hint then
			if spec.hint:sub(1, 7) == "plugin " or spec.hint:sub(1, 7) == "option " then
				return spec.hint
			end
			return ("plugin `%s` option `%s` has been retired; %s"):format(plugin, key, spec.hint)
		end
		return ('plugin `%s` option `%s` has been retired; rebind `%s` with `mare.keymap.set("editor", "<key>", "%s")` or disable the default (%s) with `mare.keymap.set("editor", "%s", false)`'):format(
			plugin,
			key,
			spec.action,
			spec.action,
			spec.key,
			spec.key
		)
	end

	local function check_option(name, value)
		if RETIRED_OPTIONS[name] then
			warn(("option `%s` has been retired; use `%s` instead"):format(name, RETIRED_OPTIONS[name]))
			return false
		end
		local o = mare.options[name] or (STRUCT[name] and { type = "table" })
		if not o then
			-- state.json also stores plugin-owned runtime keys (model_params, recent_models,
			-- onboarding, etc.) alongside core/plugin options; do not warn on those.
			if validation_layer == "state" then
				return true
			end
			local near = closest(name, mare.options)
			warn(("unknown option `%s`%s"):format(name, near and (" — did you mean `" .. near .. "`?") or ""))
			return false
		end
		if name == "ui" then
			if type(value) == "table" and value.working_interval_ms ~= nil then
				warn("option `ui.working_interval_ms` has been retired; use `ui.working.text_throttle_ms` instead")
				value.working_interval_ms = nil
			end
			validate_ui_config(value)
			if type(value) == "table" and value.working ~= nil and o.fields and o.fields.working then
				local committed_working = type(C.ui) == "table" and type(C.ui.working) == "table" and C.ui.working
					or nil
				if not check_entry(nil, o.fields.working, value.working, "ui.working", value, committed_working) then
					value.working = nil
				end
			end
		-- the same check as a plugin option's: an invalid value is dropped
		elseif value ~= nil and o.type and not check_entry(nil, o, value, name, nil, C[name]) then
			return false
		end
		if o.invocation_only and value ~= nil and not validation_layer then
			local note = o.invocation_note and (" (" .. o.invocation_note .. ")") or ""
			notice(name .. " only takes effect via --set or MARE_" .. name:upper() .. note)
		end
		return true
	end

	-- `plugins.preset`: data-driven preset table (single source for validation and introspection)
	local PLUGIN_PRESETS = {
		minimal = {
			desc = "Minimal plugin set (essential, minimal, and provider built-ins).",
			include_tags = { "minimal", "provider" },
		},
	}
	local plugin_preset_choices = {}
	for pname in pairs(PLUGIN_PRESETS) do
		plugin_preset_choices[#plugin_preset_choices + 1] = pname
	end
	table.sort(plugin_preset_choices)
	local PLUGIN_PRESET_ENTRY = {
		type = "enum",
		choices = plugin_preset_choices,
		on_invalid = "error",
		desc = "Plugin-set preset (e.g. minimal).",
		format_unknown = function(v, labels)
			return ("unknown plugins.preset `%s` (presets: %s); no preset applies"):format(
				tostring(v),
				table.concat(labels, ", ")
			)
		end,
	}
	cfg.plugin_presets = PLUGIN_PRESETS
	cfg.plugin_preset_entry = PLUGIN_PRESET_ENTRY
	local function check_plugin_preset(v)
		return check_entry(nil, PLUGIN_PRESET_ENTRY, v, "plugins.preset")
	end

	-- One check for every entry point (init.lua, --set, MARE_*, runtime writes): a value against its
	-- declaration `{ type, choices?, choices_from?, nullable?, env?, tag?, min?, max?, fields?, items?, values?, variants?, on_invalid? }`. Returns false
	-- when the value as a whole is invalid (the caller drops it, so the declared default stays); an
	-- object or map keeps its valid keys and drops the rest. `on_invalid = "error"` reports an error.
	local function article(t)
		return (t:match("^[aeiou]") and "an " or "a ") .. t
	end
	local function collect_dynamic_choices(tbl, into_set, into_extra)
		if type(tbl) ~= "table" then
			return
		end
		local n = #tbl
		if n > 0 then
			for i = 1, n do
				local item = tbl[i]
				if type(item) == "string" and not into_set[item] then
					into_set[item] = true
					into_extra[#into_extra + 1] = item
				end
			end
		else
			for k in pairs(tbl) do
				if type(k) == "string" and not into_set[k] then
					into_set[k] = true
					into_extra[#into_extra + 1] = k
				end
			end
		end
	end
	local function check_object(plugin, fields, v, path, entry, committed_obj)
		local atomic = entry and (entry.atomic or entry.on_invalid == "error" or entry.on_invalid == "fail")
		local ok = true
		for k, x in pairs(v) do
			local sub = type(k) == "string" and fields[k] or nil
			if not sub then
				local near = type(k) == "string" and closest(k, fields) or nil
				local subject = plugin and ("plugin `" .. plugin .. "`") or "core"
				local msg = ("%s has no option `%s.%s`%s"):format(
					subject,
					path,
					tostring(k),
					near and (" — did you mean `" .. path .. "." .. near .. "`?") or ""
				)
				warn((atomic and "error: " or "") .. msg)
				if atomic then
					if plugin and not collecting then
						if entry.on_invalid == "fail" and not state.stage then
							state.plugin_failed[plugin] = state.plugin_failed[plugin] or msg
						else
							state.plugin_error[plugin] = state.plugin_error[plugin] or msg
						end
					end
					ok = false
				else
					v[k] = nil
				end
			elseif not check_entry(plugin, sub, x, path .. "." .. k, v, committed_obj) then
				if atomic then
					ok = false
				else
					v[k] = nil
				end
			end
		end
		return ok
	end
	check_entry = function(plugin, entry, v, path, parent_tbl, committed_parent)
		if entry.nullable and (v == nil or luatype(v) == "null") then
			return true
		end
		local function say(msg)
			local fail = entry.on_invalid == "fail"
			local err = entry.on_invalid == "error"
			warn(((err or fail) and "error: " or "") .. msg)
			if plugin and not collecting then
				if fail and not state.stage then
					state.plugin_failed[plugin] = state.plugin_failed[plugin] or msg
				elseif err or (fail and state.stage) then
					state.plugin_error[plugin] = state.plugin_error[plugin] or msg
				end
			end
			return false
		end
		-- one validator for plugin options and core options (`plugin == nil`)
		local label = plugin and ("plugin `%s` option `%s`"):format(plugin, path) or ("option `%s`"):format(path)
		local want = entry.type
		if want == "union" then
			-- `{ type = "union", tag?, variants = { entry, … } }`: if `tag` is set and `v` is a
			-- table carrying that discriminator, validate directly against the matching variant.
			if type(entry.tag) == "string" and type(v) == "table" and v[entry.tag] ~= nil then
				local tv = v[entry.tag]
				for _, variant in ipairs(entry.variants or {}) do
					local tf = type(variant.fields) == "table" and variant.fields[entry.tag] or nil
					if type(tf) == "table" and type(tf.choices) == "table" then
						for _, ch in ipairs(tf.choices) do
							if ch == tv then
								return check_entry(plugin, variant, v, path, parent_tbl, committed_parent)
							end
						end
					end
				end
			end
			-- the value matches one form exactly (an unknown field or a wrong leaf fails that form
			-- rather than being dropped from it)
			local reasons = {}
			for _, variant in ipairs(entry.variants or {}) do
				local previous = collecting
				collecting = {}
				check_entry(
					plugin,
					variant,
					type(v) == "table" and deepcopy(v) or v,
					path,
					parent_tbl,
					committed_parent
				)
				local problems = collecting
				collecting = previous
				if #problems == 0 then
					return true
				end
				reasons[#reasons + 1] = problems[1]
			end
			return say(("%s matches none of its forms: %s"):format(label, table.concat(reasons, "; or ")))
		end
		if type(want) == "table" then
			-- plain alternatives (`{ "string", "table" }`): the value is one of these Lua types
			local actual = luatype(v)
			for _, t in ipairs(want) do
				if t == actual or (t == "integer" and math.type(v) == "integer") then
					return run_custom_validate(plugin, entry, v, path, say)
				end
			end
			return say(("%s expects %s, got %s"):format(label, article(table.concat(want, " or ")), actual))
		end
		local actual = luatype(v)
		local expected = ({
			integer = "number",
			list = "table",
			object = "table",
			map = "table",
			union = "table",
		})[want]
		if expected == nil and want ~= "enum" then
			expected = want
		end
		if want == "enum" and (not entry.choices or #entry.choices == 0) then
			expected = "string"
		end
		if expected and actual ~= expected then
			return say(("%s expects %s, got %s"):format(label, article(want == "enum" and "string" or want), actual))
		end
		if want == "integer" and math.type(v) ~= "integer" and v ~= math.floor(v) then
			return say(("%s expects an integer, got %s"):format(label, tostring(v)))
		end
		if want == "number" or want == "integer" then
			if entry.min and v < entry.min then
				return say(("%s: %s is below the minimum %s"):format(label, tostring(v), tostring(entry.min)))
			end
			if entry.max and v > entry.max then
				return say(("%s: %s is above the maximum %s"):format(label, tostring(v), tostring(entry.max)))
			end
		end
		if (entry.choices or entry.choices_from) and (want == "enum" or want == "string") then
			local ok = false
			local labels, set = {}, {}
			for _, c in ipairs(entry.choices or {}) do
				ok = ok or c == v
				local s = tostring(c)
				if not set[s] then
					set[s] = true
					labels[#labels + 1] = s
				end
			end
			if type(entry.choices_from) == "string" then
				local extra = {}
				if type(parent_tbl) == "table" then
					collect_dynamic_choices(parent_tbl[entry.choices_from], set, extra)
				end
				if type(committed_parent) == "table" then
					collect_dynamic_choices(committed_parent[entry.choices_from], set, extra)
				end
				table.sort(extra)
				for _, s in ipairs(extra) do
					labels[#labels + 1] = s
				end
				if type(v) == "string" and set[v] then
					ok = true
				end
			end
			if #labels > 0 and not ok then
				if entry.format_unknown then
					return say(entry.format_unknown(v, labels))
				end
				local near = type(v) == "string" and closest(v, set) or nil
				return say(
					("%s: `%s` is not one of %s%s"):format(
						label,
						tostring(v),
						table.concat(labels, ", "),
						near and (" — did you mean `" .. near .. "`?") or ""
					)
				)
			end
		end
		if want == "list" then
			local count = 0
			for k in pairs(v) do
				if type(k) ~= "number" or math.type(k) ~= "integer" or k < 1 then
					return say(("%s expects a list, got non-list key `%s`"):format(label, tostring(k)))
				end
				count = count + 1
			end
			for i = 1, count do
				if v[i] == nil then
					return say(("%s expects a contiguous list"):format(label))
				end
			end
			if entry.max_items and count > entry.max_items then
				return say(("%s allows at most %d items (got %d)"):format(label, entry.max_items, count))
			end
			if entry.items then
				for i = 1, count do
					if not check_entry(plugin, entry.items, v[i], ("%s[%d]"):format(path, i)) then
						return false -- a list is one value: invalid as a whole
					end
				end
			end
		elseif (want == "object" or want == "table") and type(entry.fields) == "table" then
			local nested_committed = type(committed_parent) == "table" and committed_parent or nil
			if not check_object(plugin, entry.fields, v, path, entry, nested_committed) then
				return false
			end
		elseif want == "map" and entry.values then
			for k, x in pairs(v) do
				if not check_entry(plugin, entry.values, x, path .. "." .. tostring(k)) then
					v[k] = nil
				end
			end
		end
		return run_custom_validate(plugin, entry, v, path, say)
	end

	-- `plugins.<name>.enabled`: the slot switch, not a plugin option, for every known
	-- plugin whether or not it declares a config_schema. Not a boolean: warned, and false is returned
	-- so the caller drops this layer's value (an earlier layer's stays).
	local function check_enabled(name, v)
		return check_entry(name, { type = "boolean", default = true }, v, "enabled")
	end

	local deferred_plugins = {}
	local plugins_declared = false
	local rechecking_deferred = false

	local function check_plugin(name, t)
		if name == "preset" then
			return check_plugin_preset(t)
		end
		local schema = state.plugin_schemas[name]
		if schema then
			if t ~= nil and type(t) ~= "table" then
				warn(("plugin `%s` expects a table, got %s"):format(name, luatype(t)))
				return true
			end
			local committed_plugin = type(C.plugins) == "table" and type(C.plugins[name]) == "table" and C.plugins[name]
				or nil
			for k, v in pairs(t or {}) do
				if k == "enabled" then
					if not check_enabled(name, v) then
						t[k] = nil
					end
				elseif type(k) == "string" and schema[k] then
					if not check_entry(name, schema[k], v, k, t, committed_plugin) then
						t[k] = nil
					end
				else
					local retired_msg = retired_plugin_option_warning(name, k)
					if retired_msg then
						warn(retired_msg)
					else
						local near = type(k) == "string" and closest(k, schema) or nil
						warn(
							("plugin `%s` has no option `%s`%s"):format(
								name,
								tostring(k),
								near and (" — did you mean `" .. near .. "`?") or ""
							)
						)
					end
					t[k] = nil
				end
			end
			return true
		end
		if t ~= nil and type(t) ~= "table" then
			warn(("plugin `%s` expects a table, got %s"):format(name, luatype(t)))
			return false
		end
		local defaults = state.plugin_defaults[name]
		if next(state.plugin_defaults) ~= nil and defaults == nil then
			if
				(in_config_scope() or (validation_layer ~= nil and not plugins_declared))
				and not collecting
				and not rechecking_deferred
			then
				-- Package plugins are declared only after config files/CLI layers finish (merge_package_opts),
				-- so defer unknown names during config evaluation instead of falsely warning mid-eval.
				-- Non-config calls warn immediately.
				deferred_plugins[#deferred_plugins + 1] = {
					name = name,
					opts = type(t) == "table" and deepcopy(t) or t,
					at = validation_layer or where(),
				}
				return false
			end
			local near = closest(name, state.plugin_defaults)
			warn(("unknown plugin `%s`%s"):format(name, near and (" — did you mean `" .. near .. "`?") or ""))
			return false
		elseif defaults then
			-- a plugin that declares no options (no `config_schema`): only the slot switch
			for k, v in pairs(t or {}) do
				if k == "enabled" then
					if not check_enabled(name, v) then
						t[k] = nil
					end
				else
					local retired_msg = retired_plugin_option_warning(name, k)
					warn(
						retired_msg
							or ("plugin `%s` has no option `%s` (it declares no config_schema)"):format(
								name,
								tostring(k)
							)
					)
					t[k] = nil
				end
			end
			return true
		elseif type(t) == "table" and t.enabled ~= nil then
			if not check_enabled(name, t.enabled) then
				t.enabled = nil
			end
			return true
		end
		return false
	end

	-- Shared soft contract for file APIs, invocation layers, and runtime option writes.
	check_options = function(t)
		if type(t) ~= "table" then
			return
		end
		for name, value in pairs(t) do
			-- an unknown name only warns (it may be read by code that registers it later); a known
			-- option's invalid value is taken out, so its default stays
			if not check_option(name, value) and (mare.options[name] or RETIRED_OPTIONS[name]) then
				t[name] = nil
			end
			if name == "plugins" and type(value) == "table" then
				for plugin, opts in pairs(value) do
					local defaults = mare.options.plugins.default
					if defaults[plugin] ~= nil then
						if type(opts) ~= type(defaults[plugin]) then
							warn(
								("option `plugins.%s`: expected %s, got %s"):format(
									plugin,
									type(defaults[plugin]),
									type(opts)
								)
							)
							value[plugin] = nil
						end
					else
						local ok = check_plugin(plugin, opts)
						if plugin == "preset" then
							if ok == false then
								value.preset = nil
								if not collecting then
									-- `on_invalid = "error"`: invalid preset clears candidate state only
									set_preset_state(false)
								end
							elseif ok == true and not collecting then
								set_preset_state(true)
							end
						end
					end
				end
			end
		end
	end
	--- The problems `t` (option → value, `plugins` included) has, without warning or writing
	--- anything: what a runtime write must not commit.
	function state.validate_options(t)
		local previous = collecting
		collecting = {}
		local ok, err = pcall(check_options, t)
		local problems = collecting
		collecting = previous
		if not ok then
			problems[#problems + 1] = tostring(err)
		end
		return problems
	end

	--- Re-check deferred plugin entries once package plugins have been declared in merge_package_opts.
	--- Uses each entry's saved location `at` and prunes invalid keys from `plugins`.
	local function check_deferred_plugins(plugins)
		plugins_declared = true
		local entries = deferred_plugins
		deferred_plugins = {}
		if #entries == 0 then
			return nil
		end
		local prev_layer = validation_layer
		local prev_recheck = rechecking_deferred
		rechecking_deferred = true
		local rebuilt = {}
		local ok, err = pcall(function()
			for _, d in ipairs(entries) do
				validation_layer = d.at
				local opts = d.opts
				if check_plugin(d.name, opts) and type(opts) == "table" then
					rebuilt[d.name] = rebuilt[d.name] or {}
					merge(rebuilt[d.name], opts)
				end
			end
		end)
		validation_layer = prev_layer
		rechecking_deferred = prev_recheck
		if not ok then
			error(err, 0)
		end
		if type(plugins) ~= "table" or next(rebuilt) == nil then
			return nil
		end
		for name, clean in pairs(rebuilt) do
			if type(plugins[name]) == "table" then
				for k in pairs(plugins[name]) do
					if clean[k] == nil then
						plugins[name][k] = nil
						clear_sources("plugins." .. name .. "." .. tostring(k))
					else
						plugins[name][k] = clean[k]
					end
				end
			end
		end
		return plugins
	end

	--- Check an invocation layer (command line, environment) like any other entry point and return
	--- it with the invalid values of declared plugin options taken out (their defaults stay).
	--- When `flush_deferred` is true, re-check deferred plugin options against newly declared package
	--- plugins and return the pruned `plugins` table (or nil).
	function state.check_config(t, layer, flush_deferred)
		if flush_deferred then
			return check_deferred_plugins(t)
		end
		local previous = validation_layer
		validation_layer = layer
		local ok, err = pcall(check_options, t)
		validation_layer = previous
		if not ok then
			error(err, 0)
		end
		return t
	end

	return { check_options = check_options, check_plugin = check_plugin }
end
