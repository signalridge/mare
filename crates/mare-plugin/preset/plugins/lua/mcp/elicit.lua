-- mcp.elicit: elicitation requests as a UI form (headless declines);
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
-- ---- elicitation: a form in the UI; headless always declines --

local function elicit(e, client, ev, ctx, svc)
	local p = type(ev.params) == "table" and ev.params or {}
	local function answer(r)
		pcall(client.respond, client, ev.id, r)
	end
	local approvals = svc and svc.approvals
	if approvals then
		-- no mandatory policy of its own ("ask"): the approval policy's gates and reviewer
		-- decide; answering lets the request through to the form below
		local result = approvals:request("mcp_elicitation", { server = e.name, request = p }, function()
			return { decision = "ask" }
		end, function()
			return { decision = "allow" }
		end)
		if result.decision ~= "allow" then
			return answer({ action = "decline" })
		end
	end
	if ctx then
		local ok, handled = pcall(ctx.bus.bail, ctx.bus, "mcp/elicit", {
			server = e.name,
			id = ev.id,
			params = p,
		}, ev.cancel)
		if ok and type(handled) == "table" and type(handled.action) == "string" then
			return answer(handled)
		end
	end
	if not mare.ui.connected() or p.mode == "url" then
		return answer({ action = "decline" })
	end
	local i = mare.ui.select({
		title = ("MCP %s asks for input"):format(e.name),
		body = tostring(p.message or ""),
		items = { "Answer", "Decline", "Cancel" },
	})
	if i ~= 1 then
		return answer({ action = i == 2 and "decline" or "cancel" })
	end
	local schema = type(p.requestedSchema) == "table" and p.requestedSchema or {}
	local props = type(schema.properties) == "table" and schema.properties or {}
	local names = {}
	for k in pairs(props) do
		names[#names + 1] = k
	end
	table.sort(names)
	local content = {}
	for _, k in ipairs(names) do
		local f = props[k]
		local title = ("%s%s"):format(f.title or k, f.description and (" — " .. f.description) or "")
		local enum = f.enum or (type(f.items) == "table" and f.items.enum)
		if f.type == "boolean" then
			local j = mare.ui.select({ title = title, items = { "yes", "no" } })
			if not j then
				return answer({ action = "cancel" })
			end
			content[k] = j == 1
		elseif type(enum) == "table" then
			local j = mare.ui.select({ title = title, items = enum })
			if not j then
				return answer({ action = "cancel" })
			end
			content[k] = enum[j]
		else
			local v = mare.ui.input({ title = title, placeholder = f.default and tostring(f.default) or "" })
			if v == nil then
				return answer({ action = "cancel" })
			end
			if f.type == "number" or f.type == "integer" then
				v = tonumber(v)
			end
			content[k] = v
		end
	end
	answer({ action = "accept", content = content })
end

return { elicit = elicit }
