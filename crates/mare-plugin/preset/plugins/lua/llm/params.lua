-- llm.params: the model-parameters engine ( 模型参数's
-- ModelParameterDefinition / RequestedModel{model_id, parameters} shapes, no code taken).
-- Parameters (enum or bool: effort, verbosity, thinking …) and how each option maps to request
-- fields per route are data, merged by layer: built-in < provider < model < user < trusted
-- project < agent < session < request. Part of the `llm` plugin (plugins/llm.lua).
--
-- A rule: { provider =, route =, family =, match = "<provider/id glob>", when = fn(model),
--   define = { <param> = { name, type = "enum"|"bool", tooltip, default, cycleable, statusline,
--     visible, on_unsupported, options = { "v" | { value, name, cost, tooltip, wire } },
--     only = { values } (this model's options, in order), aliases = { from = to },
--     wire = { <route> = { ["a.b"] = v | "$value" | false (remove) | { at_least = n } | { value = v }
--       } | fn(req, ctx) } } },
--   remove = { "<param>" } | { <param> = { values } }, on_unsupported = "clamp"|"error"|"as_is" }
-- A partial definition (no `type`) only changes a parameter some layer defines.
return function(D)
	local ctx, state, llm = D.ctx, D.state, D.llm

	local LAYERS = { "builtin", "provider", "model", "user", "project", "agent", "session", "request" }
	local RANK = {}
	for i, l in ipairs(LAYERS) do
		RANK[l] = i
	end
	local TYPES = { enum = true, bool = true }
	local POLICIES = { clamp = true, error = true, as_is = true }
	local SCALARS = { "name", "tooltip", "cycleable", "statusline", "visible", "on_unsupported", "affects_cache" }

	local P = { LAYERS = LAYERS }
	local plugin_rules = {} -- provider-layer rules registered by plugins (define_params)
	local seq, version = 0, 0
	local problems = {} -- rule errors, shown by /doctor (file and line when known)
	local cache = setmetatable({}, { __mode = "k" }) -- model table -> {version, route, defs}

	-- built-in layer: the effort parameter and its fallback default (formerly llm.lua's "medium")
	local BUILTIN_RULE = {
		define = {
			effort = {
				name = "Effort",
				type = "enum",
				tooltip = "How much the model thinks before answering",
				default = "medium",
				cycleable = true,
				statusline = true,
				on_unsupported = "clamp",
				options = D.EFFORTS,
			},
		},
	}

	local function glob(p)
		return "^" .. p:gsub("[%^%$%(%)%%%.%[%]%+%-]", "%%%0"):gsub("%*", ".*"):gsub("%?", ".") .. "$"
	end

	--- validates and normalizes a rule; nil, err when it is malformed
	local function normalize(rule, layer, where)
		if type(rule) ~= "table" then
			return nil, "a params rule must be a table"
		end
		local r = {
			layer = layer,
			where = where or rule.where,
			provider = rule.provider,
			route = rule.route,
			family = rule.family,
			match = rule.match,
			when = rule.when,
			model_ref = rule.model_ref,
			define = {},
			remove = rule.remove,
			on_unsupported = rule.on_unsupported,
		}
		if r.on_unsupported ~= nil and not POLICIES[r.on_unsupported] then
			return nil, ("on_unsupported must be clamp, error or as_is, not %q"):format(tostring(r.on_unsupported))
		end
		for name, d in pairs(rule.define or {}) do
			if type(d) ~= "table" then
				return nil, ("define.%s must be a table"):format(tostring(name))
			end
			if d.type ~= nil and not TYPES[d.type] then
				return nil, ("define.%s: unknown type %q (enum, bool)"):format(name, tostring(d.type))
			end
			if d.on_unsupported ~= nil and not POLICIES[d.on_unsupported] then
				return nil, ("define.%s: on_unsupported must be clamp, error or as_is"):format(name)
			end
			local opts
			if type(d.options) == "table" then
				opts = {}
				for i, o in ipairs(d.options) do
					o = type(o) == "table" and o or { value = o }
					if o.value == nil then
						return nil, ("define.%s.options[%d] has no value"):format(name, i)
					end
					if d.type == "enum" and type(o.value) ~= "string" then
						return nil, ("define.%s.options[%d]: an enum value is a string"):format(name, i)
					end
					opts[#opts + 1] = o
				end
			end
			local nd = {}
			for k, v in pairs(d) do
				nd[k] = v
			end
			nd.options = opts
			r.define[name] = nd
		end
		-- specificity inside a layer: provider < route < family or glob < one model
		local spec = 0
		if r.provider then
			spec = 1
		end
		if r.route then
			spec = 2
		end
		if r.family or (r.match and r.match:find("[%*%?]")) then
			spec = 3
		end
		if r.model_ref or (r.match and not r.match:find("[%*%?]")) then
			spec = 4
		end
		r.spec = spec
		seq = seq + 1
		r.seq = seq
		return r
	end

	-- routes are open: the ones something provides (llm/routes: a registered wire's
	-- describe.route, a provider's route / routes) and the ones a plugin's own rules map. An
	-- init.lua or mare.model rule naming another route stays inert and /doctor says so (checked
	-- when asked, so the order plugins register in doesn't matter)
	local function known_routes()
		local known = D.routes.known_routes()
		for _, r in ipairs(plugin_rules) do
			if r.route then
				known[r.route] = true
			end
			for _, d in pairs(r.define) do
				for route in pairs(type(d.wire) == "table" and d.wire or {}) do
					known[route] = true
				end
				for _, o in ipairs(d.options or {}) do
					for route in pairs(type(o.wire) == "table" and o.wire or {}) do
						known[route] = true
					end
				end
			end
		end
		return known
	end

	--- the first route of rule `r` nobody provides, as a problem line | nil
	local function unknown_route(r, known)
		if r.route ~= nil and not known[r.route] then
			local names = {}
			for k in pairs(known) do
				names[#names + 1] = tostring(k)
			end
			table.sort(names)
			return ("unknown route %q (%s)"):format(tostring(r.route), table.concat(names, ", "))
		end
		for name, d in pairs(r.define) do
			for route in pairs(type(d.wire) == "table" and d.wire or {}) do
				if not known[route] then
					return ("define.%s: unknown route %q in wire"):format(name, tostring(route))
				end
			end
			for i, o in ipairs(d.options or {}) do
				for route in pairs(type(o.wire) == "table" and o.wire or {}) do
					if not known[route] then
						return ("define.%s.options[%d]: unknown route %q"):format(name, i, tostring(route))
					end
				end
			end
		end
		return nil
	end

	local BUILTIN = assert(normalize(BUILTIN_RULE, "builtin", "built-in"))
	BUILTIN.spec, BUILTIN.seq = 0, 0

	-- the route a model's requests take (llm/routes: per model for a provider with several)
	local function route_of(model)
		local _, _, wire_route = D.routes.route_of(model)
		return wire_route
	end
	P.route_of = route_of

	local function matches(r, model, route)
		if r.provider and r.provider ~= model.provider then
			return false
		end
		if r.route and r.route ~= route then
			return false
		end
		if r.model_ref and r.model_ref ~= model.ref then
			return false
		end
		if r.match then
			local target = r.match:find("/", 1, true) and model.ref or model.id
			if not tostring(target or ""):match(glob(r.match)) then
				return false
			end
		end
		if r.family and not tostring(model.id or ""):find("%f[%w]" .. r.family:gsub("%p", "%%%0") .. "%f[%W]") then
			return false
		end
		if r.when then
			local ok, yes = pcall(r.when, model)
			if not ok or not yes then
				return false
			end
		end
		return true
	end

	-- the model layer: what the catalogue says (reasoning, efforts, effort_map, verbosity), then
	-- `mare.model{ params = }` (definitions, or plain values = that model's defaults)
	local function model_rules(model)
		local derived = { layer = "model", where = "model " .. tostring(model.ref), spec = 4, seq = 0, define = {} }
		if not model.reasoning then
			derived.remove = { "effort" }
		else
			local effort_opts
			if type(model.efforts) == "table" then
				effort_opts = {}
				for _, v in ipairs(model.efforts) do
					effort_opts[#effort_opts + 1] = { value = v }
				end
			end
			local fallback_only = { "low", "medium", "high" }
			if
				type(model.efforts) ~= "table"
				and type(model.default_effort) == "string"
				and model.default_effort ~= ""
			then
				local seen = false
				for _, v in ipairs(fallback_only) do
					if v == model.default_effort then
						seen = true
						break
					end
				end
				if not seen then
					fallback_only[#fallback_only + 1] = model.default_effort
				end
			end
			derived.define.effort = {
				only = model.efforts or fallback_only,
				options = effort_opts,
				default = model.default_effort,
				aliases = model.effort_map,
			}
		end
		if model.verbosity then
			derived.define.verbosity = {
				name = "Verbosity",
				type = "enum",
				tooltip = "How long and detailed the answers are",
				default = model.verbosity,
				on_unsupported = "as_is",
				options = { { value = "low" }, { value = "medium" }, { value = "high" } },
			}
		end
		local out = { derived }
		if type(model.params) == "table" then
			local define = {}
			for k, v in pairs(model.params) do
				define[k] = type(v) == "table" and v or { default = v }
			end
			local r, err = normalize({ define = define }, "model", "mare.model " .. tostring(model.ref))
			if r then
				r.spec, r.seq = 4, 0.5
				out[#out + 1] = r
				local bad = unknown_route(r, known_routes())
				problems["model:" .. tostring(model.ref)] = bad and ("mare.model %s: %s"):format(model.ref, bad) or nil
			else
				problems["model:" .. tostring(model.ref)] = ("mare.model %s: %s"):format(model.ref, err)
			end
		end
		return out
	end

	-- the user and trusted-project rules from init.lua (`mare.params{}`), normalized once per
	-- config table
	local config_rules, config_src
	local function from_config()
		local list = mare.config.param_rules
		if list == config_src and config_rules then
			return config_rules
		end
		config_src, config_rules = list, {}
		for k in pairs(problems) do
			if type(k) == "string" and k:find("^config:") then
				problems[k] = nil
			end
		end
		for i, raw in ipairs(type(list) == "table" and list or {}) do
			local layer = (type(raw) == "table" and raw._layer == "project") and "project" or "user"
			local r, err = normalize(raw, layer, type(raw) == "table" and raw._where or nil)
			if r then
				config_rules[#config_rules + 1] = r
			else
				local where = type(raw) == "table" and raw._where
				problems["config:" .. i] = ("mare.params%s: %s"):format(where and (" (" .. where .. ")") or "", err)
				mare.log("warn", problems["config:" .. i])
			end
		end
		version = version + 1
		return config_rules
	end

	local function new_def(name, order)
		return {
			id = name,
			options = {},
			index = {},
			gone = {},
			wire = {},
			wire_src = {},
			aliases = {},
			src = {},
			order = order,
		}
	end

	local function merge_option(def, o, rule)
		local cur = def.index[o.value]
		def.gone[o.value] = nil
		if not cur then
			cur = { value = o.value, wire = {} }
			def.index[o.value] = cur
			def.options[#def.options + 1] = cur
		end
		for k, v in pairs(o) do
			if k == "wire" then
				for route, m in pairs(v) do
					cur.wire[route] = m
					cur.wire_src = cur.wire_src or {}
					cur.wire_src[route] = rule
				end
			elseif k ~= "value" then
				cur[k] = v
			end
		end
	end

	local function apply_rule(defs, r, counter)
		local function remove(name, vals)
			if vals == true or vals == nil then
				defs[name] = nil
				return
			end
			local d = defs[name]
			if not d then
				return
			end
			for _, v in ipairs(type(vals) == "table" and vals or { vals }) do
				d.index[v] = nil
				d.gone[v] = true
				for i = #d.options, 1, -1 do
					if d.options[i].value == v then
						table.remove(d.options, i)
					end
				end
			end
			d.removed = d.removed or {}
			d.removed[#d.removed + 1] = r
		end
		if type(r.remove) == "table" then
			for k, v in pairs(r.remove) do
				if type(k) == "number" then
					remove(v, true)
				else
					remove(k, v)
				end
			end
		elseif type(r.remove) == "string" then
			remove(r.remove, true)
		end
		local names = {}
		for name in pairs(r.define or {}) do
			names[#names + 1] = name
		end
		table.sort(names)
		for _, name in ipairs(names) do
			local d = r.define[name]
			local cur = defs[name]
			if not cur then
				counter.n = counter.n + 1
				cur = new_def(name, counter.n)
				defs[name] = cur
			end
			if d.type ~= nil then
				cur.type = d.type
				cur.src.type = r
			end
			for _, k in ipairs(SCALARS) do
				if d[k] ~= nil then
					cur[k] = d[k]
					cur.src[k] = r
				end
			end
			if d.default ~= nil then
				cur.default = d.default
				cur.src.default = r
			end
			for _, o in ipairs(d.options or {}) do
				merge_option(cur, o, r)
			end
			if type(d.only) == "table" then
				cur.only = d.only
				cur.src.only = r
			end
			if type(d.aliases) == "table" then
				for k, v in pairs(d.aliases) do
					cur.aliases[k] = v
				end
			end
			if type(d.wire) == "table" then
				for route, m in pairs(d.wire) do
					cur.wire[route] = m
					cur.wire_src[route] = r
				end
			end
		end
		if r.on_unsupported then
			for _, d in pairs(defs) do
				d.on_unsupported = r.on_unsupported
				d.src.on_unsupported = r
			end
		end
	end

	local function order_rules(list)
		table.sort(list, function(a, b)
			if RANK[a.layer] ~= RANK[b.layer] then
				return RANK[a.layer] < RANK[b.layer]
			end
			if a.spec ~= b.spec then
				return a.spec < b.spec
			end
			return a.seq < b.seq
		end)
		return list
	end

	--- the merged parameter definitions for `model` on `route` (cached per model table)
	function P.defs(model, route)
		route = route or route_of(model)
		local cfg_rules = from_config()
		local c = cache[model]
		if c and c.version == version and c.route == route and c.config == cfg_rules then
			return c.defs, c.rules
		end
		local list = { BUILTIN }
		for _, r in ipairs(plugin_rules) do
			if matches(r, model, route) then
				list[#list + 1] = r
			end
		end
		for _, r in ipairs(model_rules(model)) do
			list[#list + 1] = r
		end
		for _, r in ipairs(cfg_rules) do
			if matches(r, model, route) then
				list[#list + 1] = r
			end
		end
		order_rules(list)
		local defs, counter = {}, { n = 0 }
		for _, r in ipairs(list) do
			apply_rule(defs, r, counter)
		end
		for name, d in pairs(defs) do
			if not d.type then
				defs[name] = nil
			else
				-- rank: the position in the full option list (the nearest-level rule); `only` keeps
				-- this model's options in its own order, an undefined one ranks as legacy (nil)
				d.rank = {}
				for i, o in ipairs(d.options) do
					d.rank[o.value] = i
				end
				if d.type == "bool" then
					d.list = { true, false }
				elseif d.only then
					d.list = {}
					for _, v in ipairs(d.only) do
						if not d.gone[v] then
							d.list[#d.list + 1] = v
						end
					end
				else
					d.list = {}
					for _, o in ipairs(d.options) do
						d.list[#d.list + 1] = o.value
					end
				end
			end
		end
		cache[model] = { version = version, route = route, config = cfg_rules, defs = defs, rules = list }
		return defs, list
	end

	--- the parameters of `model`, in definition order
	function P.list(model, route)
		local defs = P.defs(model, route)
		local out = {}
		for _, d in pairs(defs) do
			out[#out + 1] = d
		end
		table.sort(out, function(a, b)
			return a.order < b.order
		end)
		return out
	end

	local function offers(d, v)
		for _, x in ipairs(d.list) do
			if x == v then
				return true
			end
		end
		return false
	end
	P.offers = offers

	--- nearest offered option (lower on a tie); an alias (a model's effort_map) wins first. The
	--- ranks come from the full option list: an option no layer defines ranks as the middle (4)
	--- for the distance and lowest for the tie, the long-standing effort rule.
	function P.clamp(d, want)
		if d.type == "bool" then
			return want
		end
		local mapped = d.aliases[want]
		if mapped then
			return mapped
		end
		local r = d.rank
		local mid = r.medium or 4
		local w = r[want] or mid
		local best, bestd = nil, math.huge
		for _, e in ipairs(d.list) do
			local dist = math.abs((r[e] or mid) - w)
			if dist < bestd or (dist == bestd and (r[e] or 0) < (r[best] or 0)) then
				best, bestd = e, dist
			end
		end
		return best
	end

	--- whether the route's mapping for `d` passes any value through (`$value`)
	local function passes_value(d, route)
		local m = d.wire[route]
		if type(m) ~= "table" then
			return false
		end
		for _, v in pairs(m) do
			if v == "$value" then
				return true
			end
		end
		return false
	end

	-- state.json `model_params`, read once at load and kept current by llm.remember_param
	local function remembered(ref)
		local t = type(state.remembered) == "table" and state.remembered[ref]
		return type(t) == "table" and t or {}
	end

	--- select(model, opts) -> {params, sources, wanted, notes, error, defs, route}: each
	--- parameter's value by layer, then the unsupported-option policy. opts: request = {p = v},
	--- only = {p = true}, route
	function P.select(model, opts)
		opts = opts or {}
		local route = opts.route or route_of(model)
		local defs = P.defs(model, route)
		local R = { params = {}, sources = {}, wanted = {}, notes = {}, defs = defs, route = route, model = model }
		local mem
		-- a sub-agent's selections (`mare.subagent.start{ params = }`, with what its parent handed
		-- down: llm.subagent_params)
		local agent = mare.mode == "child" and type(mare.args) == "table" and mare.args.params
		agent = type(agent) == "table" and agent or nil
		for name, d in pairs(defs) do
			if opts.only and not opts.only[name] then
				goto continue
			end
			local best, brank, bspec, bsrc
			local function offer(layer, spec, v, where)
				if v == nil then
					return
				end
				local rk = RANK[layer]
				if not best or rk > brank or (rk == brank and spec >= bspec) then
					best, brank, bspec, bsrc = v, rk, spec, { layer = layer, where = where }
				end
			end
			local dr = d.src.default
			if dr then
				offer(dr.layer, dr.spec, d.default, dr.where)
			end
			mem = mem or remembered(model.ref)
			offer("user", -1, mem[name], "state.json")
			local sess = state.params[model.ref]
			offer("session", 0, sess and sess[name], "session")
			local scope_name = state.active_scope
			if not opts.ignore_scope and type(scope_name) == "string" and scope_name ~= "" then
				local cleared = state.scoped_cleared
					and state.scoped_cleared[scope_name]
					and state.scoped_cleared[scope_name][model.ref]
				local s_mem = not cleared
					and state.scoped_remembered
					and state.scoped_remembered[scope_name]
					and state.scoped_remembered[scope_name][model.ref]
				offer("user", 1, s_mem and s_mem[name], "scope:" .. scope_name .. ":state.json")
				local s_sess = not cleared
					and state.scoped_params
					and state.scoped_params[scope_name]
					and state.scoped_params[scope_name][model.ref]
				offer("session", 10, s_sess and s_sess[name], "scope:" .. scope_name)
			end
			offer("agent", 0, agent and agent[name], "agent")
			-- --effort and MARE_EFFORT are invocation overrides, above per-model selections.
			-- Old global init.lua/state.json effort values are deliberately ignored.
			if name == "effort" then
				-- the host installs config_sources after the plugins start; a read during apply
				-- (the statusline's effort) sees no invocation override yet
				local sources = mare.config_sources and mare.config_sources() or {}
				local source = sources.effort and sources.effort.layer
				if source == "cli" or (type(source) == "string" and source:find("^env ")) then
					offer("session", 100, mare.config.effort, source)
				end
			end
			offer("request", 0, opts.request and opts.request[name], "request")
			if best ~= nil then
				R.wanted[name] = best
				local v = best
				if d.type == "enum" and d.aliases[v] then
					v = d.aliases[v]
				elseif not offers(d, v) then
					local policy = d.on_unsupported or "clamp"
					if policy == "error" then
						R.error = ("not sent: %s %s is not offered by %s (route %s; options: %s)"):format(
							name,
							tostring(v),
							tostring(model.ref),
							tostring(route),
							table.concat(d.list, " ")
						)
						v = nil
					elseif policy == "as_is" and passes_value(d, route) then
						R.notes[#R.notes + 1] = { param = name, from = best, to = best, kind = "as_is" }
					else
						v = P.clamp(d, v)
						R.notes[#R.notes + 1] = {
							param = name,
							from = best,
							to = v,
							kind = policy == "as_is" and "as_is_clamped" or "clamp",
						}
					end
				end
				R.params[name] = v
				R.sources[name] = bsrc
			end
			::continue::
		end
		return R
	end

	D.lib.lazy_group(
		P,
		{ "output_cap", "cap_output", "mapping", "apply", "overlay_operations", "merge_overlay" },
		function()
			require("llm.params_wire")(P, D)
		end
	)

	-- ---- rules from plugins -----------------------------------------------------------------

	--- define(caller, rule, layer?) — a plugin's rule (provider layer by default), removed when
	--- the plugin is disposed
	function P.define(caller, rule, layer)
		local r, err = normalize(rule, layer or "provider", rule.where or require("mare_lib").caller_name(caller))
		if not r then
			error("params: " .. err, 2)
		end
		plugin_rules[#plugin_rules + 1] = r
		version = version + 1
		local disposed = false
		local function dispose()
			if disposed then
				return
			end
			disposed = true
			for i = #plugin_rules, 1, -1 do
				if plugin_rules[i] == r then
					table.remove(plugin_rules, i)
				end
			end
			version = version + 1
		end
		-- the rule goes with whoever defined it (the caller's fiber)
		if caller and caller.effect then
			caller:effect(function()
				return dispose
			end, "llm params rule")
		end
		return r, dispose
	end

	--- whether `v` is an option of parameter `name` that some rule defines (a session value may
	--- name an option only some models offer: each request clamps it to its model)
	function P.known_option(name, v)
		local function has(r)
			local d = r.define and r.define[name]
			for _, o in ipairs(d and d.options or {}) do
				if o.value == v then
					return true
				end
			end
			return false
		end
		if has(BUILTIN) then
			return true
		end
		for _, r in ipairs(plugin_rules) do
			if has(r) then
				return true
			end
		end
		for _, r in ipairs(from_config()) do
			if has(r) then
				return true
			end
		end
		return false
	end

	function P.problems()
		local out = {}
		local known = known_routes()
		for _, r in ipairs(from_config()) do
			local bad = unknown_route(r, known)
			if bad then
				out[#out + 1] = ("mare.params%s: %s"):format(r.where and (" (" .. r.where .. ")") or "", bad)
			end
		end
		for _, p in pairs(problems) do
			out[#out + 1] = p
		end
		table.sort(out)
		return out
	end

	--- invalidates the cached definitions (a model table changed in place)
	function P.touch()
		version = version + 1
	end

	-- ---- notes (never a silent change: /profile and the llm/params-adjusted event) -----------

	local log, last = {}, {}
	function P.note(model, n)
		local key = tostring(model.ref) .. "\0" .. n.param
		local sig = tostring(n.from) .. "\0" .. tostring(n.to) .. "\0" .. n.kind
		if last[key] == sig then
			return
		end
		last[key] = sig
		local rec =
			{ at_ms = mare.task.now(), model = model.ref, param = n.param, from = n.from, to = n.to, kind = n.kind }
		log[#log + 1] = rec
		if #log > 50 then
			table.remove(log, 1)
		end
		ctx.bus:emit("llm/params-adjusted", rec)
	end

	--- Resolve the request's effort once, preserving the existing adjustment events.
	function P.resolve_request(req, route, prepared)
		local model = req.model
		if prepared then
			req.effort = prepared.effort
		elseif req.effort == nil then
			local selected = P.select(model, { only = { effort = true }, route = route, request = req.params })
			for _, note in ipairs(selected.notes) do
				P.note(model, note)
			end
			if selected.error then
				return nil, selected.error
			end
			req.effort = selected.params.effort
		end
		local request_params = {}
		for key, value in pairs(req.params or {}) do
			request_params[key] = value
		end
		request_params.effort = req.effort
		req.params = request_params
		return true
	end
	function P.log()
		return { table.unpack(log) }
	end

	return P
end
