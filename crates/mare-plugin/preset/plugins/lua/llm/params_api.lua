-- llm.params_api: the model-parameters engine's service methods (define, select, apply, set,
-- cycle, explain) and its persistence (state.json `model_params`); part of the `llm` plugin
-- (plugins/llm.lua). The engine itself is llm/params.
return function(D)
	local ctx, state, llm = D.ctx, D.state, D.llm
	local P = D.params

	--- define_params(rule, layer?) — a plugin's parameters and wire mappings (provider layer by
	--- default; "user" for a provider's own config), dropped when the plugin is disposed
	if not D.lazy_params_api then
		function llm.define_params(_, caller, rule, layer)
			return P.define(caller, rule, layer)
		end
	end

	--- param_list(model?) -> the model's parameter definitions, in definition order:
	--- {id, name, type, list (offered values), options, default, cycleable, statusline, visible, tooltip}
	function llm.param_list(self, caller, model)
		model = model or self:current(caller)
		return model and P.list(model) or {}
	end

	--- resolve_params(model?, opts?) -> {params, sources, wanted, notes, error, route}: every
	--- parameter's value by layer (built-in < provider < model < user < project < agent <
	--- session < request) after the unsupported-option policy. opts: request = {param = value}
	function llm.resolve_params(self, caller, model, opts)
		model = model or self:current(caller)
		if not model then
			return { params = {}, sources = {}, wanted = {}, notes = {} }
		end
		return P.select(model, opts)
	end

	--- apply_params(req, wire, opts?) — use the request's resolved wire parameter route.
	function llm.apply_params(_, _, req, wire, opts)
		local route = req.routes and req.routes.wire
		if not route then
			local _, _, resolved, err = D.routes.route_of(req.model)
			if err then
				error(err, 0)
			end
			route = resolved
		end
		if opts and opts.prepare then
			local base = wire.body.max_tokens
			local copy = table.copy(req)
			local ok, err = P.resolve_request(copy, route)
			if not ok then
				error({ message = err, kind = "invalid" }, 0)
			end
			copy.params = mare.projection.project(copy.params)
			local applied = P.apply(copy, wire, route, opts)
			local operations, seed_written = P.overlay_operations(applied, wire.body)
			if wire.body.max_tokens == base and not seed_written then
				wire.body.max_tokens = nil
			end
			return operations, copy.effort, route, P.output_cap(wire.body, base, operations)
		end
		return P.apply(req, wire, route, opts)
	end

	--- remember(ref, name, value): state.json `model_params[ref][name]`; never from a child
	local function remember(ref, name, value)
		if mare.mode == "child" then
			return
		end
		local ok, err = pcall(mare.state.update, function(st)
			st.model_params = type(st.model_params) == "table" and st.model_params or {}
			st.model_params[ref] = type(st.model_params[ref]) == "table" and st.model_params[ref] or {}
			st.model_params[ref][name] = value
			return st
		end)
		if not ok then
			mare.log("warn", "state.json: " .. tostring(err))
		end
		state.remembered = type(state.remembered) == "table" and state.remembered or {}
		state.remembered[ref] = state.remembered[ref] or {}
		state.remembered[ref][name] = value
	end
	--- remember_param(ref, name, value) — state.json `model_params` (the /effort command's memory)
	function llm.remember_param(_, _, ref, name, value)
		remember(ref, name, value)
	end

	--- remember_scoped_param(scope, ref, name, value) — state.json `scoped_model_params[scope][ref][name]`
	--- (shared by the llm modules; not part of the service)
	function D.remember_scoped_param(scope, ref, name, value)
		if type(scope) ~= "string" or scope == "" or type(ref) ~= "string" or ref == "" then
			return
		end
		if mare.mode ~= "child" then
			local ok, err = pcall(mare.state.update, function(st)
				st.scoped_model_params = type(st.scoped_model_params) == "table" and st.scoped_model_params or {}
				if value == nil then
					if
						type(st.scoped_model_params[scope]) == "table"
						and type(st.scoped_model_params[scope][ref]) == "table"
					then
						st.scoped_model_params[scope][ref][name] = nil
						if next(st.scoped_model_params[scope][ref]) == nil then
							st.scoped_model_params[scope][ref] = nil
						end
						if next(st.scoped_model_params[scope]) == nil then
							st.scoped_model_params[scope] = nil
						end
					end
				else
					st.scoped_model_params[scope] = type(st.scoped_model_params[scope]) == "table"
							and st.scoped_model_params[scope]
						or {}
					st.scoped_model_params[scope][ref] = type(st.scoped_model_params[scope][ref]) == "table"
							and st.scoped_model_params[scope][ref]
						or {}
					st.scoped_model_params[scope][ref][name] = value
				end
				return st
			end)
			if not ok then
				mare.log("warn", "state.json: " .. tostring(err))
			end
		end
		state.scoped_remembered = type(state.scoped_remembered) == "table" and state.scoped_remembered or {}
		if value == nil then
			if state.scoped_remembered[scope] and state.scoped_remembered[scope][ref] then
				state.scoped_remembered[scope][ref][name] = nil
			end
		else
			state.scoped_remembered[scope] = state.scoped_remembered[scope] or {}
			state.scoped_remembered[scope][ref] = state.scoped_remembered[scope][ref] or {}
			state.scoped_remembered[scope][ref][name] = value
		end
	end

	local function names_of(list)
		local out = {}
		for _, d in ipairs(list) do
			out[#out + 1] = d.id
		end
		return table.concat(out, ", ")
	end

	local function coerce(d, v)
		if d.type == "bool" then
			if v == "on" or v == "true" or v == "yes" then
				return true
			elseif v == "off" or v == "false" or v == "no" then
				return false
			end
		end
		return v
	end

	--- set_param(name, value, opts?) -> the value in effect | nil, err. The current model's
	--- parameter for this session; remembered for the model unless opts.save == false. An
	--- option the model doesn't offer is adjusted to the nearest one (the result says so).
	function llm.set_param(self, caller, name, value, opts)
		opts = opts or {}
		local m = self:current(caller)
		if not m then
			return nil, "no model selected"
		end
		local defs = P.defs(m)
		local d = defs[name]
		if not d then
			local list = P.list(m)
			return nil,
				("%s has no %s parameter%s"):format(
					m.ref,
					name,
					#list > 0 and ("; its parameters: " .. names_of(list)) or ""
				)
		end
		value = coerce(d, value)
		local used = value
		if not P.offers(d, value) then
			used = P.clamp(d, value)
			if used == nil then
				return nil, ("%s: %s is not one of %s"):format(name, tostring(value), table.concat(d.list, " "))
			end
		end
		state.params[m.ref] = state.params[m.ref] or {}
		state.params[m.ref][name] = used
		if opts.save ~= false then
			remember(m.ref, name, used)
		end
		ctx.bus:emit("llm/params-changed", { model = m, param = name, value = used, requested = value })
		if name == "effort" then
			ctx.bus:emit("llm/model-changed", {
				model = m,
				effort = self:effort(caller),
				global_effort = self.global_effort and self:global_effort(caller, m) or nil,
				scope = state.active_scope,
				scoped_only = false,
				-- the same model with a new effort: recorded (resume restores it), not a switch
				params_only = true,
			})
		end
		return used
	end

	--- cycle_param(name, dir) -> the new value | nil, err: the next offered option of a
	--- `cycleable` parameter (dir +1 / -1, wrapping)
	function llm.cycle_param(self, caller, name, dir)
		local m = self:current(caller)
		local d = m and P.defs(m)[name]
		if not d then
			return nil, ("%s has no %s parameter"):format(m and m.ref or "this model", name)
		end
		if not d.cycleable then
			return nil, name .. " is not cycleable"
		end
		local cur = P.select(m).params[name]
		local at = 1
		for i, v in ipairs(d.list) do
			if v == cur then
				at = i
			end
		end
		local nxt = d.list[((at - 1 + (dir or 1)) % #d.list) + 1]
		return self:set_param(caller, name, nxt)
	end

	-- ---- explain: the effective parameters, the exact wire fields and where each came from ----

	local function encode(v)
		local ok, s = pcall(mare.json.encode, v)
		return ok and s or tostring(v)
	end

	-- the leaf fields of `t` as "a.b=json", sorted
	local function flatten(t, prefix, out)
		for k, v in pairs(t) do
			local path = prefix and (prefix .. "." .. tostring(k)) or tostring(k)
			if type(v) == "table" and next(v) ~= nil and #v == 0 then
				flatten(v, path, out)
			else
				out[path] = encode(v)
			end
		end
		return out
	end

	--- what applying one parameter writes on a scratch request (a function mapping is run too)
	local function sends(model, route, name, value)
		local base = { max_tokens = model.max_output }
		local wire = { body = { max_tokens = model.max_output }, headers = {} }
		local req = { model = model, effort = name == "effort" and value or nil, params = { [name] = value } }
		local ok, applied = pcall(P.apply, req, wire, route, { only = { [name] = true } })
		if not ok then
			return { "(mapping failed: " .. tostring(applied) .. ")" }, nil
		end
		local before, after = flatten(base, nil, {}), flatten(wire.body, nil, {})
		local out = {}
		for k, v in pairs(after) do
			if before[k] ~= v then
				out[#out + 1] = k .. "=" .. v
			end
		end
		for k in pairs(before) do
			if after[k] == nil then
				out[#out + 1] = k .. " removed"
			end
		end
		table.sort(out)
		local a = applied and applied[1]
		return out, a and { where = a.where, fn = a.fn }
	end

	--- explain_params(ref?) -> {model, route, route_source?, rows, notes, problems}; route_source
	--- only for a provider with several routes; a row per parameter:
	--- {param, name, value, wanted, source = {layer, where}, sends = {"path=json"}, by = {where, fn},
	--- options = {values}, cost = {value = true}, default, cycleable, statusline}
	function llm.explain_params(self, caller, ref)
		local m = type(ref) == "table" and ref or (ref and self:resolve(caller, ref)) or self:current(caller)
		if not m then
			return nil, "no model"
		end
		local R = P.select(m)
		local rows = {}
		for _, d in ipairs(P.list(m, R.route)) do
			local v = R.params[d.id]
			local cost = {}
			for _, o in ipairs(d.options) do
				if o.cost then
					cost[o.value] = true
				end
			end
			local s, by = {}, nil
			if v ~= nil and R.route then
				s, by = sends(m, R.route, d.id, v)
			end
			rows[#rows + 1] = {
				param = d.id,
				name = d.name or d.id,
				type = d.type,
				value = v,
				wanted = R.wanted[d.id],
				source = R.sources[d.id],
				default = d.default,
				sends = s,
				by = by,
				options = d.list,
				cost = cost,
				cycleable = d.cycleable,
				statusline = d.statusline,
				visible = d.visible,
				tooltip = d.tooltip,
			}
		end
		-- a provider with several routes: which layer chose this model's (llm/routes)
		local p = D.providers[m.provider]
		local gateway, source, wire_route, route_error = D.routes.route_of(m)
		local route_source = p and type(p.routes) == "table" and source or nil
		return {
			model = m.ref,
			route = gateway,
			wire_route = wire_route,
			route_source = route_source,
			route_error = route_error,
			rows = rows,
			notes = R.notes,
			error = R.error,
			problems = P.problems(),
		}
	end

	--- explain_lines(ref?) -> plain text lines (/model preview, /doctor, `mare doctor`)
	function llm.explain_lines(self, caller, ref)
		local e = self:explain_params(caller, ref)
		if not e then
			return {}
		end
		local out = {}
		if e.route_source then
			local wire = e.wire_route and e.wire_route ~= e.route and (" → " .. e.wire_route) or ""
			out[#out + 1] = ("route %s%s ← %s"):format(tostring(e.route), wire, e.route_source)
			if e.route_error then
				out[#out + 1] = e.route_error
			end
		end
		for _, r in ipairs(e.rows) do
			local val = tostring(r.value)
			if r.wanted ~= nil and r.wanted ~= r.value then
				val = tostring(r.wanted) .. "→" .. val
			end
			local src = r.source and (r.source.layer .. (r.source.where and (" " .. r.source.where) or "")) or "—"
			out[#out + 1] = ("%s %s ← %s"):format(r.param, val, src)
			if r.by and r.by.fn then
				out[#out + 1] = ("  sends (by a function, %s) %s"):format(
					r.by.where or "?",
					#r.sends > 0 and table.concat(r.sends, " ") or "nothing"
				)
			elseif #r.sends > 0 then
				out[#out + 1] = ("  sends %s%s"):format(
					table.concat(r.sends, " "),
					r.by and r.by.where and (" ← " .. r.by.where) or ""
				)
			end
			local opts = {}
			for _, o in ipairs(r.options or {}) do
				opts[#opts + 1] = tostring(o) .. (r.cost[o] and "*" or "")
			end
			out[#out + 1] = ("  options %s"):format(table.concat(opts, " "))
		end
		for _, p in ipairs(e.problems) do
			out[#out + 1] = "! " .. p
		end
		return out
	end

	--- note_param(model, {param, from, to, kind}) — records an adjustment (a clamp on a model
	--- switch …) for /profile and `llm/params-adjusted`; a repeat of the last one is dropped
	function llm.note_param(_, _, model, n)
		P.note(model, n)
	end

	--- params_log() -> the recent adjustments (clamped / dropped / passed as is), oldest first
	function llm.params_log(_, _)
		return P.log()
	end

	--- subagent_params(model_ref?) -> {param = value} | nil: what a sub-agent on `model_ref` starts
	--- with in its agent layer (mare-host start asks; `start{params=}` goes on top) — this
	--- session's resolved values when it is the current model, nothing for another model (that one
	--- uses its remembered values or defaults:117`)
	function llm.subagent_params(self, caller, ref)
		local cur = self:current(caller)
		local m = type(ref) == "string" and self:resolve(caller, ref) or nil
		if not (cur and m and m.ref == cur.ref) then
			return nil
		end
		local out = {}
		for k, v in pairs(P.select(cur).params) do
			out[k] = v
		end
		return next(out) and out or nil
	end

	--- the rule functions a sub-agent's VM would not get (mare.subagent.uncarried: one capturing a
	--- userdata, a thread or a built-in the globals don't reach), each with its rule
	local function uncarried()
		local check = mare.subagent and mare.subagent.uncarried
		local out = {}
		if not check then
			return out
		end
		for i, r in ipairs(type(mare.config.param_rules) == "table" and mare.config.param_rules or {}) do
			for _, p in ipairs(check(r)) do
				out[#out + 1] = ("mare.params (%s): %s"):format(type(r) == "table" and r._where or ("#" .. i), p)
			end
		end
		for ref, m in pairs(type(mare.config.models) == "table" and mare.config.models or {}) do
			if type(m) == "table" and type(m.params) == "table" then
				for _, p in ipairs(check(m.params)) do
					out[#out + 1] = ("mare.model %s: %s"):format(tostring(ref), p)
				end
			end
		end
		table.sort(out)
		return out
	end

	-- `mare doctor` / `/doctor`: the current model's effective parameters and rule problems
	local function on_doctor(checks)
		local m = llm.current(llm, ctx)
		if not m then
			return
		end
		local e = llm.explain_params(llm, ctx, m)
		local parts = {}
		for _, r in ipairs(e and e.rows or {}) do
			local src = r.source and r.source.layer or "—"
			parts[#parts + 1] = ("%s %s (%s)%s"):format(
				r.param,
				tostring(r.value),
				src,
				#r.sends > 0 and (" → " .. table.concat(r.sends, " ")) or ""
			)
		end
		if e and e.route_source then
			table.insert(parts, 1, ("route %s (%s)"):format(tostring(e.route), e.route_source))
		end
		local probs = P.problems()
		local lost = uncarried()
		checks[#checks + 1] = {
			name = "model params",
			ok = #probs == 0,
			warn = #probs > 0 or #lost > 0,
			detail = (#parts > 0 and table.concat(parts, "; ") or "none")
				.. (#probs > 0 and (" · " .. table.concat(probs, "; ")) or "")
				.. (#lost > 0 and (" · sub-agents run without: " .. table.concat(lost, "; ")) or ""),
		}
	end
	if D.lazy_params_api then
		return { on_doctor = on_doctor }
	end
	ctx:on("kernel/doctor", on_doctor)
end
