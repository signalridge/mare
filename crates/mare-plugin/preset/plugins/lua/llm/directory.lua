-- llm.directory: unified provider directory (code & data entries, config layer & overlay,
-- wire factory materialization, lifecycle disposal, and strict declaration-only attribution).
-- Part of the `llm` plugin (plugins/llm.lua), loaded during its apply.
local validate = require("llm.validate")

local active_instance

local M = {}

--- Query the current status of provider `name` from the active `llm` directory instance:
--- `{ state = "registered"|"disabled"|"failed"|"unresolved", reason?, plugin?, origin = "plugin"|"config"|"rpc", wire? } | nil`.
function M.status(name)
	return active_instance and active_instance.status(name) or nil
end

--- Return the materialized effective configuration for `name` from the active `llm` directory.
function M.effective_config(name)
	if active_instance and active_instance.effective_config then
		return active_instance.effective_config(name)
	end
	return (mare.config and mare.config.providers or {})[name] or {}
end

local function format_one_failure(plugin, f)
	if type(f) == "table" then
		if type(f.summary) == "string" and f.summary ~= "" then
			return f.summary
		end
		local phase = f.phase and (" " .. tostring(f.phase)) or ""
		return ("plugin %s%s: %s"):format(plugin, phase, tostring(f.error or "failed"))
	end
	if type(f) == "string" and f ~= "" then
		return f
	end
	return ("plugin %s failed"):format(plugin)
end

