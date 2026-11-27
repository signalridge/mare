-- mcp.startup: the server entries, cached direct tools, eager starts, startup approvals
-- and trust/changed;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local servers, config_errors, load_cache = D.servers, D.config_errors, D.load_cache
	local approve = D.approvals.approve
	local merged, order, resolve, server_tier, blocked =
		D.config.merged, D.config.order, D.config.resolve, D.config.server_tier, D.config.blocked
	local name_tools, tool_tier, has_direct_tools, eager, filter_tools =
		D.exposure.name_tools,
		D.exposure.tool_tier,
		D.exposure.has_direct_tools,
		D.exposure.eager,
		D.exposure.filter_tools
	local connect, register, fail = D.connection.connect, D.connection.register, D.connection.fail

	-- ---- build the entries ------------------------------------------------------------

	local norm_owner = {}
	table.sort(order)
	for _, name in ipairs(order) do
		local m = merged[name]
		local e = {
			name = name,
			raw = m.raw,
			source = m.source,
			file = m.file,
			overlays = m.overlays,
			rejected = m.rejected,
			tools = {},
			names = {},
			registered = {},
			registered_defs = {},
			registered_stale = {},
			registered_activate = {},
			prompt_cmds = {},
			prompts = {},
			resources = {},
			templates = {},
			state = "idle",
		}
		e.srv = resolve(e)
		e.tier = server_tier(name, e.srv)
		-- names that differ only in - and _ sanitize alike: the second is rejected
		local key = name:gsub("%-", "_")
		if norm_owner[key] then
			e.rejected = ("server name %s collides with %s after sanitizing; rename one"):format(name, norm_owner[key])
			config_errors[#config_errors + 1] = e.rejected
		else
			norm_owner[key] = name
		end
		if not e.rejected then
			if type(e.srv.type) == "string" and e.srv.type:lower() == "sse" then
				e.rejected = "the legacy SSE transport isn't supported; use the server's streamable HTTP endpoint "
					.. "(commonly /mcp instead of /sse)"
			elseif not (type(e.srv.command) == "string" or type(e.srv.url) == "string") then
				e.rejected = "needs `command` (stdio) or `url` (streamable HTTP)"
			end
		end
		servers[name] = e
		local st, why = blocked(e)
		if st then
			e.state, e.error = st, (st ~= "disabled") and why or nil
		end
		if e.tier ~= "direct" then
			e.tools = filter_tools(e, load_cache(name) or {})
			e.names = name_tools(e, e.tools)
			e.cached = #e.tools > 0
		end
	end
	-- When `--tools` (`tools_enabled`) is set, skip auto-connecting, caching, and prompting
	-- approval for any MCP server whose tools (`mcp__<server>__*`) are completely excluded.
	local function tools_enabled_allows_server(name)
		local only = mare.config.tools_enabled
		if type(only) ~= "table" then
			return true
		end
		local prefix = "mcp__" .. D.sanitize(name) .. "__"
		for _, item in ipairs(only) do
			if type(item) == "string" and item:sub(1, #prefix) == prefix then
				return true
			end
		end
		return false
	end

	-- direct and cached deferred tools of a non-direct server: declared from the first
	-- request on (calling one connects the server; deferred ones stay hidden on non-Anthropic
	-- models until tool_search loads them)
	local function register_cached(e)
		if e.cached and not blocked(e) and tools_enabled_allows_server(e.name) then
			for _, t in ipairs(e.tools) do
				local tt = tool_tier(e, t.name)
				if tt == "direct" then
					register(e, t)
				elseif tt == "deferred" then
					register(e, t, { exposure = "deferred" })
				end
			end
		end
	end
	for _, name in ipairs(order) do
		register_cached(servers[name])
	end
	for _, err in ipairs(config_errors) do
		mare.mcp.log("config", "error", err)
	end

	--- start the servers that connect at startup: `direct` and `required` ones
	local function start(e)
		if blocked(e) or e.client or e.connecting or not tools_enabled_allows_server(e.name) then
			return
		end
		if eager(e) then
			e.connecting = true -- a required server is waited for from the start
			e.state = "connecting"
			ctx:spawn(function()
				connect(e.name, false, true, true)
			end)
		end
	end
	for _, name in ipairs(order) do
		start(servers[name])
	end

	-- approvals at startup: `--approve-mcps` approves all; the TUI asks; headless says how
	local waiting = {}
	for _, name in ipairs(order) do
		if servers[name].state == "needs_approval" and tools_enabled_allows_server(name) then
			waiting[#waiting + 1] = servers[name]
		end
	end
	local function after_approval(e)
		e.srv = resolve(e)
		e.state, e.error = "idle", nil
		local st, why = blocked(e)
		if st then
			e.state, e.error = st, why
		end
		-- enabled or approved again: its cached tools are found by tool_search as at startup
		register_cached(e)
		start(e)
	end
	if #waiting > 0 then
		if mare.args and mare.args.approve_mcps then
			for _, e in ipairs(waiting) do
				approve(e)
				after_approval(e)
			end
		else
			for _, e in ipairs(waiting) do
				e.approval_pending = true
			end
			ctx:spawn(function()
				for _ = 1, 150 do
					if mare.ui.connected() then
						break
					end
					mare.task.sleep(20)
				end
				if not mare.ui.connected() then
					for _, e in ipairs(waiting) do
						e.approval_pending = false
					end
					local names = {}
					for _, e in ipairs(waiting) do
						names[#names + 1] = e.name
					end
					mare.ui.notify(
						("MCP servers not approved yet (not started): %s — pass --approve-mcps or run /mcp enable <name>"):format(
							table.concat(names, ", ")
						),
						"warning"
					)
					return
				end
				for _, e in ipairs(waiting) do
					local what = e.srv.command
							and mare.mcp.redact(
								e.srv.command
									.. " "
									.. table.concat(type(e.srv.args) == "table" and e.srv.args or {}, " ")
							)
						or mare.mcp.redact(tostring(e.srv.url))
					local ok, err = pcall(function()
						local i = mare.ui.select({
							title = ("Start the MCP server %s from %s?"):format(e.name, e.file),
							body = { what, "Approval is remembered until its configuration changes." },
							items = { "Approve and start", "Not now" },
						})
						if i == 1 then
							approve(e)
							after_approval(e)
						end
					end)
					e.approval_pending = false
					if not ok then
						fail(e, err, true)
					end
				end
			end)
		end
	end
	-- Project trust changed: start newly allowed servers on grant, or tear down blocked project
	-- stdio clients, registered tools, and prompts immediately on revocation.
	ctx:on("trust/changed", function(ev)
		if type(ev) == "table" and ev.trusted and mare.trusted then
			for _, name in ipairs(order) do
				local e = servers[name]
				if e.state == "untrusted" or (e.source == "project" and not e.client) then
					e.srv = resolve(e)
					if blocked(e) == "needs_approval" and mare.args and mare.args.approve_mcps then
						approve(e)
					end
					after_approval(e)
				end
			end
		else
			for _, name in ipairs(order) do
				local e = servers[name]
				local st, why = blocked(e)
				if st then
					D.connection.disconnect(e)
					local names = {}
					for tname in pairs(e.registered) do
						names[#names + 1] = tname
					end
					for _, tname in ipairs(names) do
						D.connection.unregister(e, tname)
					end
					D.prompts.apply_prompts(e, {})
					e.tools = {}
					e.resources = {}
					e.templates = {}
					e.state, e.error = st, (st ~= "disabled") and why or nil
				end
			end
		end
	end)

	return {
		after_approval = after_approval,
		tools_enabled_allows_server = tools_enabled_allows_server,
	}
end
