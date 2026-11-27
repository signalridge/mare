-- mcp.prompts: server prompts as `/mcp__<server>__<prompt>` slash commands;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local sanitize = D.sanitize

	-- client_of lives in mcp.connection, which is built after this module (it needs apply_prompts)
	local function client_of(name)
		return D.connection.client_of(name)
	end

	-- ---- prompts as slash commands `/mcp__<server>__<prompt>` ----------------------

	local function prompt_args(p, line)
		local decl = type(p.arguments) == "table" and p.arguments or {}
		local args, pos = {}, {}
		for tok in (line or ""):gmatch("%S+") do
			local k, v = tok:match("^([%w_%-]+)=(.*)$")
			if k then
				args[k] = v
			else
				pos[#pos + 1] = tok
			end
		end
		local free = {}
		for _, d in ipairs(decl) do
			if args[d.name] == nil then
				free[#free + 1] = d.name
			end
		end
		for i, v in ipairs(pos) do
			local slot = free[i]
			if not slot then
				if free[#free] then
					args[free[#free]] = args[free[#free]] .. " " .. v
				end
			else
				args[slot] = v
			end
		end
		for _, d in ipairs(decl) do
			if d.required and (args[d.name] == nil or args[d.name] == "") then
				local usage = {}
				for _, x in ipairs(decl) do
					usage[#usage + 1] = x.required and ("<%s>"):format(x.name) or ("[%s]"):format(x.name)
				end
				error(("missing argument %s — usage: %s"):format(d.name, table.concat(usage, " ")))
			end
		end
		return args
	end
	local function apply_prompts(e, prompts)
		e.prompts = {}
		local commands = D.svc.commands
		if not commands then
			return
		end
		local seen = {}
		for _, p in ipairs(type(prompts) == "table" and prompts or {}) do
			if type(p) == "table" and type(p.name) == "string" then
				e.prompts[p.name] = p
				local cmd = "mcp__" .. sanitize(e.name) .. "__" .. sanitize(p.name)
				seen[cmd] = true
				if not e.prompt_cmds[cmd] then
					local pname = p.name
					local args_doc = {}
					for _, a in ipairs(type(p.arguments) == "table" and p.arguments or {}) do
						args_doc[#args_doc + 1] = a.required and ("<%s>"):format(a.name) or ("[%s]"):format(a.name)
					end
					e.prompt_cmds[cmd] = commands:register(cmd, {
						description = ("[MCP %s prompt] %s%s"):format(
							e.name,
							p.description or pname,
							#args_doc > 0 and (" — " .. table.concat(args_doc, " ")) or ""
						),
						run = function(line)
							local cur = e.prompts[pname]
							if not cur then
								error(("MCP %s no longer offers the prompt %s"):format(e.name, pname))
							end
							local args = prompt_args(cur, line)
							local client = client_of(e.name)
							local r = client:get_prompt(
								pname,
								args,
								{ timeout_ms = e.srv.call_timeout_ms or cfg.call_timeout_ms }
							)
							local texts, images = {}, {}
							local msgs = type(r.messages) == "table" and r.messages or {}
							local show_role = #msgs > 1
							for _, m in ipairs(msgs) do
								local role_prefix = ""
								if type(m.role) == "string" and m.role ~= "" and (show_role or m.role ~= "user") then
									role_prefix = ("[%s] "):format(m.role)
								end
								local c = m.content
								if type(c) == "table" and c.type == "text" then
									texts[#texts + 1] = role_prefix .. (c.text or "")
								elseif type(c) == "table" and c.type == "image" and c.data then
									images[#images + 1] = { type = "image", data = c.data, mime = c.mimeType }
									if role_prefix ~= "" then
										texts[#texts + 1] = role_prefix .. "[image]"
									end
								elseif type(c) == "table" and c.type == "resource" and type(c.resource) == "table" then
									texts[#texts + 1] = role_prefix
										.. (c.resource.text or ("[resource " .. tostring(c.resource.uri) .. "]"))
								elseif type(c) == "table" then
									texts[#texts + 1] = role_prefix .. ("[%s content]"):format(tostring(c.type))
								end
							end
							ctx.bus:emit("input/submit", {
								text = table.concat(texts, "\n\n"),
								images = #images > 0 and images or nil,
							})
						end,
					})
				end
			end
		end
		for cmd, remove in pairs(e.prompt_cmds) do
			if not seen[cmd] then
				-- the release is the effect disposer, a callable table
				if require("mare_lib").callable(remove) then
					pcall(remove)
				end
				e.prompt_cmds[cmd] = nil
			end
		end
	end

	return { apply_prompts = apply_prompts }
end
