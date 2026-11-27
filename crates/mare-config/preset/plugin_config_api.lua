local mare, C, apis, core = ...
local cfg = core.config
local state = cfg.state
for _, api in ipairs(apis) do
	for path, fn in pairs(api) do
		local t, last = mare, nil
		local parts = {}
		for seg in string.gmatch(path, "[^%.]+") do
			parts[#parts + 1] = seg
		end
		for i = 1, #parts - 1 do
			if not t[parts[i]] then
				if state.api_undo then
					state.api_undo[#state.api_undo + 1] = { target = t, key = parts[i] }
				end
				t[parts[i]] = {}
			end
			t = t[parts[i]]
		end
		last = parts[#parts]
		if state.api_undo then
			state.api_undo[#state.api_undo + 1] = { target = t, key = last, old = t[last] }
		end
		t[last] = function(...)
			if cfg.state.stage and not cfg.in_config() then
				error("config update unavailable during config reload; retry afterwards", 2)
			end
			return fn(C, ...)
		end
	end
end
