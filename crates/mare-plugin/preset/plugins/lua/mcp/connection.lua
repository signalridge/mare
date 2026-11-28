-- mcp.connection: connect / watch / disconnect, tool (un)registration and the in-turn queue
-- of tool updates;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local servers, owned, codemode, startup_cap = D.servers, D.owned, D.codemode, D.startup_cap
	local agent_notify, pending_cache = D.agent_notify, D.pending_cache
	local blocked = D.config.blocked
	local name_tools, tool_tier, filter_tools = D.exposure.name_tools, D.exposure.tool_tier, D.exposure.filter_tools
	-- results, elicitation and prompts load on first use, not while this module registers
	local function content_of(...)
		return D.results.content_of(...)
	end
	local function elicit(...)
		return D.elicit.elicit(...)
	end
	local function apply_prompts(...)
		return D.prompts.apply_prompts(...)
	end

	-- ---- connecting ---------------------------------------------------------------

	local function spec_of(e)
		local srv = e.srv
		local oauth = nil
		if srv.oauth then
			oauth = type(srv.oauth) == "table" and table.copy(srv.oauth) or {}
			oauth.key = e.name
		end
		local timeout = math.min(srv.startup_timeout_ms or cfg.startup_timeout_ms or 60000, startup_cap)
		-- the same environment filter as bash: the tools plugin's
		-- policy, then the server's own env_include / env_exclude
		local include, exclude = {}, {}
		local pok, pol = pcall(function()
			return ctx.tools:env_policy()
		end)
		for _, src in ipairs({ pok and pol or {}, { env_include = srv.env_include, env_exclude = srv.env_exclude } }) do
			for _, n in ipairs(type(src.env_include) == "table" and src.env_include or {}) do
				include[#include + 1] = n
			end
			for _, n in ipairs(type(src.env_exclude) == "table" and src.env_exclude or {}) do
				exclude[#exclude + 1] = n
			end
		end
		return {
			name = e.name,
			type = srv.type,
			command = srv.command,
			args = srv.args,
			env = srv.env,
			env_include = include,
			env_exclude = exclude,
			cwd = srv.command and (srv.cwd or mare.cwd) or nil,
			roots = { mare.cwd },
			url = srv.url,
			headers = srv.headers,
			bearer_token = srv.bearer_token,
			oauth = oauth,
			tls = srv.tls,
			timeout_ms = timeout,
		}
	end

	local connect, apply_tools, watch, disconnect, unregister -- forward
	local in_turn = false
	-- Server input callbacks belong to the agent request, like its tool tasks.
	-- MCP has no parent-tool id for elicitation; never guess one for concurrent calls.
	local input_tasks = {}
	local function cancel_inputs()
		local pending = input_tasks
		input_tasks = {}
		for record in pairs(pending) do
			record.task:abort()
			pcall(record.client.respond, record.client, record.id, { action = "cancel" })
		end
	end
	local pending_tools = {}
	local function flush_tool_updates()
		-- apply_tools can yield: take the entries out first, so an update queued meanwhile
		-- isn't lost or applied by a table traversal that was modified underneath it
		local updates = {}
		for name, update in pairs(pending_tools) do
			updates[#updates + 1] = { name = name, update = update }
		end
		table.sort(updates, function(a, b)
			return a.name < b.name
		end)
		pending_tools = {}
		for _, u in ipairs(updates) do
			local e = servers[u.name]
			if e and e.client == u.update.client then
				apply_tools(e, u.update.tools, u.update.generation)
				-- the deferred tools are registered now: listeners that saw a server ready
				-- with no tools (mcp/ready during a turn) learn about them
				ctx.bus:emit("mcp/changed", { server = u.name })
			end
		end
	end
	ctx:on("step/started", function()
		in_turn = true
	end)
	ctx:on("step/completed", function(ev)
		if ev and ev.will_retry then
			return
		end
		in_turn = false
		flush_tool_updates()
	end)
	-- Failed or interrupted requests can exit without a non-retry step/completed.
	ctx:on("turn/completed", function()
		cancel_inputs()
		in_turn = false
		flush_tool_updates()
	end)

	local function fail(e, err, quiet)
		e.error = mare.mcp.redact(tostring(err))
		e.state = (e.error:find("not logged in", 1, true) or e.error:find("401", 1, true)) and "needs_login" or "error"
		ctx.bus:emit("mcp/error", { server = e.name, error = e.error })
		mare.mcp.log(e.name, "error", e.error)
		if not quiet and not e.told then
			-- the model hears once that the server is gone
			e.told = true
			agent_notify(("MCP server %s is unavailable: %s"):format(e.name, e.error))
		end
	end

	--- the client of a server, connecting on first use (non-direct tiers)
	local function client_of(name, cancel)
		local e = servers[name]
		local state, why = blocked(e)
		if state then
			disconnect(e)
			local names = {}
			for tname in pairs(e.registered) do
				names[#names + 1] = tname
			end
			for _, tname in ipairs(names) do
				unregister(e, tname)
			end
			apply_prompts(e, {})
			e.state, e.error = state, (state ~= "disabled") and why or nil
			require("mare_lib").fail("mcp.connect_failed", ("MCP %s: %s"):format(name, why))
		end
		-- while connecting, `e.client` exists before its tools are registered: wait for it
		if e.client and not e.connecting then
			return e.client
		end
		connect(name, true, nil, nil, cancel)
		if not e.client or e.connecting then
			require("mare_lib").fail("mcp.connect_failed", ("MCP %s: %s"):format(name, e.error or "not connected"))
		end
		return e.client
	end

	--- register one tool of a server (declared from the next request on)
	local function register(e, t, opts)
		local tname = e.names[t.name]
		if not tname or e.registered[tname] then
			return tname
		end
		if e.client and e.list_generation ~= e.client:tools_generation() then
			mare.mcp.log(e.name, "warn", "refusing stale MCP tool list for " .. tostring(t.name))
			return nil
		end
		if t.schema_error then
			local reason = ("MCP %s tool %s schema: %s"):format(e.name, tostring(t.name), t.schema_error)
			mare.mcp.log(e.name, "warn", reason)
			ctx.bus:emit("mcp/error", { server = e.name, error = reason })
			return nil
		end
		local source = e.client
		local generation = source and source:tools_generation() or nil
		local remove
		local function current()
			return source == nil and e.client == nil
				or source ~= nil and e.client == source and source:tools_generation() == generation
		end
		local tt = tool_tier(e, t.name)
		local exposure = (opts and opts.exposure) or (tt == "codemode" and "nested" or nil)
		local ok
		ok, remove = pcall(ctx.tools.register, ctx.tools, tname, {
			description = ("[MCP %s] %s"):format(e.name, type(t.description) == "string" and t.description or t.name),
			parameters = t.inputSchema,
			unconfined = true,
			exposure = exposure,
			sort_key = e.name .. "\0" .. tostring(t.name) .. "\0" .. tname,
			search = { aliases = { t.name }, text = e.name .. " " .. (e.srv.description or "") },
			timeout_policy = "managed", -- native call owns cancellable idle/total active-time budgets
			is_current = current,
			side_effect = (type(t.annotations) == "table" and t.annotations.readOnlyHint) and "read" or "external",
			summary = function()
				return ("%s:%s"):format(e.name, t.name)
			end,
			execute = function(a, t_ctx)
				local timeout = e.srv.call_timeout_ms or cfg.call_timeout_ms
				local cok, r = pcall(function()
					local bstate, bwhy = blocked(e)
					if bstate then
						require("mare_lib").fail("mcp.connect_failed", ("MCP %s: %s"):format(e.name, bwhy))
					end
					local client = client_of(e.name, t_ctx and t_ctx.cancel)
					if
						source == nil
						and e.registered[tname] == remove
						and e.list_generation == client:tools_generation()
					then
						-- A cached tool's first connection confirmed this exact definition.
						source, generation = client, client:tools_generation()
					end
					if e.registered[tname] ~= remove or not current() then
						error("MCP tool definition changed before dispatch")
					end
					return client:call(t.name, a, {
						timeout_ms = timeout,
						tools_generation = generation,
						progress = t_ctx and t_ctx.update and function(p)
							local msg = p.message
							if not msg or msg == "" then
								if p.total then
									msg = ("%g/%g"):format(p.progress or 0, p.total)
								elseif p.progress ~= nil then
									msg = tostring(p.progress)
								end
							end
							if msg and msg ~= "" then
								t_ctx.update({
									output = msg,
									progress = p.progress,
									total = p.total,
									message = p.message,
								})
							end
						end or nil,
					})
				end)
				if not cok then
					return { content = ("MCP %s: %s"):format(e.name, tostring(r)), is_error = true }
				end
				return { content = content_of(e.name, r), is_error = r.isError == true }
			end,
		})
		if not ok then
			local reason = ("MCP %s tool %s: %s"):format(e.name, tostring(t.name), tostring(remove))
			mare.mcp.log(e.name, "warn", reason)
			ctx.bus:emit("mcp/error", { server = e.name, error = reason })
			return nil
		end
		owned[tname] = e.name
		e.registered[tname] = type(remove) == "function" and remove or function() end
		e.registered_stale[tname] = function()
			return source ~= nil and not current()
		end
		e.registered_activate[tname] = function(client)
			if source == nil and e.client == client and e.list_generation == client:tools_generation() then
				source, generation = client, e.list_generation
			end
		end
		-- The definition fingerprint preserves JSON array/object distinctions
		-- (including empty arrays in schema defaults and enums).
		e.registered_defs[tname] = mare.json.encode_strict(t)
		return tname
	end
	unregister = function(e, tname)
		local remove = e.registered[tname]
		if remove then
			e.registered[tname] = nil
			e.registered_defs[tname] = nil
			e.registered_stale[tname] = nil
			e.registered_activate[tname] = nil
			owned[tname] = nil
			pcall(remove)
		end
	end

	--- a (new) tool list: add new tools, withdraw removed ones, refresh the cache
	apply_tools = function(e, tools, generation)
		if blocked(e) then
			-- disabled or unapproved meanwhile (e.g. while its connect yielded): register nothing,
			-- so tool_search cannot find it; checked once here, the registration below never yields
			return {}
		end
		e.list_generation = generation
		tools = filter_tools(e, tools)
		e.tools = tools
		e.names = name_tools(e, tools)
		local keep = {}
		for _, t in ipairs(tools) do
			keep[e.names[t.name]] = t
		end
		local replace = {}
		for tname in pairs(e.registered) do
			if not keep[tname] then
				unregister(e, tname)
			elseif e.registered_defs[tname] ~= mare.json.encode_strict(keep[tname]) or e.registered_stale[tname]() then
				-- The name survived, but its schema, description or safety hints changed.
				-- Re-register even a previously discovered deferred tool, so approval and
				-- validation use the new definition instead of stale readOnlyHint data.
				replace[#replace + 1] = tname
			elseif e.client then
				e.registered_activate[tname](e.client)
			end
		end
		for _, tname in ipairs(replace) do
			unregister(e, tname)
			local t = keep[tname]
			local tt = tool_tier(e, t.name)
			register(e, t, tt == "deferred" and { exposure = "deferred" } or nil)
		end
		if e.tier ~= "direct" then
			pending_cache[e.name] = tools
		end
		local direct, cm = {}, {}
		for _, t in ipairs(tools) do
			local tt = tool_tier(e, t.name)
			if tt == "direct" then
				direct[#direct + 1] = register(e, t)
			elseif tt == "deferred" then
				register(e, t, { exposure = "deferred" })
			elseif tt == "codemode" and codemode() then
				cm[#cm + 1] = t
			end
		end
		if #cm > 0 then
			ctx.bus:emit("mcp/codemode", { server = e.name, tools = cm })
		end
		return direct
	end
	--- server events: list_changed (debounced in the core), reconnects, elicitation
	watch = function(e, client)
		ctx:spawn(function()
			while true do
				local ok, ev = pcall(client.next_event, client)
				if not ok or ev == nil or e.client ~= client then
					break
				end
				if ev.type == "elicit" then
					local record = { client = client, id = ev.id }
					input_tasks[record] = true
					record.task = ctx:spawn(function()
						local cleanup <close> = setmetatable({}, {
							__close = function()
								input_tasks[record] = nil
								pcall(client.respond, client, ev.id, { action = "cancel" })
							end,
						})
						if ev.cancel:is_cancelled() then
							return
						end
						mare.task.select(function()
							elicit(e, client, ev, ctx, D.svc)
						end, function()
							ev.cancel:wait()
						end)
					end)
				else
					local rok, err = true, nil
					if ev.type ~= "reconnected" then
						rok, err = pcall(client.refresh, client)
					end
					if rok then
						local tools, generation = client:tools()
						if in_turn then
							pending_tools[e.name] = { client = client, tools = tools, generation = generation }
						else
							apply_tools(e, tools, generation)
						end
						apply_prompts(e, client:prompts())
						e.resources = client:resources()
						e.templates = client:resource_templates()
						ctx.bus:emit("mcp/changed", { server = e.name })
					else
						mare.mcp.log(e.name, "warn", "refresh after " .. ev.type .. ": " .. tostring(err))
					end
				end
			end
		end)
	end

	--- `claimed`: the caller (an eager start) already set `e.connecting`; it stays set through the
	--- checks below, which yield (they read state files), so a required server is never seen as
	--- neither connecting nor connected while its start is under way
	connect = function(name, quiet, defer_if_busy, claimed, caller_cancel)
		local e = servers[name]
		local mare_lib = require("mare_lib")
		if e.connecting and not claimed then
			-- A client can exist while its tools are still being registered.
			local deadline = mare.task.clock() + spec_of(e).timeout_ms + 5000
			while e.connecting and mare.task.clock() < deadline do
				if caller_cancel and caller_cancel:is_cancelled() then
					return false
				end
				mare.task.sleep(20, caller_cancel)
			end
			return e.client ~= nil and not e.connecting and e.state == "connected"
		end
		-- Guard any pre-claimed `e.connecting = true` across yielding preflight checks.
		local claimed_guard
		if claimed then
			claimed_guard = mare_lib.guard(function()
				e.connecting = false
				if e.state == "connecting" then
					e.state = "idle"
				end
			end)
		end
		local _claimed_cleanup <close> = claimed_guard
		if caller_cancel and caller_cancel:is_cancelled() then
			return false
		end
		if e.client then
			if claimed_guard then
				claimed_guard:disarm()
			end
			e.connecting = false
			return true
		end
		if blocked(e) then
			return false
		end
		if e.srv.oauth and e.srv.url and not mare.mcp.has_credentials(name, e.srv.url) then
			e.state, e.error = "needs_login", ("not logged in — run /mcp login %s"):format(name)
			return false
		end
		local my_gen = (e.connect_gen or 0) + 1
		e.connect_gen = my_gen
		local cancel_tok = caller_cancel and caller_cancel:child() or mare.task.token()
		e.connect_cancel = cancel_tok
		if claimed_guard then
			claimed_guard:disarm()
		end
		local completed = false
		local connected_client = nil
		local previous = nil
		e.connecting, e.error, e.state = true, nil, "connecting"
		local _conn_guard <close> = mare_lib.guard(function()
			if e.connect_cancel == cancel_tok then
				e.connect_cancel = nil
			end
			if not completed then
				pcall(cancel_tok.cancel, cancel_tok)
				if e.connect_gen == my_gen then
					e.connecting = false
					if connected_client and e.client == connected_client then
						e.client = nil
					end
					pending_tools[e.name] = nil
					if previous then
						for tname in pairs(e.registered) do
							if not previous[tname] then
								unregister(e, tname)
							end
						end
					end
					if e.state == "connecting" then
						e.state = "idle"
					end
				end
				if connected_client then
					pcall(connected_client.close, connected_client)
				end
			end
		end)
		local ok, client = pcall(mare.mcp.connect, spec_of(e), cancel_tok)
		if e.connect_cancel == cancel_tok then
			e.connect_cancel = nil
		end
		if not ok then
			if e.connect_gen ~= my_gen then
				completed = true
				return false
			end
			e.connecting = false
			if cancel_tok:is_cancelled() then
				local bstate, bwhy = blocked(e)
				if bstate then
					e.state, e.error = bstate, (bstate ~= "disabled") and bwhy or nil
				elseif e.state == "connecting" then
					e.state = "idle"
				end
				completed = true
				return false
			end
			fail(e, client, quiet)
			completed = true
			return false
		end
		connected_client = client
		if e.connect_gen ~= my_gen or cancel_tok:is_cancelled() or blocked(e) then
			pcall(client.close, client)
			connected_client = nil
			if e.connect_gen == my_gen then
				e.connecting = false
				local bstate, bwhy = blocked(e)
				if bstate then
					e.state, e.error = bstate, (bstate ~= "disabled") and bwhy or nil
				elseif e.state == "connecting" then
					e.state = "idle"
				end
			end
			completed = true
			return false
		end
		-- A transport is not ready until its tools and prompts are available and registered.
		-- These operations may yield: retain `connecting` through the entire sequence.
		previous = {}
		for tname in pairs(e.registered) do
			previous[tname] = true
		end
		local ready, direct = pcall(function()
			local iok, info = pcall(client.info, client)
			local resources = client:resources()
			local templates = client:resource_templates()
			local tools, generation = client:tools()
			local prompts = client:prompts()
			if e.connect_gen ~= my_gen or cancel_tok:is_cancelled() or blocked(e) then
				error("MCP connection cancelled")
			end
			e.client, e.info, e.resources, e.templates = client, iok and info or {}, resources, templates
			local declared = {}
			if in_turn and defer_if_busy then
				pending_tools[e.name] = { client = client, tools = tools, generation = generation }
			else
				declared = apply_tools(e, tools, generation)
			end
			apply_prompts(e, prompts)
			watch(e, client)
			return declared
		end)
		local st, why
		if ready then
			st, why = blocked(e)
			if not st and (e.connect_gen ~= my_gen or cancel_tok:is_cancelled()) then
				st, why = "untrusted", "MCP connection cancelled during startup"
			end
		end
		if st then
			-- disabled, unapproved, or revoked while connecting: no live client, nothing stays
			ready, direct = false, nil
		end
		if not ready then
			pcall(client.close, client)
			connected_client = nil
			if e.connect_gen == my_gen then
				e.connecting = false
				if e.client == client then
					e.client = nil
				end
				pending_tools[e.name] = nil
				for tname in pairs(e.registered) do
					if not previous[tname] then
						unregister(e, tname)
					end
				end
				if st then
					apply_prompts(e, {})
					e.state, e.error = st, (st ~= "disabled") and why or nil
				else
					fail(e, direct, quiet)
				end
			end
			completed = true
			return false
		end
		if e.client_effect then
			local prev_eff = e.client_effect
			e.client_effect = nil
			pcall(prev_eff)
		end
		e.client_effect = ctx:effect(function()
			return function()
				pcall(client.close, client)
			end
		end, "cleanup")
		e.connecting, e.state, e.told = false, "connected", nil
		completed = true
		ctx.bus:emit("mcp/ready", { server = name, tools = direct })
		return true
	end

	disconnect = function(e)
		e.connect_gen = (e.connect_gen or 0) + 1
		if e.connect_cancel then
			pcall(e.connect_cancel.cancel, e.connect_cancel)
		end
		pending_tools[e.name] = nil
		local eff = e.client_effect
		e.client_effect = nil
		local c = e.client
		e.client = nil
		if eff then
			pcall(eff)
		elseif c then
			pcall(c.close, c)
		end
		if e.connecting then
			local deadline = mare.task.clock() + 5000
			while e.connecting and mare.task.clock() < deadline do
				mare.task.sleep(10)
			end
			e.connecting = false
		end
	end

	return {
		connect = connect,
		client_of = client_of,
		register = register,
		unregister = unregister,
		fail = fail,
		disconnect = disconnect,
	}
end
