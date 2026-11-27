-- llm.params_wire: the model-parameters engine's field writing — how a parameter's value
-- becomes request fields on a route (field tables: dot paths, "$value", false, {at_least},
-- {value}; or function(req, ctx)). Adds P.mapping and P.apply to the engine (llm/params);
-- part of the `llm` plugin (plugins/llm.lua).
return function(P, D)
	local ctx = D.ctx

	-- ---- wire ---------------------------------------------------------------------------------

	local function copy(v)
		if type(v) ~= "table" then
			return v
		end
		local t = {}
		for k, x in pairs(v) do
			t[k] = copy(x)
		end
		return t
	end

	local function split(path)
		local parts = {}
		for seg in tostring(path):gmatch("[^%.]+") do
			parts[#parts + 1] = seg
		end
		return parts
	end

	local function get_path(t, path)
		for _, seg in ipairs(split(path)) do
			if type(t) ~= "table" then
				return nil
			end
			t = t[seg]
		end
		return t
	end

	local function set_path(t, path, v)
		local parts = split(path)
		for i = 1, #parts - 1 do
			if type(t[parts[i]]) ~= "table" then
				if v == nil then
					return
				end
				t[parts[i]] = {}
			end
			t = t[parts[i]]
		end
		t[parts[#parts]] = v
	end

	local OUTPUT_CAP_FIELDS =
		{ "max_tokens", "max_output_tokens", "max_completion_tokens", "generationConfig.maxOutputTokens" }

	function P.output_cap(body, base, operations)
		local limit
		for _, path in ipairs(OUTPUT_CAP_FIELDS) do
			local value = get_path(body, path)
			if value ~= nil then
				if not D.lib.positive_integer(value) then
					error("invalid mapped output cap " .. path, 0)
				end
				for _, op in ipairs(operations) do
					if op.path == path and op.at_least ~= nil then
						value = math.max(value, base)
					end
				end
				limit = limit and math.max(limit, value) or value
			end
		end
		return limit or base
	end

	function P.cap_output(body, cap)
		for _, path in ipairs(OUTPUT_CAP_FIELDS) do
			local value = get_path(body, path)
			if type(value) == "number" then
				set_path(body, path, math.min(value, cap))
			end
		end
		if
			type(body.max_tokens) == "number"
			and type(body.thinking) == "table"
			and body.thinking.type == "enabled"
			and type(body.thinking.budget_tokens) == "number"
		then
			if body.max_tokens <= 1024 then
				body.thinking = nil
			elseif body.thinking.budget_tokens >= body.max_tokens then
				body.thinking.budget_tokens = math.max(1024, body.max_tokens - 1024)
			end
		end
	end

	--- runs a field table on `body`: "$value" → the value, false → removed, {at_least = n} →
	--- raised to n; returns the writes ({path, value}) for explain
	local function run_fields(map, body, value)
		local keys = {}
		for k in pairs(map) do
			keys[#keys + 1] = k
		end
		table.sort(keys, function(a, b)
			return tostring(a) < tostring(b)
		end)
		local writes = {}
		for _, k in ipairs(keys) do
			local v = map[k]
			if v == "$value" then
				set_path(body, k, value)
				writes[#writes + 1] = { path = k, value = value }
			elseif v == false then
				set_path(body, k, nil)
				writes[#writes + 1] = { path = k, removed = true }
			elseif type(v) == "table" and v.value ~= nil and next(v, next(v)) == nil then
				-- { value = x }: x as it is (false included, which alone would remove the field)
				set_path(body, k, copy(v.value))
				writes[#writes + 1] = { path = k, value = v.value }
			elseif type(v) == "table" and v.at_least ~= nil and next(v, next(v)) == nil then
				local cur = get_path(body, k)
				if type(cur) ~= "number" or cur < v.at_least then
					set_path(body, k, v.at_least)
				end
				writes[#writes + 1] = { path = k, at_least = v.at_least }
			else
				set_path(body, k, copy(v))
				writes[#writes + 1] = { path = k, value = v }
			end
		end
		return writes
	end

	--- the mapping of value `v` of `d` on `route`: the option's own, else the parameter's (a bool
	--- parameter's is keyed by the value); and the rule it came from
	function P.mapping(d, v, route)
		local o = d.index[v]
		if o and o.wire[route] ~= nil then
			return o.wire[route], o.wire_src and o.wire_src[route]
		end
		local m = d.wire[route]
		if d.type == "bool" and type(m) == "table" then
			local b = m[v]
			if b == nil then
				b = m[tostring(v)]
			end
			return b, d.wire_src[route]
		end
		return m, d.wire_src[route]
	end

	local GUARDED = { authorization = true, ["x-api-key"] = true, ["x-goog-api-key"] = true }

	--- apply(req, wire, route, opts) → the applied mappings. Writes the request's parameters into
	--- wire.body / wire.headers. Request parameters are already selected by llm.stream;
	--- absent values are resolved from the model's parameter layers.
	--- opts: only = {param = true}, ctx = extra fields for mapping functions
	function P.apply(req, wire, route, opts)
		opts = opts or {}
		local model = req.model
		local defs = P.defs(model, route)
		local values = {}
		local others
		for name in pairs(defs) do
			if not opts.only or opts.only[name] then
				if req.params and req.params[name] ~= nil then
					values[name] = req.params[name]
				else
					others = others or P.select(model, { route = route })
					values[name] = others.params[name]
				end
			end
		end
		local names = {}
		for name in pairs(values) do
			names[#names + 1] = name
		end
		table.sort(names)
		local applied = {}
		for _, name in ipairs(names) do
			local d, v = defs[name], values[name]
			local m, rule = P.mapping(d, v, route)
			if type(m) == "table" then
				applied[#applied + 1] =
					{ param = name, value = v, writes = run_fields(m, wire.body, v), where = rule and rule.where }
			elseif type(m) == "function" then
				wire.headers = wire.headers or {}
				local original_headers = wire.headers
				local before = {}
				if rule and rule.layer == "project" then
					for k, hv in pairs(wire.headers) do
						if GUARDED[string.lower(k)] then
							before[k] = hv
						end
					end
				end
				local fctx = { model = model, route = route, params = values, param = name, value = v }
				for k, x in pairs(opts.ctx or {}) do
					fctx[k] = x
				end
				local body, tracked, seed_touched = wire.body, nil, false
				if opts.prepare then
					tracked = setmetatable({}, {
						__index = body,
						__newindex = function(_, key, value)
							if key == "max_tokens" then
								seed_touched = true
							end
							body[key] = value
						end,
						__pairs = function()
							return pairs(body)
						end,
					})
					wire.body = tracked
				end
				local ok, err = pcall(m, wire, fctx)
				if tracked then
					if wire.body == tracked then
						wire.body = body
					else
						seed_touched = true
					end
				end
				if not ok then
					error(
						("params: %s mapping (%s) failed: %s"):format(name, rule and rule.where or "?", tostring(err)),
						0
					)
				end
				if rule and rule.layer == "project" then
					if wire.headers ~= original_headers then
						error("params: project mapping replaced the headers table", 0)
					end
					local changed = {}
					for k, value in pairs(before) do
						if wire.headers[k] ~= value then
							changed[k] = true
						end
					end
					for k in pairs(wire.headers) do
						if GUARDED[string.lower(k)] then
							if wire.headers[k] ~= before[k] then
								changed[k] = true
							end
							wire.headers[k] = nil
						end
					end
					for k, value in pairs(before) do
						wire.headers[k] = value
					end
					for k in pairs(changed) do
						mare.log(
							"warn",
							("params: %s refused writing header %s from project scope"):format(rule.where or "?", k)
						)
					end
				end
				applied[#applied + 1] = {
					param = name,
					value = v,
					fn = true,
					where = rule and rule.where,
					seed_touched = seed_touched,
					seed_written = seed_touched and wire.body.max_tokens ~= nil,
				}
			end
		end
		return applied
	end

	--- Retain declarative removals and lower bounds, which a sparse overlay cannot express.
	function P.overlay_operations(applied, body)
		local final = {}
		local seed_written = false
		for _, entry in ipairs(applied) do
			if entry.seed_touched then
				seed_written = entry.seed_written
			end
			for _, write in ipairs(entry.writes or {}) do
				if write.path == "max_tokens" then
					seed_written = not write.removed and write.at_least == nil
				end
				if write.removed or write.at_least ~= nil then
					final[write.path] = write
				else
					final[write.path] = nil
				end
			end
		end
		local paths = {}
		for path, write in pairs(final) do
			-- A later function mapping may have restored a declaratively removed field.
			if not write.removed or get_path(body, path) == nil then
				paths[#paths + 1] = path
			end
		end
		table.sort(paths)
		local out = mare.json.array()
		for _, path in ipairs(paths) do
			local write = final[path]
			out[#out + 1] = { path = path, removed = write.removed, at_least = write.at_least }
		end
		return out, seed_written
	end

	local function merge(into, from)
		for key, value in pairs(from or {}) do
			if type(value) == "table" and not mare.json.is_array(value) then
				if type(into[key]) ~= "table" or mare.json.is_array(into[key]) then
					into[key] = {}
				end
				merge(into[key], value)
			else
				into[key] = value
			end
		end
	end

	function P.merge_overlay(wire, overlay)
		wire.body, wire.headers = wire.body or {}, wire.headers or {}
		local before = {}
		for _, op in ipairs(overlay.operations or {}) do
			if op.at_least ~= nil then
				before[op.path] = get_path(wire.body, op.path)
			end
		end
		merge(wire.body, overlay.body)
		merge(wire.headers, overlay.headers)
		for _, op in ipairs(overlay.operations or {}) do
			if op.removed then
				set_path(wire.body, op.path, nil)
			else
				local previous, current = before[op.path], get_path(wire.body, op.path)
				local value = type(current) == "number" and math.max(current, op.at_least) or op.at_least
				if type(previous) == "number" then
					value = math.max(value, previous)
				end
				if value ~= current then
					set_path(wire.body, op.path, value)
				end
			end
		end
	end
end
