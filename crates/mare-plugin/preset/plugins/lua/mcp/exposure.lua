-- mcp.exposure: tool names (hash suffix), per-tool tiers, enabled/disabled filters, eager;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local owned, TIERS, MAX_NAME, glob_match, sanitize = D.owned, D.TIERS, D.MAX_NAME, D.glob_match, D.sanitize

	-- ---- tool names ---------------------------------------------------------------

	local function hashed(base, server, tool)
		local h = mare.crypto.sha256(server .. "/" .. tool):sub(1, 8)
		return base:sub(1, MAX_NAME - 9) .. "_" .. h
	end
	--- tool name → registered name for one server's list: a short hash suffix when longer
	--- than 64, when two of its tools sanitize alike, or when the name is taken by another
	--- server or a built-in
	local function name_tools(e, tools)
		local base, count = {}, {}
		for _, t in ipairs(tools) do
			local s = "mcp__" .. sanitize(e.name) .. "__" .. sanitize(t.name)
			base[t.name] = s
			count[s] = (count[s] or 0) + 1
		end
		local names = {}
		for _, t in ipairs(tools) do
			local s = base[t.name]
			local taken = (owned[s] and owned[s] ~= e.name) or (not owned[s] and ctx.tools:get(s) ~= nil)
			if #s > MAX_NAME or count[s] > 1 or taken then
				s = hashed(s, e.name, t.name)
			end
			names[t.name] = s
		end
		return names
	end

	local function requested_tier(e, tool)
		local map = type(e.srv.tools) == "table" and e.srv.tools or {}
		local t = map[tool]
		if not TIERS[t or ""] then
			for pat, tier in pairs(map) do
				if type(pat) == "string" and pat:find("*", 1, true) and glob_match(pat, tool) and TIERS[tier] then
					t = tier
					break
				end
			end
		end
		return TIERS[t or ""] and t or e.tier
	end
	local function resolve_tier(tier)
		if tier == "codemode" and not ctx.tools:executor(tier) then
			return "deferred"
		end
		return tier
	end
	local function tool_tier(e, tool)
		return resolve_tier(requested_tier(e, tool))
	end
	--- a per-tool `direct` override on a server of another tier: those tools are declared from
	--- the start (from the cached list, or by connecting in the background)
	local function has_direct_tools(e)
		for _, t in pairs(type(e.srv.tools) == "table" and e.srv.tools or {}) do
			if t == "direct" then
				return true
			end
		end
		return false
	end
	--- connects at startup (in the background; only `required` ones are waited for)
	local function eager(e)
		return e.tier == "direct" or e.srv.required == true or (has_direct_tools(e) and not e.cached)
	end
	--- enabled_tools / disabled_tools (`*` patterns:205:206)
	local function tool_allowed(e, tool)
		local function any(list)
			for _, p in ipairs(type(list) == "table" and list or {}) do
				if type(p) == "string" and glob_match(p, tool) then
					return true
				end
			end
			return false
		end
		if type(e.srv.enabled_tools) == "table" and not any(e.srv.enabled_tools) then
			return false
		end
		return not any(e.srv.disabled_tools)
	end
	local function filter_tools(e, tools)
		local out = {}
		for _, t in ipairs(type(tools) == "table" and tools or {}) do
			if type(t) == "table" and type(t.name) == "string" and tool_allowed(e, t.name) then
				out[#out + 1] = t
			end
		end
		return out
	end

	return {
		name_tools = name_tools,
		tool_tier = tool_tier,
		requested_tier = requested_tier,
		resolve_tier = resolve_tier,
		has_direct_tools = has_direct_tools,
		eager = eager,
		filter_tools = filter_tools,
	}
end
