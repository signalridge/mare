-- /login: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /login. Runs in the commands plugin's env.
return function(ctx, args)
	local llm = ctx:get("llm")
	local list = {}
	for _, p in ipairs(llm:providers()) do
		if p.login then
			list[#list + 1] = p
		end
	end
	local p, login_opts
	if args ~= "" then
		for _, x in ipairs(list) do
			if x.name == args then
				p = x
			end
		end
	else
		local items = {}
		for i, x in ipairs(list) do
			items[i] = { label = x.name, detail = x.login_label }
		end
		items[#items + 1] = { label = "Paste an API key", detail = "identify provider by key prefix" }
		local i = ui.select({ title = "Log in to", items = items })
		if i == #items then
			local key = ui.input({ title = "Paste API key", secret = true })
			key = key and key:match("^%s*(.-)%s*$")
			if not key or key == "" then
				return
			end
			local detected = llm.identify_key_provider and llm:identify_key_provider(key) or nil
			if detected then
				for _, x in ipairs(list) do
					if x.name == detected then
						p = x
						break
					end
				end
			end
			if not p then
				local pick = {}
				for idx, x in ipairs(list) do
					pick[idx] = { label = x.name, detail = x.login_label }
				end
				local j =
					ui.select({ title = "Could not identify provider from key prefix — choose one", items = pick })
				p = j and list[j]
			end
			if not p then
				return
			end
			login_opts = { key = key, method = "api_key" }
		else
			p = i and list[i]
		end
	end
	if not p then
		if args ~= "" then
			ui.notify("No login for " .. args, "error")
		end
		return
	end
	ctx:spawn(function()
		-- llm.login: the provider's flow, then a catalogue refresh
		local ok, err = pcall(ctx:get("llm").login, ctx:get("llm"), p.name, login_opts)
		if ok then
			ui.notify("Logged in to " .. p.name, "success")
		else
			ui.notify(
				"Login failed: " .. tostring(type(err) == "table" and (err.message or mare.json.encode(err)) or err),
				"error"
			)
		end
	end)
end
