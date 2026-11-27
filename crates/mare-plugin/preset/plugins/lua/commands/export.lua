-- /export: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /export. Runs in the commands plugin's env.
return function(ctx, args)
	local agent = ctx:get("agent")
	if not agent then
		return
	end
	ctx:spawn(function()
		local out = { "# mare session", "" }
		for _, m in ipairs(agent:list_messages().data) do
			local text = {}
			for _, b in ipairs(m.content or {}) do
				if b.type == "text" then
					text[#text + 1] = b.text
				elseif b.type == "tool_call" then
					text[#text + 1] = ("`%s(%s)`"):format(b.name, mare.json.encode(b.arguments or {}))
				end
			end
			local body = table.concat(text, "\n\n")
			if m.role == "user" then
				out[#out + 1] = "## User\n\n" .. body
			elseif m.role == "assistant" then
				out[#out + 1] = "## Assistant\n\n" .. body
			elseif m.role == "custom" then
				out[#out + 1] = ("## Custom (%s)\n\n%s"):format(tostring(m.custom_type or "custom"), body)
			else
				out[#out + 1] = "<details><summary>"
					.. (m.tool_name or "tool")
					.. " result</summary>\n\n```\n"
					.. body
					.. "\n```\n</details>"
			end
			out[#out + 1] = ""
		end
		local path = args ~= "" and args or ("mare-session-" .. mare.date("%Y%m%d-%H%M%S") .. ".md")
		local content = mare.text.redact(table.concat(out, "\n"))
		local ok, err = mare.fs.write(path, content, { mode = 384 })
		if ok then
			ui.notify("Exported to " .. path, "success")
		else
			ui.notify("Export failed: " .. tostring(err), "error")
		end
	end)
end
