-- Session-local subagent types. Definitions have one owner-aware stack per name.
return function(ctx, lib)
	local stacks = {}
	local base, pending = {}, {}
	local last_scope, activated = pending, false
	local session = lib.follow(ctx, "session")
	local allowed = {
		name = true,
		description = true,
		when_to_use = true,
		system = true,
		tools = true,
		disallowed_tools = true,
		model = true,
		effort = true,
	}

	local function invalid(message)
		lib.fail("invalid_arguments", message)
	end

	local function validate_fields(def, frontmatter)
		if type(def) ~= "table" then
			invalid("subagent type must be an object")
		end
		for key in pairs(def) do
			if not allowed[key] or (frontmatter and key == "system") then
				invalid("unknown subagent type field: " .. tostring(key))
			end
		end
	end

	local function validate(def)
		validate_fields(def, false)
		if type(def.name) ~= "string" or not def.name:match("^[A-Za-z0-9_.%-]+$") then
			invalid("subagent type name must contain only ASCII letters, digits, underscore, hyphen or dot")
		end
		for _, key in ipairs({ "description", "when_to_use", "model", "effort" }) do
			local value = def[key]
			if (key == "description" or value ~= nil) and (type(value) ~= "string" or value == "") then
				invalid("subagent type " .. key .. " must be a nonempty string")
			end
		end
		if def.system ~= nil and type(def.system) ~= "string" then
			invalid("subagent type system must be a string")
		end
		for _, key in ipairs({ "tools", "disallowed_tools" }) do
			local list = def[key]
			if list ~= nil then
				if type(list) ~= "table" then
					invalid("subagent type " .. key .. " must be an array")
				end
				local seen, count = {}, 0
				for i, value in pairs(list) do
					if math.type(i) ~= "integer" or i < 1 or type(value) ~= "string" or value == "" or seen[value] then
						invalid("subagent type " .. key .. " must be a dense array of distinct nonempty strings")
					end
					seen[value], count = true, count + 1
				end
				if count ~= #list then
					invalid("subagent type " .. key .. " must be a dense array")
				end
			end
		end
		return lib.copy_deep(def)
	end

	local function changed()
		if ctx.tools and ctx.tools.touch then
			ctx.tools:touch()
		end
	end

	local function actual_scope()
		local service = session()
		local info = service and service:info() or nil
		return info
				and info.session_id
				and info.session_epoch
				and (info.session_id .. "\0" .. tostring(info.session_epoch))
			or pending
	end

	local function scope()
		if not activated then
			return pending
		end
		local current = actual_scope()
		if current ~= last_scope then
			if last_scope ~= pending then
				changed()
			end
			last_scope = current
		end
		return current
	end

	local function start_scope()
		if activated then
			return scope()
		end
		local current = actual_scope()
		if current == pending then
			return pending
		end
		-- Startup may select --session-id or resume after plugins register types.
		for _, stack in pairs(stacks) do
			for _, entry in ipairs(stack) do
				if entry.scope == pending then
					entry.scope = current
				end
			end
		end
		activated, last_scope = true, current
		return current
	end

	local function visible(stack, current)
		for i = #stack, 1, -1 do
			if stack[i].scope == base or stack[i].scope == current then
				return stack[i]
			end
		end
	end

	local function same(a, b)
		if type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return a == b
		end
		for key, value in pairs(a) do
			if not same(value, b[key]) then
				return false
			end
		end
		for key in pairs(b) do
			if a[key] == nil then
				return false
			end
		end
		return true
	end

	local function visible_definitions(current)
		local result = {}
		for name, stack in pairs(stacks) do
			local entry = visible(stack, current)
			if entry then
				result[name] = entry.definition
			end
		end
		return result
	end

	local function replace_discovered(definitions, notify)
		local current = scope()
		local before = visible_definitions(current)
		-- Scanning and validation have finished before the stacks change. Mutate each
		-- stack in place so existing owner disposers retain their exact registrations.
		for name, stack in pairs(stacks) do
			for i = #stack, 1, -1 do
				if stack[i].discovered then
					table.remove(stack, i)
				end
			end
			if #stack == 0 and definitions[name] == nil then
				stacks[name] = nil
			end
		end
		for name, def in pairs(definitions) do
			local stack = stacks[name]
			if not stack then
				stack = {}
				stacks[name] = stack
			end
			local i = 1
			while stack[i] and stack[i].builtin do
				i = i + 1
			end
			table.insert(stack, i, { definition = def, scope = base, discovered = true })
		end
		if notify and not same(before, visible_definitions(current)) then
			changed()
		end
	end

	local reloaded = false
	ctx:on("session/started", start_scope)
	ctx:on("session/changed", function()
		if reloaded then
			start_scope()
		else
			scope()
		end
	end)
	ctx:on("kernel/reloaded", function()
		reloaded = true
		start_scope()
	end)

	local function push(def, replace, notify, owner_scope, meta)
		def = validate(def)
		local stack = stacks[def.name]
		local current = owner_scope == base and base or scope()
		if stack and visible(stack, current) and not replace then
			lib.fail("subagent.duplicate_type", "subagent type already registered: " .. def.name)
		end
		if not stack then
			stack = {}
			stacks[def.name] = stack
		end
		local item = {
			definition = def,
			scope = owner_scope or current,
			builtin = owner_scope == base and not notify,
			read_only = meta and meta.read_only == true or false,
		}
		stack[#stack + 1] = item
		if notify then
			changed()
		end
		return function()
			for i, entry in ipairs(stack) do
				if entry == item then
					local was_visible = visible(stack, scope()) == item
					table.remove(stack, i)
					if #stack == 0 then
						stacks[def.name] = nil
					end
					if was_visible then
						changed()
					end
					return
				end
			end
		end
	end

	local empty = function()
		return mare.json.array()
	end
	for _, spec in ipairs({
		{
			read_only = true,
			def = {
				name = "explore",
				description = "Inspect code and report findings.",
				when_to_use = "Locate files, trace behavior, or investigate without changing files.",
				system = "Inspect the repository without modifying it. Return the relevant evidence and file locations.",
				tools = mare.json.array({ "read", "ls", "bash" }),
				disallowed_tools = empty(),
			},
		},
		{
			def = {
				name = "general",
				description = "Carry out a general delegated task.",
				when_to_use = "Use for implementation or other work subject to the inherited permissions.",
				disallowed_tools = empty(),
			},
		},
		{
			read_only = true,
			def = {
				name = "plan",
				description = "Design an implementation without changing files.",
				when_to_use = "Develop a concrete implementation plan and identify risks.",
				system = "Do not change files. Return an actionable plan grounded in the inspected code.",
				tools = mare.json.array({ "read", "ls", "bash" }),
				disallowed_tools = empty(),
			},
		},
	}) do
		push(spec.def, false, false, base, { read_only = spec.read_only })
	end

	return {
		get = function(name)
			local stack = stacks[name]
			local entry = stack and visible(stack, scope())
			return entry and lib.copy_deep(entry.definition) or nil
		end,
		is_read_only = function(name)
			local stack = stacks[name]
			local entry = stack and visible(stack, scope())
			return entry ~= nil and entry.read_only == true
		end,
		list = function()
			local names, data = {}, mare.json.array()
			local current = scope()
			for name in pairs(stacks) do
				if visible(stacks[name], current) then
					names[#names + 1] = name
				end
			end
			table.sort(names)
			for _, name in ipairs(names) do
				data[#data + 1] = lib.copy_deep(visible(stacks[name], current).definition)
			end
			return { data = data }
		end,
		register = function(caller, def, opts)
			local fiber = type(caller) == "table" and caller.fiber or nil
			local state = fiber and fiber.state
			local fiber_state = require("cordis").FiberState
			-- A plugin's apply owns its defaults across session changes. Registrations
			-- made later, including connection registrations, belong to this session.
			local applying = fiber
				and (state == fiber_state.LOADING or state == fiber_state.PENDING)
				and not lib.rpc_caller_id(caller)
			local remove = push(def, opts and opts.replace == true, true, applying and base or nil)
			return lib.owned(caller, remove, "subagent type " .. def.name)
		end,
		validate = validate,
		validate_frontmatter = function(def)
			return validate_fields(def, true)
		end,
		replace_discovered = replace_discovered,
	}
end
