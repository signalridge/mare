-- mcp.discovery: which tiers the servers use, and the tools-service search source that connects
-- uncached deferred servers on the first tool_search;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx, servers = D.ctx, D.servers
	local enabled = D.approvals.enabled
	local order, blocked = D.config.order, D.config.blocked
	local resolve_tier = D.exposure.resolve_tier
	local connect = D.connection.connect

	local function has_tier(tier)
		for _, e in pairs(servers) do
			if not e.rejected and enabled(e) and e.tier ~= "hidden" then
				if resolve_tier(e.tier) == tier then
					return true
				end
				for _, t in pairs(type(e.srv.tools) == "table" and e.srv.tools or {}) do
					if resolve_tier(t) == tier then
						return true
					end
				end
			end
		end
		return false
	end

	local function deferred_prefixes()
		local out = {}
		for _, n in ipairs(order) do
			local e = servers[n]
			if e and not e.rejected and enabled(e) and e.tier ~= "hidden" and not blocked(e) then
				local has_def = resolve_tier(e.tier) == "deferred"
				if not has_def then
					for _, t in pairs(type(e.srv.tools) == "table" and e.srv.tools or {}) do
						if resolve_tier(t) == "deferred" then
							has_def = true
							break
						end
					end
				end
				if has_def then
					out[#out + 1] = "mcp__" .. D.sanitize(n) .. "__"
				end
			end
		end
		return out
	end

	if has_tier("deferred") and require("mare_lib").callable(ctx.tools.register_search_source) then
		ctx.tools:register_search_source({
			prefixes = deferred_prefixes,
			prepare = function()
				-- Uncached deferred servers connect on the first search.
				for _, n in ipairs(order) do
					local e = servers[n]
					if resolve_tier(e.tier) == "deferred" and not e.client and not e.cached and not blocked(e) then
						if connect(n, true) then
							e.cached = true
						end
					end
				end
			end,
		})
	end

	return { has_tier = has_tier }
end
