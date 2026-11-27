-- /mcp: the interactive panel and the subcommands, for the command registered by
-- plugins/mcp.lua (the entry layer); loaded on the first /mcp. `D` carries the
-- plugin state the command reads (tables shared with the entry, never copies).
return function(D, args)
	local ctx = D.ctx
	local servers = D.servers
	local actions = D.actions
	local config_errors = D.config_errors
	local state_of = D.state_of
	local transport_of = D.transport_of
	local sorted_entries = D.sorted_entries
	local summary_line = D.summary_line
	local zero_tools_hint = D.zero_tools_hint
	local tools_lines = D.tools_lines
	local tools_text = D.tools_text

	local function details(e)
		local st = state_of(e)
		local kind, where = transport_of(e)
		local count = #(e.tools or {})
		local hint = zero_tools_hint(e, st)
		local body = {
			("state: %s%s"):format(st, e.error and st ~= "connected" and (" — " .. e.error) or ""),
			("source: %s (%s)"):format(e.source, e.file),
			("transport: %s %s"):format(kind, where),
			("tier: %s, %d tools%s"):format(e.tier, count, hint and (" — " .. hint) or ""),
		}
		if #(e.missing or {}) > 0 then
			body[#body + 1] = "missing variables: " .. table.concat(e.missing, ", ")
		end
		local tail = e.client and e.client:stderr() or {}
		if #tail > 0 then
			body[#body + 1] = "stderr:"
			for i = math.max(1, #tail - 5), #tail do
				body[#body + 1] = "  " .. tail[i]
			end
		end
		return body
	end
	local function run_action(act, e)
		local ok, res, err = pcall(function()
			if act == "login" then
				return actions.login(e.name, function(url)
					mare.ui.notify("Opened the browser to log in to " .. e.name .. ": " .. url, "info")
				end)
			end
			return actions[act](e.name)
		end)
		if not ok then
			mare.ui.notify(("MCP %s %s: %s"):format(act, e.name, tostring(res)), "error")
		elseif act == "logout" then
			mare.ui.notify(res and ("Forgot the MCP tokens for " .. e.name) or ("No stored MCP tokens for " .. e.name))
		elseif res == false then
			mare.ui.notify(("MCP %s %s failed: %s"):format(act, e.name, tostring(err or e.error)), "error")
		else
			mare.ui.notify(("MCP %s: %s done"):format(e.name, act), "success")
		end
	end
	local function interactive(filter)
		local list = sorted_entries()
		local items = {}
		for _, e in ipairs(list) do
			local st = state_of(e)
			local kind, where = transport_of(e)
			local count = #(e.tools or {})
			local hint = zero_tools_hint(e, st)
			items[#items + 1] = {
				label = hint and ("%s  [%s, %d tools]"):format(e.name, st, count) or ("%s  [%s]"):format(e.name, st),
				detail = hint and ("%s · %s · %s %s"):format(hint, e.source, kind, where)
					or ("%s · %s %s · %d tools"):format(e.source, kind, where, count),
			}
		end
		local i = mare.ui.select({ title = "MCP servers", items = items, filter = filter or "" })
		local e = i and list[i]
		if not e then
			return
		end
		local st = state_of(e)
		local acts = {}
		if st == "connected" then
			acts = { "tools", "reconnect", e.srv.oauth and "logout" or nil, "disable" }
		elseif st == "needs_login" then
			acts = { "login", "disable" }
		elseif st == "needs_approval" then
			acts = { "enable", "tools" }
		elseif st == "disabled" then
			acts = { "enable" }
		else
			acts = { "reconnect", "tools", "disable" }
		end
		local labels, keys = {}, {}
		for _, a in pairs(acts) do
			keys[#keys + 1] = a
		end
		local rank = { login = 1, enable = 1, tools = 2, reconnect = 3, logout = 4, disable = 5 }
		table.sort(keys, function(a, b)
			return rank[a] < rank[b]
		end)
		for _, a in ipairs(keys) do
			labels[#labels + 1] = a == "tools" and "Show tools and schemas" or (a:sub(1, 1):upper() .. a:sub(2))
		end
		local j = mare.ui.select({ title = "MCP " .. e.name, body = details(e), items = labels })
		local act = j and keys[j]
		if act == "tools" then
			mare.ui.overlay("MCP " .. e.name .. " tools", tools_lines(e))
		elseif act then
			run_action(act, e)
		end
	end

	local sub, name = (args or ""):match("^(%S*)%s*(.-)%s*$")
	local e = servers[name]
	if sub == "login" then
		if not e then
			return ui.notify("no MCP server named " .. tostring(name), "error")
		end
		ctx:spawn(function()
			run_action("login", e)
		end)
		return
	elseif sub == "logout" or sub == "reconnect" or sub == "enable" or sub == "disable" then
		if not e then
			return ui.notify("no MCP server named " .. tostring(name), "error")
		end
		run_action(sub, e)
		return
	elseif sub == "tools" then
		if not e then
			return ui.notify("no MCP server named " .. tostring(name), "error")
		end
		if mare.ui.connected() then
			mare.ui.overlay("MCP " .. name .. " tools", tools_lines(e))
		else
			mare.ui.notify(tools_text(e))
		end
		return
	end
	if mare.ui.connected() and next(servers) then
		return interactive(sub ~= "list" and args or nil)
	end
	local lines = {}
	for _, x in ipairs(sorted_entries()) do
		lines[#lines + 1] = summary_line(x)
	end
	for _, err in ipairs(config_errors) do
		lines[#lines + 1] = "config error: " .. err
	end
	mare.ui.notify(#lines > 0 and table.concat(lines, "\n") or "No MCP servers configured")
end