local function format_failure(plugin, failures)
	if type(failures) == "table" and #failures > 0 then
		local parts, seen = {}, {}
		for _, f in ipairs(failures) do
			local text = format_one_failure(plugin, f)
			if not seen[text] then
				seen[text] = true
				parts[#parts + 1] = text
			end
		end
		return table.concat(parts, "; ")
	end
	return format_one_failure(plugin, failures)
end

local function plugin_failure_record(plugin)
	local out = {}
	if mare.plugins and mare.plugins.failures then
		for _, f in ipairs(mare.plugins.failures() or {}) do
			if f.plugin == plugin then
				out[#out + 1] = f
			end
		end
	end
	if #out > 0 then
		return out
	end
	local st = mare.plugins and mare.plugins.status and mare.plugins.status(plugin)
	if st and st.failure then
		return { type(st.failure) == "table" and st.failure or { summary = tostring(st.failure) } }
	end
	return nil
end

local function is_plugin_disabled(plugin)
	if not (mare.plugins and mare.plugins.status) then
		return false
	end
	local st = mare.plugins.status(plugin)
	return st ~= nil and st.disabled == true
end

local function declaring(kind, name)
	if not (mare.plugins and mare.plugins.declaring) then
		return {}
	end
	return mare.plugins.declaring(kind, name) or {}
end

local function config_entry_for(name)
	local cfg_map = mare.config and mare.config.providers
	if type(cfg_map) ~= "table" then
		return nil
	end
	local pc = cfg_map[name]
	return type(pc) == "table" and pc or nil
end

--- Merge a config-layer overlay onto a plugin/RPC data entry.
--- `models` replaces the entire seed list; an explicit `wire` or `wire_api` in the overlay
--- clears the complementary field inherited from the base entry so wire switching is unambiguous.
local function apply_config_overlay(base, overlay)
	local out = table.copy(base)
	if type(overlay) ~= "table" then
		return out
	end
	if overlay.wire ~= nil and overlay.wire_api == nil then
		out.wire_api = nil
	elseif overlay.wire_api ~= nil and overlay.wire == nil then
		out.wire = nil
	end
	for k, v in pairs(overlay) do
		out[k] = v
	end
	return out
end

local function new_directory(D)
	local ctx, providers = D.ctx, D.providers
	local stacks = {} -- provider name -> ordered array of entry records (top = active)
	local doctor_problems = {} -- key -> message

	local dir = {}

	local function set_config_problem(rec, key, message)
		rec.problem_keys = rec.problem_keys or {}
		rec.problem_keys[key] = true
		doctor_problems[key] = message
	end

	local function set_rec_problem(rec, key, message)
		rec.problem_keys = rec.problem_keys or {}
		rec.problem_keys[key] = true
		if rec.plugin then
			validate.report_problem(rec.plugin, key, message)
		else
			doctor_problems[key] = message
		end
	end

	local function clear_rec_problems(rec)
		if not rec or not rec.problem_keys then
			return
		end
		for key in pairs(rec.problem_keys) do
			doctor_problems[key] = nil
			if rec.plugin then
				validate.clear_problem(rec.plugin, key)
			end
		end
		rec.problem_keys = nil
	end

	local function teardown_entry(pname, rec)
		if not rec or not rec.materialized then
			return false
		end
		local mat = rec.materialized
		rec.materialized = nil
		for _, dispose in ipairs(mat.param_disposers or {}) do
			pcall(dispose)
		end
		if providers[pname] == mat.provider then
			D.models.uninstall_provider(pname)
		end
		return true
	end

	local function materialize_data(pname, rec)
		clear_rec_problems(rec)
		local overlay = rec.origin ~= "config" and config_entry_for(pname) or nil
		local effective
		if rec.origin == "config" then
			effective = table.copy(rec.raw)
		else
			effective = apply_config_overlay(rec.raw, overlay)
		end
		effective.provider = pname
		rec.effective = effective

		local wire_name, route_hint, wire_err = validate.resolve_wire(pname, effective)
		if wire_err then
			rec.wire = nil
			rec.error = wire_err
			if type(overlay) == "table" and (overlay.wire ~= nil or overlay.wire_api ~= nil) then
				set_config_problem(rec, "provider:" .. pname, wire_err)
			else
				set_rec_problem(rec, "provider:" .. pname, wire_err)
			end
			return false
		end
		rec.wire = wire_name
		rec.error = nil

		local wire_obj = D.routes.get_wire(wire_name)
		if not wire_obj then
			-- Wire is absent (either not loaded yet, disabled, failed, or unknown);
			-- `dir.status` attributes missing wires via `mare.plugins.declaring("wires", wire_name)`.
			return false
		end

		if type(wire_obj.spec.provider) ~= "function" then
			rec.error = ("wire %s does not accept configured endpoints"):format(wire_name)
			return false
		end

		local ok_data, data_err, unknown, bad_field = validate.data_entry(pname, effective, wire_obj.spec)
		if not ok_data then
			rec.error = data_err
			if type(overlay) == "table" and bad_field and overlay[bad_field] ~= nil then
				set_config_problem(rec, "provider:" .. pname, data_err)
			else
				set_rec_problem(rec, "provider:" .. pname, data_err)
			end
			return false
		end
		for _, field in ipairs(unknown or {}) do
			local key = "provider-field:" .. pname .. ":" .. field
			local msg = ("provider %q: unknown field `%s`"):format(pname, field)
			if rec.origin == "config" or (type(overlay) == "table" and overlay[field] ~= nil) then
				set_config_problem(rec, key, msg)
			else
				set_rec_problem(rec, key, msg)
			end
		end

		local ok_factory, hooks = pcall(wire_obj.spec.provider, pname, effective)
		if not ok_factory or type(hooks) ~= "table" then
			local err_msg = ("provider %q: wire %s factory failed: %s"):format(
				pname,
				wire_name,
				ok_factory and ("expected table, got " .. type(hooks)) or tostring(hooks)
			)
			rec.error = err_msg
			set_rec_problem(rec, "provider:" .. pname, err_msg)
			return false
		end

		local param_rules = hooks.params
		local hooks_clean = table.copy(hooks)
		hooks_clean.params = nil
		hooks_clean.wire = hooks_clean.wire or wire_name
		hooks_clean.route = hooks_clean.route or route_hint or (wire_obj.describe and wire_obj.describe.route)

		local decl, cfg_err = validate.provider_from_config(pname, effective, hooks_clean, wire_obj.spec)
		if not decl then
			rec.error = cfg_err
			set_rec_problem(rec, "provider:" .. pname, cfg_err)
			return false
		end

		local ok_decl, err_key, reason = validate.provider(decl, {}, wire_obj.spec)
		if not ok_decl then
			rec.error = reason
			set_rec_problem(rec, err_key or ("provider:" .. pname), reason)
			return false
		end

		local entry = D.models.normalize_entry(pname, decl, false)
		local disposers = {}
		if type(param_rules) == "table" then
			local ok_params, p_err = pcall(function()
				for _, rule in ipairs(param_rules) do
					if type(rule) == "table" then
						local rcopy = table.copy(rule)
						rcopy.where = rcopy.where or (rec.plugin or ("wire:" .. wire_name))
						local _, dispose = D.params.define(nil, rcopy, "provider")
						disposers[#disposers + 1] = dispose
					end
				end
			end)
			if not ok_params then
				for _, dispose in ipairs(disposers) do
					pcall(dispose)
				end
				local err_msg = ("provider %q: wire %s params failed: %s"):format(pname, wire_name, tostring(p_err))
				rec.error = err_msg
				set_rec_problem(rec, "provider:" .. pname, err_msg)
				return false
			end
		end

		rec.materialized = {
			provider = entry,
			param_disposers = disposers,
			wire = wire_obj,
		}
		D.models.install_provider(pname, entry)
		return true
	end

	local function sync_top(pname)
		local st = stacks[pname]
		local top = st and st[#st] or nil
		if not top then
			if providers[pname] ~= nil then
				D.models.uninstall_provider(pname)
				return true
			end
			return false
		end
		if top.materialized then
			return false
		end
		if top.kind == "code" then
			top.materialized = { provider = top.entry, param_disposers = {} }
			D.models.install_provider(pname, top.entry)
			return true
		end
		return materialize_data(pname, top)
	end

	local function push_record(pname, rec)
		local st = stacks[pname]
		if not st then
			st = {}
			stacks[pname] = st
		end
		local prev_top = st[#st]
		local changed = false
		if prev_top and prev_top.materialized then
			teardown_entry(pname, prev_top)
			changed = true
		end
		st[#st + 1] = rec
		if sync_top(pname) then
			changed = true
		end

		local removed = false
		local function remove()
			if removed then
				return false, false
			end
			removed = true
			clear_rec_problems(rec)
			local cur = stacks[pname]
			if not cur then
				return false, false
			end
			local idx
			for i = #cur, 1, -1 do
				if cur[i] == rec then
					idx = i
					break
				end
			end
			if not idx then
				return false, false
			end
			local was_top = (idx == #cur)
			local had_mat = teardown_entry(pname, rec)
			table.remove(cur, idx)
			if #cur == 0 then
				stacks[pname] = nil
			end
			if not was_top then
				return true, false
			end
			local restored = sync_top(pname)
			return true, (had_mat or restored)
		end

		return remove, changed
	end

	--- Emit one coalesced `llm/providers-changed` notification.
	function dir.emit_changed()
		ctx.bus:emit("llm/providers-changed")
	end

	--- Push a validated code-tier provider entry onto the unified stack.
	function dir.push_code(pname, entry, meta)
		local pc = config_entry_for(pname)
		local has_config_wire = pc ~= nil and (pc.wire ~= nil or pc.wire_api ~= nil)
		local rec = {
			name = pname,
			kind = "code",
			origin = meta.origin or "plugin",
			plugin = meta.plugin,
			wire = entry.wire,
			entry = entry,
			config_wire_problem = has_config_wire,
		}
		if has_config_wire then
			set_rec_problem(
				rec,
				"config-wire:" .. pname,
				("provider %q is code-backed and ignores config `wire`/`wire_api`"):format(pname)
			)
		elseif meta.plugin then
			validate.clear_problem(meta.plugin, "config-wire:" .. pname)
		end
		return push_record(pname, rec)
	end

	--- Push a data-tier provider entry (no `stream`/`endpoint`) onto the unified stack.
	function dir.push_data(pname, raw_entry, meta)
		local rec = {
			name = pname,
			kind = "data",
			origin = meta.origin or "plugin",
			plugin = meta.plugin,
			raw = table.copy(raw_entry),
		}
		return push_record(pname, rec)
	end

	--- Initialize config-layer standalone provider entries synchronously from `mare.config.providers`
	--- (in-memory only: zero disk I/O, zero yields).
	function dir.init()
		local cfg_map = mare.config and mare.config.providers
		if type(cfg_map) ~= "table" then
			return
		end
		local names = {}
		for name, pc in pairs(cfg_map) do
			if type(name) == "string" and name ~= "" and type(pc) == "table" then
				names[#names + 1] = name
			end
		end
		table.sort(names)
		for _, name in ipairs(names) do
			local pc = cfg_map[name]
			local has_declaring_plugin = #declaring("providers", name) > 0
			local defines_endpoint = (type(pc.base_url) == "string" and pc.base_url ~= "")
				or pc.wire ~= nil
				or pc.wire_api ~= nil
			if not has_declaring_plugin and defines_endpoint then
				local rec = {
					name = name,
					kind = "data",
					origin = "config",
					plugin = nil,
					raw = table.copy(pc),
				}
				stacks[name] = { rec }
				sync_top(name)
			end
		end
	end

	--- Re-evaluate pending (or previously materialized on an older wire instance) data entries
	--- targeting `wire_name` when that wire registers or re-registers.
	function dir.on_wire_added(wire_name, wire_obj)
		local names = {}
		for pname, st in pairs(stacks) do
			local top = st[#st]
			if
				top
				and top.kind == "data"
				and top.wire == wire_name
				and (not top.materialized or top.materialized.wire ~= wire_obj)
			then
				names[#names + 1] = pname
			end
		end
		table.sort(names)
		local any_changed = false
		for _, pname in ipairs(names) do
			local st = stacks[pname]
			local top = st and st[#st]
			if top and top.materialized then
				teardown_entry(pname, top)
				any_changed = true
			end
			if sync_top(pname) then
				any_changed = true
			end
		end
		if any_changed then
			ctx.bus:emit("llm/providers-changed")
		end
	end

	--- Dispose materialized data entries backed by `wire_name` when that wire unregisters.
	function dir.on_wire_removed(wire_name, wire_obj)
		local names = {}
		for pname, st in pairs(stacks) do
			local top = st[#st]
			if
				top
				and top.kind == "data"
				and top.materialized
				and (top.wire == wire_name or top.materialized.wire == wire_obj)
			then
				names[#names + 1] = pname
			end
		end
		table.sort(names)
		local any_changed = false
		for _, pname in ipairs(names) do
			local st = stacks[pname]
			local top = st and st[#st]
			if top and teardown_entry(pname, top) then
				any_changed = true
			end
		end
		if any_changed then
			ctx.bus:emit("llm/providers-changed")
		end
	end

	--- Status & attribution for provider `name` strictly per effective `providers`/`wires` declarations.
	--- Returns `{ state = "registered"|"disabled"|"failed"|"unresolved", reason?, plugin?, origin, wire? } | nil`.
	function dir.status(name)
		if type(name) ~= "string" or name == "" then
			return nil
		end
		local st = stacks[name]
		local top = st and st[#st] or nil

		-- 1. Live registered provider
		local live = providers[name]
		if live ~= nil then
			return {
				state = "registered",
				origin = top and top.origin or "plugin",
				plugin = top and top.plugin or nil,
				wire = (top and top.wire) or live.wire,
			}
		end

		-- 2. Effective plugin declaring this provider name is disabled or failed
		for _, plug in ipairs(declaring("providers", name)) do
			if is_plugin_disabled(plug) then
				return {
					state = "disabled",
					plugin = plug,
					origin = "plugin",
					reason = ("provider %s is disabled (plugin %s) — enable it or pick another model with /model"):format(
						name,
						plug
					),
				}
			end
			local fail = plugin_failure_record(plug)
			if fail then
				return {
					state = "failed",
					plugin = plug,
					origin = "plugin",
					reason = ("provider %s is unavailable: %s — fix it and /reload"):format(
						name,
						format_failure(plug, fail)
					),
				}
			end
		end

		-- 3–5. Data entry exists in directory (plugin, config, or rpc), not materialized
		if top and top.kind == "data" then
			local wire_name = top.wire
			if wire_name then
				local wire_obj = D.routes.get_wire(wire_name)
				if not wire_obj then
					for _, wire_plug in ipairs(declaring("wires", wire_name)) do
						if is_plugin_disabled(wire_plug) then
							return {
								state = "disabled",
								plugin = wire_plug,
								origin = top.origin,
								wire = wire_name,
								reason = ("provider %s is unavailable: wire %s (plugin %s) is disabled"):format(
									name,
									wire_name,
									wire_plug
								),
							}
						end
						local fail = plugin_failure_record(wire_plug)
						if fail then
							return {
								state = "failed",
								plugin = wire_plug,
								origin = top.origin,
								wire = wire_name,
								reason = ("provider %s is unavailable: wire %s (plugin %s) failed: %s"):format(
									name,
									wire_name,
									wire_plug,
									format_failure(wire_plug, fail)
								),
							}
						end
					end
					return {
						state = "unresolved",
						plugin = top.plugin,
						origin = top.origin,
						wire = wire_name,
						reason = ("provider %s: unknown wire %s (%s)"):format(name, wire_name, top.origin),
					}
				end
				if type(wire_obj.spec.provider) ~= "function" then
					return {
						state = "unresolved",
						plugin = top.plugin,
						origin = top.origin,
						wire = wire_name,
						reason = ("wire %s does not accept configured endpoints"):format(wire_name),
					}
				end
			end
			return {
				state = "unresolved",
				plugin = top.plugin,
				origin = top.origin,
				wire = wire_name,
				reason = top.error or ("provider %s is unresolved"):format(name),
			}
		end

		local declared_by = declaring("providers", name)
		if #declared_by > 0 then
			return {
				state = "unresolved",
				plugin = declared_by[1],
				origin = "plugin",
				reason = ("provider %s was declared by plugin %s but did not register"):format(name, declared_by[1]),
			}
		end

		return nil
	end

	--- Effective configuration table for `name`: for directory-managed data entries, the
	--- declared base entry overlaid with live `mare.config.providers[name]` (or live config
	--- directly for config-origin entries); otherwise `mare.config.providers[name]`.
	function dir.effective_config(name)
		if type(name) ~= "string" or name == "" then
			return {}
		end
		local st = stacks[name]
		local top = st and st[#st] or nil
		local live_cfg = config_entry_for(name)
		if top and top.kind == "data" then
			if top.origin == "config" then
				return live_cfg or top.raw or {}
			end
			local base = top.raw or {}
			if type(live_cfg) == "table" then
				return apply_config_overlay(base, live_cfg)
			end
			return base
		end
		return live_cfg or {}
	end

	--- Live user configuration table (`mare.config.providers[name]`), if present.
	function dir.user_provider_config(name)
		if type(name) ~= "string" or name == "" then
			return nil
		end
		return config_entry_for(name)
	end

	--- Underlying plugin/preset data declaration for `name` (excluding user config entries).
	function dir.preset_config(name)
		if type(name) ~= "string" or name == "" then
			return nil
		end
		local st = stacks[name]
		local top = st and st[#st] or nil
		if top and top.kind == "data" and top.origin ~= "config" and type(top.raw) == "table" then
			return top.raw
		end
		return nil
	end

	--- Sorted list of all known provider names (live, pending/unresolved directory entries,
	--- and providers declared by enabled, failed, or disabled plugins).
	function dir.known_names()
		local seen, out = {}, {}
		local function add(n)
			if type(n) == "string" and n ~= "" and not seen[n] then
				seen[n] = true
				out[#out + 1] = n
			end
		end
		local function add_list(list)
			if type(list) == "table" then
				for _, n in ipairs(list) do
					add(n)
				end
			end
		end
		for n in pairs(providers) do
			add(n)
		end
		for n in pairs(stacks) do
			add(n)
		end
		if mare.plugins then
			if mare.plugins.list then
				for _, pl in ipairs(mare.plugins.list() or {}) do
					add_list(pl.providers)
				end
			end
			if mare.plugins.failures and mare.plugins.status then
				for _, f in ipairs(mare.plugins.failures() or {}) do
					if type(f.plugin) == "string" and f.plugin ~= "" then
						local pst = mare.plugins.status(f.plugin)
						add_list(pst and pst.providers)
					end
				end
			end
			if mare.plugins.status and type(mare.config) == "table" and type(mare.config.plugins) == "table" then
				for plug in pairs(mare.config.plugins) do
					if type(plug) == "string" and plug ~= "" and plug ~= "preset" then
						local pst = mare.plugins.status(plug)
						add_list(pst and pst.providers)
					end
				end
			end
		end
		table.sort(out)
		return out
	end

	ctx:on("kernel/doctor", function(checks)
		local keys = {}
		for k in pairs(doctor_problems) do
			keys[#keys + 1] = k
		end
		table.sort(keys)
		for _, k in ipairs(keys) do
			checks[#checks + 1] = {
				name = "provider directory (" .. k .. ")",
				ok = false,
				detail = doctor_problems[k],
			}
		end
	end)

	active_instance = dir
	if ctx and ctx.effect then
		ctx:effect(function()
			return function()
				if active_instance == dir then
					active_instance = nil
				end
			end
		end, "llm directory")
	end

	return dir
end

return setmetatable(M, {
	__call = function(_, D)
		return new_directory(D)
	end,
})
