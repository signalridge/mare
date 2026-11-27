return function(mare, luatype, closest)
	local UI_STACK_REGIONS = {
		live = { hideable = false, max_height = false },
		working = { hideable = true, max_height = false },
		["above-editor"] = { hideable = true, max_height = true },
		completion = { hideable = false, max_height = true, min_height = 3 },
		editor = { hideable = false, max_height = true },
		["below-editor"] = { hideable = true, max_height = true },
		status = { hideable = false, max_height = true },
	}
	local UI_OVERLAY_REGIONS = { ["which-key"] = true, notices = true, modal = true }
	local MAX_REGION_NAME_LEN = 64

	local function parse_ui_region_name(name)
		if type(name) ~= "string" then
			error(("ui.layout region name must be a string, got %s"):format(type(name)), 0)
		end
		if #name > MAX_REGION_NAME_LEN then
			error(("unknown layout region (name too long: %d bytes, max %d)"):format(#name, MAX_REGION_NAME_LEN), 0)
		end
		if UI_STACK_REGIONS[name] then
			return name
		end
		if UI_OVERLAY_REGIONS[name] then
			error(("`%s` is an overlay region and cannot be configured in ui.layout"):format(name), 0)
		end
		local near = closest(name, UI_STACK_REGIONS)
		error(("unknown layout region `%s`%s"):format(name, near and (" — did you mean `" .. near .. "`?") or ""), 0)
	end

	local function validate_ui_config(value)
		if value == nil then
			return
		end
		if type(value) ~= "table" then
			error(("option `ui` expects a table, got %s"):format(luatype(value)), 0)
		end
		for k in pairs(value) do
			if k ~= "layout" and k ~= "working" and k ~= "tool_call" and k ~= "stopping" then
				error(("unknown key `%s` in `ui`"):format(tostring(k)), 0)
			end
		end
		for _, sub in ipairs({ "tool_call", "stopping" }) do
			if value[sub] ~= nil then
				if type(value[sub]) ~= "table" then
					error(("option `ui.%s` expects a table, got %s"):format(sub, luatype(value[sub])), 0)
				end
				for sk in pairs(value[sub]) do
					if sk ~= "spinner" then
						error(("unknown option `ui.%s.%s`"):format(sub, tostring(sk)), 0)
					end
				end
			end
		end
		local layout = value.layout
		if layout == nil then
			return
		end
		if type(layout) ~= "table" then
			error(("ui.layout must be a table or nil, got %s"):format(luatype(layout)), 0)
		end
		local n = #layout
		local order
		if n > 0 then
			order = {}
			for i = 1, n do
				order[i] = layout[i]
			end
		end
		local regions
		for k, v in pairs(layout) do
			if type(k) == "number" and k >= 1 and k <= n and math.floor(k) == k then
			-- array element already collected
			elseif k == "order" then
				if order ~= nil then
					error("ui.layout cannot specify both array items and `order`", 0)
				end
				if type(v) ~= "table" then
					error(("ui.layout.order must be a list of region names, got %s"):format(luatype(v)), 0)
				end
				order = {}
				for i = 1, #v do
					order[i] = v[i]
				end
				for ok in pairs(v) do
					if type(ok) ~= "number" or ok < 1 or ok > #v or math.floor(ok) ~= ok then
						error(("unexpected key `%s` in ui.layout.order"):format(tostring(ok)), 0)
					end
				end
			elseif k == "regions" then
				if type(v) ~= "table" then
					error(("ui.layout.regions must be a table, got %s"):format(luatype(v)), 0)
				end
				regions = v
			else
				error(("unknown key `%s` in ui.layout"):format(tostring(k)), 0)
			end
		end
		if order ~= nil then
			local seen = {}
			local editor_pos, comp_pos
			for i, name in ipairs(order) do
				parse_ui_region_name(name)
				if seen[name] then
					error(("duplicate region `%s` in ui.layout"):format(name), 0)
				end
				seen[name] = true
				if name == "editor" then
					editor_pos = i
				elseif name == "completion" then
					comp_pos = i
				end
			end
			if not editor_pos then
				error("ui.layout must include `editor`", 0)
			end
			if not seen["status"] then
				error("ui.layout must include `status`", 0)
			end
			if comp_pos and math.abs(comp_pos - editor_pos) ~= 1 then
				error("`completion` must be adjacent to `editor` in ui.layout", 0)
			end
		end
		if regions ~= nil then
			for rname, rcfg in pairs(regions) do
				parse_ui_region_name(rname)
				if type(rcfg) ~= "table" then
					error(("ui.layout.regions.%s must be a table, got %s"):format(rname, luatype(rcfg)), 0)
				end
				local spec = UI_STACK_REGIONS[rname]
				for rk, rv in pairs(rcfg) do
					if rk == "hidden" then
						if type(rv) ~= "boolean" then
							error(("ui.layout.regions.%s.hidden must be a boolean"):format(rname), 0)
						end
						if rv and not spec.hideable then
							error(("region `%s` cannot be hidden"):format(rname), 0)
						end
					elseif rk == "max_height" then
						if type(rv) ~= "number" or math.floor(rv) ~= rv or rv < 0 then
							error(("ui.layout.regions.%s.max_height must be a positive integer"):format(rname), 0)
						end
						if not spec.max_height then
							error(("region `%s` does not support max_height"):format(rname), 0)
						end
						local min_h = spec.min_height or 1
						if rv < min_h then
							error(("max_height for region `%s` must be at least %d"):format(rname, min_h), 0)
						end
					else
						error(("unknown key `%s` in ui.layout.regions.%s"):format(tostring(rk), rname), 0)
					end
				end
			end
		end
	end
	return validate_ui_config
end
