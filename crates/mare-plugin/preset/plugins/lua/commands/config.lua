-- /config: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /config. Runs in the commands plugin's env.
--- the allowed values an option's description lists ("…: always|unfocused|never",
--- "auto (OSC 11 …)|dark|light"), or nil
local function M_choices(def)
	if def.type ~= "string" or type(def.desc) ~= "string" then
		return nil
	end
	local d = string.gsub(def.desc, "%b()", "")
	d = string.gsub(d, "%s*|%s*", "|")
	local run = string.match(d, "([%w%-]+|[%w%-|]+)")
	if not run then
		return nil
	end
	local out = {}
	for v in string.gmatch(run, "[^|]+") do
		out[#out + 1] = v
	end
	return #out > 1 and out or nil
end

--- text → a value of the option's type | nil, why
local function M_parse(def, text)
	text = string.match(text, "^%s*(.-)%s*$")
	if def.type == "number" then
		local n = tonumber(text)
		if not n then
			return nil, ("%q is not a number"):format(text)
		end
		return n
	elseif def.type == "boolean" then
		local b = ({
			["true"] = true,
			yes = true,
			on = true,
			["1"] = true,
			["false"] = false,
			no = false,
			off = false,
			["0"] = false,
		})[string.lower(text)]
		if b == nil then
			return nil, ("%q is not true or false"):format(text)
		end
		return b
	end
	local choices = M_choices(def)
	if choices and not table.find(choices, text) then
		return nil, ("%q is not one of %s"):format(text, table.concat(choices, ", "))
	end
	return text
end

return function(ctx, args)
	local C = mare.config or {}
	local sources = mare.config_sources()
	-- Redact via the shared config helper before encoding, truncating, filtering, or building a preview.
	local function show(path, v, def, truncate)
		local ok, safe = pcall(mare.config_redact, path, v)
		if not ok then
			return "[REDACTED]"
		end
		if safe == nil and def and def.sensitive then
			safe = "[REDACTED]"
		end
		if type(safe) == "table" then
			ok, safe = pcall(mare.json.encode, safe)
			if not ok then
				return "[unavailable]"
			end
		else
			safe = tostring(safe)
		end
		return truncate and mare.text.trunc_width(safe, 48) or safe
	end
	local function src(key, s)
		s = s or sources[key]
		if not s then
			return "default"
		end
		return s.where and (s.layer .. "  " .. s.where) or s.layer
	end
	---@type mare.Node[]
	local lines = {
		ui.Line({ ui.Span(("%-24s %-30s %s"):format("option", "value", "source")):hl("MareDim") }),
	}
	local names = {}
	for k in pairs(mare.options or {}) do
		names[#names + 1] = k
	end
	table.sort(names)
	local filter = args ~= "" and args or nil
	for _, k in ipairs(names) do
		if not filter or string.find(k, filter, 1, true) then
			local v = C[k]
			if v == nil then
				v = mare.options[k].default
			end
			lines[#lines + 1] = ui.Line({
				ui.Span(("%-24s "):format(k)):hl("MareAccent"),
				ui.Span(("%-30s "):format(show(k, v, mare.options[k], true))),
				ui.Span(src(k)):hl("MareMuted"),
			})
		end
	end
	-- plugin opts: every declared leaf and anything else set
	local pkeys, pseen = {}, {}
	local function padd(k)
		if not pseen[k] then
			pseen[k] = true
			pkeys[#pkeys + 1] = k
		end
	end
	for _, k in ipairs(mare.config_declared and mare.config_declared() or {}) do
		padd(k)
	end
	for k in pairs(sources) do
		if string.find(k, "^plugins%.") then
			padd(k)
		end
	end
	table.sort(pkeys)
	-- effective value (or the declared default) of a plugin leaf, from the one describer
	local described = mare.config_describe and mare.config_describe(pkeys) or {}
	local function pvalue(k)
		local d = described[k]
		if d == nil then
			return nil
		end
		if d.effective ~= nil then
			return d.effective
		end
		return d.default
	end
	if #pkeys > 0 then
		lines[#lines + 1] = ui.Gap()
		lines[#lines + 1] = ui.Line({ ui.Span("plugin opts"):hl("MareDim") })
		for _, k in ipairs(pkeys) do
			if not filter or string.find(k, filter, 1, true) then
				local v = pvalue(k)
				lines[#lines + 1] = ui.Line({
					ui.Span(("%-24s "):format(k)):hl("MareAccent"),
					ui.Span(("%-30s "):format(show(k, v, nil, true))),
					ui.Span(src(k, described[k] and described[k].source)):hl("MareMuted"),
				})
			end
		end
	end
	local warns = mare.config_warnings()
	if #warns > 0 then
		lines[#lines + 1] = ui.Gap()
		for _, w in ipairs(warns) do
			lines[#lines + 1] = ui.Line({ ui.Span("! " .. mare.text.redact(w)):hl("MareWarning") })
		end
	end
	lines[#lines + 1] = ui.Gap()
	lines[#lines + 1] = ui.Line({
		ui.Span("layers: default < state.json < user init.lua < project < env MARE_* < cli"):hl("MareHint"),
	})
	if filter or not (ui.connected and ui.connected()) then
		ui.overlay("Configuration", ui.Lines(lines))
		return
	end
	-- multi-pane overlay: Options │ Plugin opts │ Warnings, preview on the
	-- right; Enter edits a string / number / boolean option for this session
	local function preview(k, v, def, extra)
		local meta = function(key, val)
			return ui.Paragraph(ui.Line({ ui.Span(("%-8s"):format(key)):hl("MareDim"), ui.Span(val) }))
		end
		local pv = {
			ui.Line({ ui.Span(k):hl("MareTitle") }),
			ui.Gap(1),
			meta("value", show(k, v, def, false)),
			meta("source", src(k, described[k] and described[k].source)),
		}
		if def and def.default ~= nil then
			pv[#pv + 1] = meta("default", show(k, def.default, def, false))
		end
		if def and def.type then
			local t = type(def.type) == "table" and table.concat(def.type, " | ") or def.type
			pv[#pv + 1] = meta("type", t)
		end
		if def and def.desc then
			pv[#pv + 1] = ui.Gap(1)
			pv[#pv + 1] = ui.Paragraph(ui.Line({ ui.Span(def.desc):hl("MareMuted") }))
		end
		if extra then
			pv[#pv + 1] = ui.Gap(1)
			pv[#pv + 1] = ui.Line({ ui.Span(extra):hl("MareHint") })
		end
		return ui.Lines(pv)
	end
	local opt_items = {}
	for _, k in ipairs(names) do
		local v = C[k]
		local def = mare.options[k]
		if v == nil then
			v = def.default
		end
		local editable = not (def.sensitive or def.invocation_only or mare.config_is_sensitive(k))
			and (def.type == "string" or def.type == "number" or def.type == "boolean")
		opt_items[#opt_items + 1] = {
			label = ("%-14s"):format(k),
			detail = show(k, v, def, true),
			preview = preview(k, v, def, editable and "Enter: change (saved to state.json)" or nil),
		}
	end
	local plug_items = {}
	for _, k in ipairs(pkeys) do
		local v = pvalue(k)
		local d = described[k] or {}
		local def = { default = d.default, type = d.type, desc = d.description, sensitive = d.sensitive }
		plug_items[#plug_items + 1] = { label = k, detail = show(k, v, def, true), preview = preview(k, v, def) }
	end
	local warn_items = {}
	for _, w in ipairs(warns) do
		warn_items[#warn_items + 1] = { label = mare.text.redact(w) }
	end
	local tabs = { { name = "Options", items = opt_items } }
	if #plug_items > 0 then
		tabs[#tabs + 1] = { name = "Plugin opts", items = plug_items }
	end
	if #warn_items > 0 then
		tabs[#tabs + 1] = { name = "Warnings (" .. #warn_items .. ")", items = warn_items }
	end
	local proj = mare.project
	if type(proj) == "table" and (proj.root or #(proj.ignored or {}) > 0 or #(proj.rejected or {}) > 0) then
		local pitems = {}
		local detail = proj.root or "none"
		if proj.root and proj.start and proj.root ~= proj.start then
			detail = detail .. " (started in " .. proj.start .. ")"
		elseif not proj.root and proj.start then
			detail = "none (started in " .. proj.start .. ")"
		end
		pitems[#pitems + 1] = { label = "resource root", detail = detail }
		for _, p in ipairs(proj.ignored or {}) do
			pitems[#pitems + 1] = {
				label = "ignored",
				detail = p,
				preview = "a nearer directory with .mare/ or .mcp.json is the resource root; only one layer is read",
			}
		end
		for _, p in ipairs(proj.rejected or {}) do
			pitems[#pitems + 1] = {
				label = "ignored symlink",
				detail = p,
				preview = "points outside the resource root",
			}
		end
		tabs[#tabs + 1] = { name = "Project", items = pitems }
	end
	local i, tab = ui.select({ title = "Configuration", tabs = tabs })
	if not i or tab ~= 1 then
		return
	end
	local k = names[i]
	local def = mare.options[k] or {}
	if
		def.sensitive
		or def.invocation_only
		or mare.config_is_sensitive(k)
		or (def.type ~= "string" and def.type ~= "number" and def.type ~= "boolean")
	then
		return
	end
	local cur = C[k]
	if cur == nil then
		cur = def.default
	end
	local choices = M_choices(def)
	local val
	if def.type == "boolean" then
		local j = ui.select({ title = k, items = { "true", "false" }, selected = cur == false and 2 or 1 })
		if not j then
			return
		end
		val = j == 1
	elseif choices then
		local at = table.find(choices, cur) or 1
		local j = ui.select({ title = k .. " — " .. (def.desc or ""), items = choices, selected = at })
		if not j then
			return
		end
		val = choices[j]
	else
		local text =
			ui.input({ title = k .. " (now: " .. show(k, cur, def, false) .. ")", placeholder = def.desc or "" })
		if text == nil or text == "" then
			return
		end
		local err
		val, err = M_parse(def, text)
		if val == nil then
			ui.notify(("%s: %s — not changed"):format(k, err), "warning")
			return
		end
	end
	-- applied now (mare.o checks the type), then saved to state.json so it survives a restart;
	-- a value set in init.lua / the project / env / --set still wins there, and that is said
	local ok, err = pcall(function()
		mare.o[k] = val
	end)
	if not ok then
		ui.notify(("%s: %s — not changed"):format(k, tostring(err)), "warning")
		return
	end
	require("mare_lib").save_state(ctx, k, val, true)
	local s = sources[k]
	local layer = s and s.layer
	if layer and layer ~= "default" and layer ~= "state" and layer ~= "state.json" then
		ui.notify(
			("%s = %s (saved; %s sets it too and wins on the next start%s)"):format(
				k,
				show(k, val, def, false),
				layer,
				s.where and (": " .. s.where) or ""
			),
			"warning"
		)
	else
		ui.notify(("%s = %s (saved)"):format(k, show(k, val, def, false)), "info")
	end
end
