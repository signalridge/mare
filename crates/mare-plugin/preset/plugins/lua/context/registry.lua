-- context.registry: the system prompt section registry; part of the `context` plugin
-- (plugins/context.lua), loaded at the start of its apply. The core only keeps the registry; every
-- section is a script registered by the plugin that owns it, and withdrawn when that plugin is
-- disposed.
--
-- Layers, farthest first: global (plugins), user (`mare.prompt.section`, scope = "user") and
-- agent (scope = "agent", or `agent = "<name>"` for one named agent). A name registered twice in
-- one layer is an error; a nearer layer's section shadows a farther one of the same name.
-- `complete`: that section is the whole prompt; two active complete sections fail the assembly.
-- `interpolate` (default true): `{{var}}` in the text is replaced strictly — an unknown variable,
-- a malformed `{{…}}` or a variable without a value fails the assembly; a `{{` with no `}}` is
-- plain text, and substituted values are not scanned again. File and third-party text (SYSTEM.md,
-- APPEND_SYSTEM.md, AGENTS.md, MCP) is registered with `interpolate = false`: passed verbatim.
return function(D)
	local invalidate = D.invalidate

	local VAR_NAME = "^[a-z][a-z0-9_]*$"

	local slots = {} -- name -> layer key ("global" | "user" | "agent:" .. agent name) -> entry
	local removed = {} -- name -> count of active removals
	local variables = {} -- name -> {fn, owner}
	local suppressors = {} -- layer key -> { [token] = true }: runtime sections hidden there

	local function owner_name(caller)
		return require("mare_lib").caller_name(caller)
	end

	local function owner_of(caller)
		return owner_name(caller) or "init.lua"
	end

	-- whether `entry` was registered by `caller` (by plugin name; by identity for an unnamed one)
	local function same_caller(entry, caller, caller_name)
		if caller_name ~= nil then
			return entry.owner == caller_name
		end
		return rawequal(entry.caller, caller)
	end

	local function layer_of(def, kind)
		if def.scope == "user" then
			return "user", "user"
		elseif def.scope == "agent" or def.agent ~= nil then
			if def.scope ~= nil and def.scope ~= "agent" then
				error(('%s: scope "%s" cannot be combined with agent'):format(kind, tostring(def.scope)), 3)
			end
			if def.agent ~= nil and type(def.agent) ~= "string" then
				error(("%s: agent must be a string"):format(kind), 3)
			end
			return "agent", "agent:" .. (def.agent or "")
		elseif def.scope == nil or def.scope == "global" then
			return "global", "global"
		end
		error(('%s: unknown scope "%s" (global, user or agent)'):format(kind, tostring(def.scope)), 3)
	end

	-- a section definition as a registry entry (`candidate`: only previewed, never registered)
	local function normalize_section(caller, def, candidate)
		assert(type(def) == "table" and type(def.name) == "string" and def.name ~= "", "section needs a name")
		if def.runtime ~= nil then
			assert(type(def.runtime) == "boolean", "section runtime must be a boolean")
		end
		if def.required ~= nil then
			assert(type(def.required) == "boolean", "section required must be a boolean")
		end
		local name = def.name
		local layer, key = layer_of(def, ('prompt section "%s"'):format(def.name))
		return {
			name = name,
			tag = def.tag or def.name,
			order = def.order or 500,
			text = def.text,
			when = def.when,
			raw = def.raw,
			complete = def.complete,
			interpolate = def.interpolate ~= false,
			runtime = def.runtime == true,
			required = def.required == true,
			layer = layer,
			owner = owner_of(caller),
			caller = caller,
			candidate = candidate == true,
		},
			key
	end

	local function twice(name, layer, prev_owner, owner)
		return ('prompt section "%s" is registered twice in the %s layer (by %s and %s)'):format(
			name,
			layer,
			prev_owner,
			owner
		)
	end

	local R = {}

	--- section(caller, def) -> release: register `def` in its layer until the release is called or
	--- `caller` is disposed (the release is the caller's effect disposer: idempotent)
	function R.section(caller, def)
		local entry, key = normalize_section(caller, def, false)
		local name = entry.name
		local list = slots[name] or {}
		local prev = list[key]
		if prev then
			error(twice(name, entry.layer, prev.owner, entry.owner), 2)
		end
		list[key] = entry
		slots[name] = list
		invalidate()
		return caller:effect(function()
			return function()
				if list[key] == entry then
					list[key] = nil
					if next(list) == nil and slots[name] == list then
						slots[name] = nil
					end
				end
				invalidate()
			end
		end, "context section " .. name)
	end

	--- suppress_runtime(caller, opts?) -> release: hide the non-required runtime sections
	--- (`runtime = true`, not `required = true`) in the matching layer until the release is called
	--- or `caller` is disposed. opts: { scope = "global"|"user"|"agent", agent? }
	function R.suppress_runtime(caller, opts)
		if opts ~= nil then
			assert(type(opts) == "table", "suppress_runtime options must be a table")
			for k in pairs(opts) do
				assert(k == "scope" or k == "agent", "unknown suppress_runtime option: " .. tostring(k))
			end
		end
		local _, key = layer_of(opts or {}, "suppress_runtime")
		local bucket = suppressors[key]
		if not bucket then
			bucket = {}
			suppressors[key] = bucket
		end
		local token = {}
		bucket[token] = true
		invalidate()
		return caller:effect(function()
			return function()
				if bucket[token] then
					bucket[token] = nil
					if next(bucket) == nil and suppressors[key] == bucket then
						suppressors[key] = nil
					end
				end
				invalidate()
			end
		end, "context runtime suppression")
	end

	--- suppress(caller, name): hide a section in every layer until the release is called or `caller`
	--- is disposed — sections registered later under that name stay hidden too
	function R.suppress(caller, name)
		removed[name] = (removed[name] or 0) + 1
		invalidate()
		local released = false
		return caller:effect(function()
			return function()
				if released then
					return
				end
				released = true
				removed[name] = (removed[name] or 1) - 1
				if removed[name] <= 0 then
					removed[name] = nil
				end
				invalidate()
			end
		end, "context registration")
	end

	--- variable(caller, name, fn(info) -> value?): a `{{name}}` for interpolated sections
	function R.variable(caller, name, fn)
		if type(name) ~= "string" or not name:match(VAR_NAME) then
			error(("prompt variable name %q must match %s"):format(tostring(name), VAR_NAME), 2)
		end
		local prev = variables[name]
		if prev then
			error(
				('prompt variable "%s" is registered twice (by %s and %s)'):format(name, prev.owner, owner_of(caller)),
				2
			)
		end
		local var = { fn = fn, owner = owner_of(caller) }
		variables[name] = var
		invalidate()
		return caller:effect(function()
			return function()
				if variables[name] == var then
					variables[name] = nil
				end
				invalidate()
			end
		end, "context registration")
	end

	local function interpolate(text, section, info)
		local out, i = {}, 1
		while true do
			local s = text:find("{{", i, true)
			local e = s and text:find("}}", s + 2, true)
			if not e then
				break -- no (further) `{{ … }}`: the rest is plain text
			end
			local inner = text:sub(s + 2, e - 1)
			if not inner:match(VAR_NAME) then
				error(('malformed prompt variable {{%s}} in section "%s"'):format(inner, section), 0)
			end
			local var = variables[inner]
			if not var then
				error(('unknown prompt variable {{%s}} in section "%s"'):format(inner, section), 0)
			end
			local v = var.fn
			if type(v) == "function" then
				v = v(info)
			end
			if v == nil then
				error(('prompt variable {{%s}} has no value (section "%s")'):format(inner, section), 0)
			end
			out[#out + 1] = text:sub(i, s - 1)
			out[#out + 1] = tostring(v)
			i = e + 2
		end
		out[#out + 1] = text:sub(i)
		return table.concat(out)
	end

	-- the entry that applies to `agent`: the named agent's, then this VM's agent layer, then the
	-- user layer, then the global one
	local function pick(list, agent)
		return (agent ~= nil and list["agent:" .. agent]) or list["agent:"] or list.user or list.global
	end

	local function render(e, model, agent)
		local text = e.text
		local info = { agent = agent, model = model, scope = e.layer }
		if type(text) == "function" then
			text = text(model, info)
		end
		if text == nil or text == "" then
			return nil
		end
		if e.interpolate and type(text) == "string" then
			text = interpolate(text, e.name, info)
		end
		return { name = e.name, tag = e.tag, order = e.order, text = text, raw = e.raw, entry = e }
	end

	local function wrap(tag, body)
		return ("<%s>\n%s\n</%s>"):format(tag, body, tag)
	end

	local function has_any(t)
		return t ~= nil and next(t) ~= nil
	end

	local function runtime_suppressed(agent, extra)
		if extra then
			return true
		end
		if has_any(suppressors.global) or has_any(suppressors.user) or has_any(suppressors["agent:"]) then
			return true
		end
		return agent ~= nil and has_any(suppressors["agent:" .. agent])
	end

	-- the registry as a preview sees it: without the caller's own sections (`replace`), with the
	-- candidate `sections` added; the live registry itself when neither is asked for
	local function effective_slots(opts)
		if not opts or (not opts.replace and not opts.sections) then
			return slots
		end
		local caller = opts.caller
		local cname = owner_name(caller)
		local effective = {}
		for name, list in pairs(slots) do
			local copy = {}
			for key, entry in pairs(list) do
				if not (opts.replace and same_caller(entry, caller, cname)) then
					copy[key] = entry
				end
			end
			if next(copy) ~= nil then
				effective[name] = copy
			end
		end
		if opts.sections then
			assert(type(opts.sections) == "table", "preview sections must be a list")
			for _, def in ipairs(opts.sections) do
				local entry, key = normalize_section(caller, def, true)
				local list = effective[entry.name] or {}
				local prev = list[key]
				if prev then
					error(twice(entry.name, entry.layer, prev.owner, entry.owner), 3)
				end
				list[key] = entry
				effective[entry.name] = list
			end
		end
		return effective
	end

	local function assemble_internal(model, agent, opts)
		local effective = effective_slots(opts)
		local suppressed = runtime_suppressed(agent, opts and opts.suppress_runtime)
		local names = {}
		for name in pairs(effective) do
			names[#names + 1] = name
		end
		table.sort(names)
		local active, completes = {}, {}
		for _, name in ipairs(names) do
			local e = not removed[name] and pick(effective[name], agent)
			if e and not (suppressed and e.runtime and not e.required) and (not e.when or e.when(model)) then
				if e.complete then
					completes[#completes + 1] = e
				else
					active[#active + 1] = e
				end
			end
		end
		local result = { suppressed = suppressed, effective = effective, names = names, chosen = {} }
		-- a complete section is the whole prompt (the others aren't built at all)
		local whole
		for _, e in ipairs(completes) do
			local c = render(e, model, agent)
			if c then
				if whole then
					error(('two complete prompt sections are active: "%s" and "%s"'):format(whole.name, c.name), 0)
				end
				whole = c
			end
		end
		if whole then
			result.text, result.complete = whole.text, whole
			return result
		end
		local chosen = {}
		for _, e in ipairs(active) do
			chosen[#chosen + 1] = render(e, model, agent)
		end
		table.sort(chosen, function(x, y)
			if x.order ~= y.order then
				return x.order < y.order
			end
			if x.tag ~= y.tag then
				return x.tag < y.tag
			end
			return x.name < y.name
		end)
		local parts = {}
		for _, c in ipairs(chosen) do
			c.fragment = c.raw and c.text or wrap(c.tag, c.text)
			parts[#parts + 1] = c.fragment
		end
		result.text, result.chosen = table.concat(parts, "\n\n"), chosen
		return result
	end

	--- assemble(model, agent) -> text, complete?: the sections in (order, tag) order, separated by
	--- blank lines (or the active complete section's text); raises when a section can't be built
	function R.assemble(model, agent)
		local res = assemble_internal(model, agent)
		return res.text, res.complete
	end

	-- a section's place in a preview: active (and whether its text survived the post-hook),
	-- shadowed by a nearer layer, removed, suppressed, inactive or preempted by a complete section
	local function diagnose(res, agent, final_text)
		local rendered = {}
		for _, c in ipairs(res.chosen) do
			rendered[c.entry] = c
		end
		local out = {}
		for _, name in ipairs(res.names) do
			local list = res.effective[name]
			local winner = pick(list, agent)
			local keys = {}
			for key in pairs(list) do
				keys[#keys + 1] = key
			end
			table.sort(keys)
			for _, key in ipairs(keys) do
				local entry = list[key]
				local status, active, preserved, shadowed_by = nil, false, false, nil
				if removed[name] then
					status = "removed"
				elseif res.suppressed and entry.runtime and not entry.required then
					status = "suppressed"
				elseif entry ~= winner then
					if winner then
						status = "shadowed"
						shadowed_by = { name = winner.name, layer = winner.layer, owner = winner.owner }
					else
						status = "inactive"
					end
				elseif res.complete then
					if entry == res.complete.entry then
						status, active = "active", true
						preserved = final_text == res.complete.text
					else
						status = "preempted_by_complete"
					end
				else
					local c = rendered[entry]
					if c then
						status, active = "active", true
						preserved = final_text:find(c.fragment, 1, true) ~= nil
					else
						status = "inactive"
					end
				end
				out[#out + 1] = {
					name = entry.name,
					tag = entry.tag,
					order = entry.order,
					layer = entry.layer,
					owner = entry.owner,
					complete = entry.complete == true,
					raw = entry.raw == true,
					runtime = entry.runtime,
					required = entry.required,
					candidate = entry.candidate,
					active = active,
					preserved = preserved,
					status = status,
					shadowed_by = shadowed_by,
				}
			end
		end
		return out
	end

	--- preview(caller, model, agent, opts?, transform?) -> { text, complete?, runtime_suppressed,
	--- sections }: the prompt the live registry would build with the candidate `opts.sections`
	--- (and without the caller's own when `opts.replace`, without runtime sections when
	--- `opts.suppress_runtime`), through `transform` (the context/system post-hook); never changes
	--- the registry. A candidate that can't be built (an unknown {{var}}, two complete sections)
	--- raises here, before anything is registered.
	function R.preview(caller, model, agent, opts, transform)
		if opts ~= nil then
			assert(type(opts) == "table", "preview options must be a table")
			for key in pairs(opts) do
				if key ~= "sections" and key ~= "replace" and key ~= "suppress_runtime" then
					error("unknown system prompt preview option: " .. tostring(key), 2)
				end
			end
			if opts.replace ~= nil then
				assert(type(opts.replace) == "boolean", "preview replace must be a boolean")
			end
			if opts.suppress_runtime ~= nil then
				assert(type(opts.suppress_runtime) == "boolean", "preview suppress_runtime must be a boolean")
			end
		end
		local res = assemble_internal(model, agent, {
			caller = caller,
			sections = opts and opts.sections,
			replace = opts and opts.replace,
			suppress_runtime = opts and opts.suppress_runtime,
		})
		local final_text = transform and transform(res.text) or res.text
		if res.complete then
			final_text = res.complete.text
		end
		return {
			text = final_text,
			complete = res.complete and res.complete.entry.name or nil,
			runtime_suppressed = res.suppressed,
			sections = diagnose(res, agent, final_text),
		}
	end

	return R
end
