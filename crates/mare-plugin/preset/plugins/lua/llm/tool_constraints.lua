-- Final, monotone tool declaration constraints. No callback receives the mutable request.
return function(D)
	local entries = {}
	function D.llm.constrain_tools(_, caller, fn)
		assert(type(fn) == "function", "tool constraint must be a function")
		local entry = { fn = fn }
		entries[#entries + 1] = entry
		local function remove()
			entry.fn = nil
			for i, current in ipairs(entries) do
				if current == entry then
					table.remove(entries, i)
					break
				end
			end
		end
		if caller then
			caller:effect(function()
				return remove
			end, "llm tool constraint")
		end
		return remove
	end
	local function allowed(value)
		if value == nil then
			return nil
		end
		assert(type(value) == "table", "tool constraint must return a string list or nil")
		local names, count = {}, 0
		for i, name in pairs(value) do
			assert(
				type(i) == "number" and i > 0 and i % 1 == 0 and type(name) == "string" and name ~= "",
				"invalid tool constraint list"
			)
			count = count + 1
			names[name] = true
		end
		assert(count == #value, "tool constraint must return a dense list")
		return names
	end
	return function(req)
		local tools = {}
		for _, tool in ipairs(req.tools or {}) do
			tools[#tools + 1] = tool
		end
		-- Snapshot registration order. Disposal is effective immediately, including during a callback.
		local snapshot = {}
		for _, entry in ipairs(entries) do
			if entry.fn then
				snapshot[#snapshot + 1] = entry
			end
		end
		for _, entry in ipairs(snapshot) do
			if entry.fn then
				local names = allowed(entry.fn({
					purpose = req.purpose,
					session_id = req.session_id,
					model_ref = req.model and req.model.ref,
				}))
				if names then
					local narrowed = {}
					for _, tool in ipairs(tools) do
						if names[tool.name] then
							narrowed[#narrowed + 1] = tool
						end
					end
					tools = narrowed
				end
			end
		end
		if #tools > 0 and req.model and D.routes and D.routes.wire_for_model then
			local wire, provider, route_cfg = D.routes.wire_for_model(req.model)
			if wire and wire.spec and type(wire.spec.filter_tools) == "function" then
				local prov_cfg = D.api_keys and D.api_keys.provider_cfg and D.api_keys.provider_cfg(req.model.provider)
					or nil
				local warm_ok, warm_url
				if provider and type(provider.warm_url) == "function" then
					warm_ok, warm_url = pcall(provider.warm_url)
				end
				local meta = {
					provider = req.model.provider,
					model = req.model,
					dialect = (route_cfg and route_cfg.dialect) or (prov_cfg and prov_cfg.dialect) or nil,
					base_url = (route_cfg and route_cfg.base_url)
						or (prov_cfg and prov_cfg.base_url)
						or (warm_ok and type(warm_url) == "string" and warm_url)
						or nil,
				}
				local filtered = wire.spec.filter_tools(tools, req, meta)
				if type(filtered) == "table" then
					tools = filtered
				end
			end
		end
		local copy = {}
		for key, value in pairs(req) do
			copy[key] = value
		end
		copy.tools = tools
		-- constrained once: its tool calls are bound to exactly these declarations, so every
		-- attempt of this request sends them as they are (llm/stream.lua)
		copy._tools_constrained = true
		return copy
	end
end
