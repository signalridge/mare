-- mcp.actions: state / summary helpers and the actions shared by /mcp, the CLI and doctor;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local servers, SOURCE_RANK = D.servers, D.SOURCE_RANK
	local approved, approve, enabled, set_enabled =
		D.approvals.approved, D.approvals.approve, D.approvals.enabled, D.approvals.set_enabled
	local order, blocked = D.config.order, D.config.blocked
	local eager, tool_tier = D.exposure.eager, D.exposure.tool_tier
	local connect, client_of, unregister, fail, disconnect =
		D.connection.connect,
		D.connection.client_of,
		D.connection.unregister,
		D.connection.fail,
		D.connection.disconnect
	local after_approval = D.startup.after_approval

	-- ---- actions shared by /mcp, the CLI and doctor --------------------------------

	local function transport_of(e)
		if e.srv.command then
			return "stdio",
				mare.mcp.redact(
					table.concat(
						{ e.srv.command, table.concat(type(e.srv.args) == "table" and e.srv.args or {}, " ") },
						" "
					)
				)
		end
		return "http", mare.mcp.redact(tostring(e.srv.url))
	end
	local function state_of(e)
		if e.connecting then
			return "connecting" -- `e.client` exists before the tools are registered
		end
		if e.client then
			local ok, st = pcall(e.client.status, e.client)
			if ok and st ~= "connected" then
				return "disconnected"
			end
			return "connected"
		end
		local st = blocked(e)
		return st or e.state or "idle"
	end
	local function sorted_entries()
		local list = {}
		for _, n in ipairs(order) do
			list[#list + 1] = servers[n]
		end
		table.sort(list, function(a, b)
			local ra, rb = SOURCE_RANK[a.source] or 9, SOURCE_RANK[b.source] or 9
			if ra ~= rb then
				return ra < rb
			end
			return a.name < b.name
		end)
		return list
	end
	local function zero_tools_hint(e, st)
		if st ~= "connected" or #(e.tools or {}) > 0 then
			return nil
		end
		local caps = type(e.info) == "table" and type(e.info.capabilities) == "table" and e.info.capabilities or nil
		if caps and caps.tools == nil then
			return "no tools exposed — server omitted tools capability"
		end
		return "no tools exposed — check server config or logs"
	end
	local function summary_line(e)
		local st = state_of(e)
		local kind, where = transport_of(e)
		local count = #(e.tools or {})
		local extra = e.error and st ~= "connected" and ("  — " .. mare.mcp.redact(tostring(e.error))) or ""
		if #(e.missing or {}) > 0 then
			extra = extra .. "  (missing variables: " .. table.concat(e.missing, ", ") .. ")"
		end
		local hint = zero_tools_hint(e, st)
		if hint then
			extra = extra .. "  (" .. hint .. ")"
		end
		return ("%-20s %-15s %-8s %-8s %s  %d tools%s"):format(
			e.name,
			st,
			e.source,
			e.tier,
			kind .. " " .. where,
			count,
			extra
		)
	end

	local function find(name)
		local e = servers[name or ""]
		if not e then
			require("mare_lib").fail("mcp.unknown_server", "no MCP server named " .. tostring(name))
		end
		return e
	end
	local actions = {}
	function actions.reconnect(name)
		local e = find(name)
		if e.client then
			local ok, err = pcall(e.client.reconnect, e.client)
			if not ok then
				fail(e, err, true)
				disconnect(e)
				return false, e.error
			end
			return true
		end
		e.error = nil
		return connect(name, true), e.error
	end
	function actions.enable(name)
		local e = find(name)
		local is_approved, approved_err = approved(e)
		if approved_err then
			error(approved_err, 0)
		end
		local is_enabled, enabled_err = enabled(e)
		if enabled_err then
			error(enabled_err, 0)
		end
		if not is_approved then
			approve(e)
		end
		if not is_enabled then
			set_enabled(e, true)
		end
		if e.state == "disabled" or e.state == "needs_approval" then
			e.state, e.error = "idle", nil
		end
		after_approval(e)
		return true
	end
	function actions.disable(name)
		local e = find(name)
		set_enabled(e, false)
		disconnect(e)
		for tname in pairs(e.registered) do
			unregister(e, tname)
		end
		D.prompts.apply_prompts(e, {})
		e.state, e.error = "disabled", nil
		return true
	end
	--- OAuth login; `on_url(url)` shows the address, optional `ask(title, opts)` reads a pasted
	--- redirect URL on a headless tty. Only for servers with `oauth`.
	function actions.login(name, on_url, ask)
		local e = servers[name or ""]
		if not e then
			error("no MCP server named " .. tostring(name))
		end
		if not (e.srv.url and e.srv.oauth) then
			error(("MCP server %s has no `oauth` configured — /mcp login only applies to OAuth servers"):format(name))
		end
		local opts = type(e.srv.oauth) == "table" and e.srv.oauth or {}
		local login = mare.mcp.oauth_begin(e.srv.url, name, opts)
		pcall(mare.proc.open, login.url)
		on_url(login.url)
		-- the browser on another machine (SSH) or headless tty: paste the redirected address instead
		local ssh = (mare.env.get("SSH_CONNECTION") or mare.env.get("SSH_TTY") or "") ~= ""
		local paste_task
		if ssh and mare.ui.connected() then
			paste_task = ctx:spawn(function()
				local pasted = mare.ui.input({
					title = "Browser on another machine? Paste the address it was redirected to",
					placeholder = login.redirect .. "?code=…",
				})
				if pasted and pasted ~= "" then
					login:paste(pasted)
				end
			end)
		elseif not mare.ui.connected() and type(ask) == "function" then
			paste_task = ctx:spawn(function()
				local pasted = ask(
					"Paste the redirected URL (" .. login.redirect .. "?code=…)",
					{ placeholder = login.redirect .. "?code=…" }
				)
				if pasted and pasted ~= "" then
					login:paste(pasted)
				end
			end)
		end
		local ok, err = pcall(function()
			login:finish()
		end)
		if paste_task and paste_task.cancel then
			pcall(paste_task.cancel, paste_task)
		end
		if not ok then
			error(err, 0)
		end
		-- the old connection is closed before reconnecting with the new tokens
		disconnect(e)
		e.state, e.error, e.told = "idle", nil, nil
		if eager(e) then
			connect(name, true)
		end
		return true
	end
	function actions.logout(name)
		local e = servers[name or ""]
		if not e then
			error("no MCP server named " .. tostring(name))
		end
		local had = mare.mcp.logout(name, e.srv.url)
		disconnect(e)
		if e.srv.oauth then
			e.state = "needs_login"
		end
		return had
	end
	local function tools_lines(e)
		local lines = {}
		for _, t in ipairs(e.tools or {}) do
			lines[#lines + 1] = ("%s  [%s]"):format(e.names[t.name] or t.name, tool_tier(e, t.name))
			for l in tostring(t.description or ""):gmatch("[^\n]+") do
				lines[#lines + 1] = "  " .. l
			end
			lines[#lines + 1] = "  schema: " .. mare.json.encode(t.inputSchema or { type = "object" })
		end
		if #lines == 0 then
			local hint = zero_tools_hint(e, state_of(e))
			lines[1] = hint and ("(connected, 0 tools — " .. hint .. ")") or "(no tools known yet)"
		end
		return lines
	end
	local function tools_text(e)
		return table.concat(tools_lines(e), "\n")
	end

	local function status_entry(e)
		local kind, where = transport_of(e)
		local st = state_of(e)
		return {
			name = e.name,
			state = st,
			source = e.source,
			tier = e.tier,
			transport = kind,
			target = where,
			tools = #(e.tools or {}),
			error = st ~= "connected" and e.error and mare.mcp.redact(tostring(e.error)) or nil,
			missing = #(e.missing or {}) > 0 and mare.json.array({ table.unpack(e.missing) }) or nil,
		}
	end

	local function each_matching_server(opts, fn)
		opts = type(opts) == "table" and opts or {}
		local only = opts.server
		if only then
			find(only)
		end
		for _, e in ipairs(sorted_entries()) do
			if not only or e.name == only then
				if opts.connect and not e.client and not blocked(e) then
					pcall(connect, e.name, true)
				end
				fn(e)
			end
		end
	end

	local function fail_connect(e, name, verb, err)
		local _, why = blocked(e)
		require("mare_lib").fail(
			"mcp.connect_failed",
			err or e.error or why or ("failed to " .. verb .. " MCP server " .. tostring(name))
		)
	end

	local function call_client(server, method, ...)
		local e = find(server)
		local client = client_of(e.name)
		local n = select("#", ...)
		local args = { ... }
		args[n + 1] = { timeout_ms = e.srv.call_timeout_ms or cfg.call_timeout_ms }
		local ok, r = pcall(client[method], client, table.unpack(args, 1, n + 1))
		if not ok then
			require("mare_lib").fail("mcp.request_failed", tostring(r))
		end
		return r
	end

	local service = {}
	function service.list_servers()
		local list = {}
		for _, e in ipairs(sorted_entries()) do
			list[#list + 1] = status_entry(e)
		end
		return {
			data = mare.json.array(list),
			errors = mare.json.array({ table.unpack(D.config_errors) }),
		}
	end
	function service.list_tools(opts)
		local tools = {}
		each_matching_server(opts, function(e)
			for _, t in ipairs(e.tools or {}) do
				tools[#tools + 1] = {
					server = e.name,
					name = t.name,
					registered_name = (e.names and e.names[t.name]) or t.name,
					description = t.description,
					input_schema = t.inputSchema,
					tier = tool_tier(e, t.name),
					schema_error = t.schema_error,
				}
			end
		end)
		return { data = mare.json.array(tools) }
	end
	function service.connect(name)
		local e = find(name)
		e.error = nil
		local ok = connect(name, true)
		if not ok then
			fail_connect(e, name, "connect")
		end
		return status_entry(e)
	end
	function service.reconnect(name)
		local e = find(name)
		local ok, err = actions.reconnect(name)
		if not ok then
			fail_connect(e, name, "reconnect", err)
		end
		return status_entry(e)
	end
	function service.list_resources(opts)
		local resources, templates = {}, {}
		each_matching_server(opts, function(e)
			for _, r in ipairs(e.resources or {}) do
				local item = table.copy(r)
				item.server = e.name
				resources[#resources + 1] = item
			end
			for _, t in ipairs(e.templates or {}) do
				local item = table.copy(t)
				item.server = e.name
				templates[#templates + 1] = item
			end
		end)
		return {
			data = mare.json.array(resources),
			resource_templates = mare.json.array(templates),
		}
	end
	function service.read_resource(server, uri)
		local r = call_client(server, "read_resource", tostring(uri))
		return { contents = mare.json.array(type(r) == "table" and type(r.contents) == "table" and r.contents or {}) }
	end
	function service.list_prompts(opts)
		local prompts = {}
		each_matching_server(opts, function(e)
			local names = {}
			for pname in pairs(e.prompts or {}) do
				names[#names + 1] = pname
			end
			table.sort(names)
			for _, pname in ipairs(names) do
				local item = table.copy(e.prompts[pname])
				item.server = e.name
				prompts[#prompts + 1] = item
			end
		end)
		return { data = mare.json.array(prompts) }
	end
	function service.get_prompt(server, name, arguments)
		local args = type(arguments) == "table" and arguments or nil
		local r = call_client(server, "get_prompt", tostring(name), args)
		return {
			description = type(r) == "table" and r.description or nil,
			messages = mare.json.array(type(r) == "table" and type(r.messages) == "table" and r.messages or {}),
		}
	end

	return {
		transport_of = transport_of,
		state_of = state_of,
		sorted_entries = sorted_entries,
		summary_line = summary_line,
		zero_tools_hint = zero_tools_hint,
		find = find,
		actions = actions,
		tools_lines = tools_lines,
		tools_text = tools_text,
		service = service,
	}
end
