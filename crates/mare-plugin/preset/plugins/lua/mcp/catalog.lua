-- Public discovery/preparation of deferred tool tiers, using the existing connection owner.
return function(D)
	local function catalog(tier)
		tier = tier or "codemode"
		if tier ~= "codemode" and tier ~= "deferred" and tier ~= "direct" then
			error("MCP catalog exposure must be direct, deferred or codemode")
		end
		if D.exposure.resolve_tier(tier) ~= tier then
			return {}
		end
		local out = {}
		for _, name in ipairs(D.config.order) do
			local e = D.servers[name]
			if not D.config.blocked(e) then
				local interested = e.tier == tier
				for _, override in pairs(type(e.srv.tools) == "table" and e.srv.tools or {}) do
					if override == tier then
						interested = true
					end
				end
				if interested and not e.client and not e.cached then
					D.connection.connect(name, true)
				end
				if not D.config.blocked(e) then
					for _, tool in ipairs(e.tools or {}) do
						if D.exposure.tool_tier(e, tool.name) == tier then
							local registered = D.connection.register(e, tool)
							if registered then
								out[#out + 1] = {
									name = registered,
									server = name,
									description = tool.description,
									parameters = tool.inputSchema,
								}
							end
						end
					end
				end
			end
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return out
	end
	return { catalog = catalog }
end
