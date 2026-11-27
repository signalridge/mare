-- The key list shared by /hotkeys and /help: the keymap's bindings (mare.keymap.list, each with
-- its description), any unbound essential actions, and the input-line prefixes.
local M = {}

M.PREFIXES = {
	{ "/", "command (/help lists them)" },
	{ "!", "on an empty line: shell mode (!! keeps the output out of the context)" },
	{ "@", "mention a file" },
}

local NAMES = {
	cr = "Enter",
	enter = "Enter",
	["return"] = "Enter",
	esc = "Esc",
	escape = "Esc",
	tab = "Tab",
	bs = "Backspace",
	backspace = "Backspace",
	del = "Delete",
	delete = "Delete",
	home = "Home",
	["end"] = "End",
	space = "Space",
	up = "↑",
	down = "↓",
	left = "←",
	right = "→",
	pageup = "PageUp",
	pagedown = "PageDown",
}
local MODS = { c = "Ctrl", ctrl = "Ctrl", m = "Alt", a = "Alt", alt = "Alt", s = "Shift", shift = "Shift" }

--- `<C-o>` / `ctrl+o` / `<M-CR>` / `alt+enter` → `Ctrl+O` / `Alt+Enter`
function M.pretty(key)
	local parts = {}
	local inner = string.match(key, "^<(.+)>$")
	if inner then
		local base = inner
		while true do
			local m, rest = string.match(base, "^([CMSAcmsa])%-(.+)$")
			if not m then
				break
			end
			parts[#parts + 1] = MODS[string.lower(m)]
			base = rest
		end
		parts[#parts + 1] = base
	else
		for p in string.gmatch(key, "[^+]+") do
			parts[#parts + 1] = p
		end
		if string.sub(key, -1) == "+" then
			parts[#parts + 1] = "+"
		end
		for i = 1, #parts - 1 do
			parts[i] = MODS[string.lower(parts[i])] or parts[i]
		end
	end
	local base = parts[#parts]
	parts[#parts] = NAMES[string.lower(base)] or (#base == 1 and string.upper(base) or base)
	return table.concat(parts, "+")
end

local ESSENTIAL = {
	{ "select", "select.accept" },
	{ "select", "select.back" },
	{ "confirm", "confirm.accept" },
	{ "confirm", "confirm.cancel" },
	{ "help", "help.accept" },
	{ "help", "help.cancel" },
}

--- essential accept/cancel modal actions that currently have no key bound
function M.unbound_essential()
	local out = {}
	for _, pair in ipairs(ESSENTIAL) do
		local keys = mare.keymap.hint(pair[2], pair[1])
		if #keys == 0 then
			out[#out + 1] = { context = pair[1], action = pair[2] }
		end
	end
	return out
end

--- groups: { {title, entries = { {key, desc} }} }
function M.groups()
	local missing = {}
	for _, a in ipairs(mare.keymap.actions()) do
		if a.essential then
			for _, ctx_name in ipairs(a.contexts or { "editor" }) do
				if #mare.keymap.hint(a.name, ctx_name) == 0 then
					missing[#missing + 1] = { a.name, (a.desc or "") .. " (" .. ctx_name .. ")" }
				end
			end
		end
	end
	local bound = { { "Ctrl+C", "interrupt / clear input · twice to exit (reserved)" } }
	local list = mare.keymap.list()
	local is_core = {}
	for _, a in ipairs(mare.keymap.actions()) do
		if a.core and not a.declines then
			is_core[a.name] = true
		end
	end
	local function when_mask(when)
		local all, none = 0, 0
		local bits = { editor_empty = 1, completion_open = 2, paste_burst = 4, working = 8 }
		local items = type(when) == "string" and { when } or (type(when) == "table" and when or {})
		for _, w in ipairs(items) do
			if type(w) == "string" then
				local neg, bare = w:match("^(!?)(.+)$")
				local b = bits[bare]
				if b then
					if neg == "!" then
						none = none | b
					else
						all = all | b
					end
				end
			end
		end
		local mask = 0
		for s = 0, 15 do
			if (s & all) == all and (s & none) == 0 then
				mask = mask | (1 << s)
			end
		end
		return mask
	end
	local blocked = {}
	local hinted_editor = {}
	local function is_hinted_in_editor(action, key)
		if type(action) ~= "string" or action:match("^fn:") then
			return true
		end
		local set_keys = hinted_editor[action]
		if not set_keys then
			set_keys = {}
			for _, hk in ipairs(mare.keymap.hint(action, "editor")) do
				set_keys[hk] = true
			end
			hinted_editor[action] = set_keys
		end
		return set_keys[key] == true
	end
	local printable_done = false
	for _, ctx_name in ipairs({ "editor", "text", "global" }) do
		for _, k in ipairs(list) do
			if k.context == ctx_name then
				local is_print = #k.key == 1 and not k.key:find("%s")
				local ok_global = ctx_name ~= "global" or is_hinted_in_editor(k.action, k.key)
				if ok_global and not (printable_done and is_print) then
					local cov = blocked[k.key] or 0
					local sm = when_mask(k.when)
					if (sm & (~cov & 0xFFFF)) ~= 0 then
						if k.context ~= "text" and k.action ~= "editor.submit" then
							bound[#bound + 1] = { M.pretty(k.key), k.desc or "" }
						end
						if is_core[k.action] then
							blocked[k.key] = cov | sm
						end
					end
				end
			end
		end
		if ctx_name == "text" then
			printable_done = true
		end
	end
	local by_action, order = {}, {}
	for _, k in ipairs(list) do
		if k.context == "text" or k.action == "editor.submit" then
			local g = by_action[k.action]
			if not g then
				g = { keys = {}, desc = k.desc or k.action }
				by_action[k.action] = g
				order[#order + 1] = k.action
			end
			g.keys[#g.keys + 1] = M.pretty(k.key)
		end
	end
	table.sort(order, function(a, b)
		if a == "editor.submit" then
			return true
		end
		if b == "editor.submit" then
			return false
		end
		return a < b
	end)
	local editor = {}
	for _, act in ipairs(order) do
		local g = by_action[act]
		editor[#editor + 1] = { table.concat(g.keys, " / "), g.desc }
	end
	local out = {}
	if #missing > 0 then
		out[#out + 1] = { title = "No key", entries = missing }
	end
	out[#out + 1] = { title = "Shortcuts", entries = bound }
	out[#out + 1] = { title = "Input editor", entries = editor }
	out[#out + 1] = { title = "Input prefixes", entries = M.PREFIXES }
	local unbound = M.unbound_essential()
	if #unbound > 0 then
		local missing_modal = {}
		for _, u in ipairs(unbound) do
			missing_modal[#missing_modal + 1] = {
				u.context .. ": " .. u.action,
				"no key bound (Ctrl+C still cancels modals)",
			}
		end
		out[#out + 1] = { title = "Unbound accept/cancel", entries = missing_modal }
	end
	return out
end

--- the groups as overlay lines (key column `width` cells)
function M.lines(width)
	local fmt = "%-" .. tostring(width or 18) .. "s"
	local lines = {}
	for gi, g in ipairs(M.groups()) do
		if gi > 1 then
			lines[#lines + 1] = ui.Gap()
		end
		lines[#lines + 1] = ui.Line({ ui.Span(g.title):hl("MareHeading") })
		for _, e in ipairs(g.entries) do
			lines[#lines + 1] = ui.Line({ ui.Span(fmt:format(e[1])):hl("MareAccent"), ui.Span(e[2]):hl("MareMuted") })
		end
	end
	return lines
end

return M
