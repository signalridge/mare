-- Shared plugins recommendations and package operation runner.
local M = {}

function M.show_log(title, r)
	local lines = {}
	for _, l in ipairs(r.log or {}) do
		lines[#lines + 1] = ui.Line({ ui.Span(l) })
	end
	if r.error then
		lines[#lines + 1] = ui.Line({ ui.Span(r.error):hl("MareError") })
	end
	if #lines == 0 then
		lines[1] = ui.Line({ ui.Span("nothing to do"):hl("MareDim") })
	end
	mare.ui.overlay(title, ui.Lines(lines))
end

--- Format a plugin's description and dependencies for the panel and first-open chooser.
function M.detail(e)
	local detail = e.description or ""
	if #(e.required or {}) > 0 then
		detail = detail .. " · installs " .. table.concat(e.required, ", ")
	end
	if #(e.optional or {}) > 0 then
		detail = detail .. " · optional " .. table.concat(e.optional, ", ")
	end
	if #(e.plugins or {}) > 0 then
		detail = detail .. " · needs " .. table.concat(e.plugins, ", ")
	end
	if #(e.needed_by or {}) > 0 then
		detail = detail .. " · needed by " .. table.concat(e.needed_by, ", ")
	end
	if #(e.conflicts or {}) > 0 then
		detail = detail .. " · conflicts " .. table.concat(e.conflicts, ", ")
	end
	if e.source and e.source ~= mare.plugins.default_source then
		detail = detail .. " · from " .. e.source
	end
	return detail
end

function M.id(e)
	return e.source .. "#" .. e.name
end

function M.preview(e)
	local lines = { ui.Line({ ui.Span(e.name):hl("MareTitle") }), ui.Line({ ui.Span(e.description or "") }) }
	local function add(label, value)
		if type(value) == "table" then
			value = table.concat(value, ", ")
		end
		if value and value ~= "" then
			lines[#lines + 1] = ui.Line({ ui.Span(label .. ": "):hl("MareMuted"), ui.Span(value) })
		end
	end
	add("Source", e.source)
	add("Category", e.category)
	add("Scope", e.scope)
	add("Version", e.version)
	add("Platforms", e.platforms)
	for _, kind in ipairs({ "tools", "commands", "hooks", "skills", "prompts", "themes", "services", "flags", "mcp" }) do
		add(kind, (e.provides or {})[kind])
	end
	add("Required packages", e.required)
	add("Optional packages", e.optional)
	add("Plugins", e.plugins)
	add("Needed by", e.needed_by)
	add("Conflicts", e.conflicts)
	return ui.Lines(lines)
end

--- Group all catalog entries by their declared category, with recommended entries first.
function M.discover_items(list, selected)
	local groups, categories = {}, {}
	for _, e in ipairs(list) do
		local category = e.category and e.category ~= "" and e.category or "other"
		if not groups[category] then
			groups[category] = {}
			categories[#categories + 1] = category
		end
		groups[category][#groups[category] + 1] = e
	end
	table.sort(categories)
	local items = {}
	for _, category in ipairs(categories) do
		items[#items + 1] = { label = category, header = true }
		table.sort(groups[category], function(a, b)
			if (a.recommended == true) ~= (b.recommended == true) then
				return a.recommended == true
			end
			if a.name ~= b.name then
				return a.name < b.name
			end
			return a.source < b.source
		end)
		for _, e in ipairs(groups[category]) do
			local id = M.id(e)
			items[#items + 1] = {
				item_id = id,
				label = (e.recommended and "★ " or "") .. (e.enabled and "✓ " or "") .. e.name,
				detail = M.detail(e),
				preview = M.preview(e),
				checked = selected and selected[id] or false,
				plugin = e,
			}
		end
	end
	return items
end

-- A single runner is shared by the panel and onboarding, including cancellation.
function M.runner(ctx)
	local running
	local function cancel()
		if running then
			running:cancel()
			return true
		end
		return false
	end
	ctx:inject({ "ui" }, function(sub)
		sub.ui:handler("interrupt", "plugins", { order = 10 }, cancel)
	end)
	local function call(label, fn)
		if running then
			return { ok = false, log = {}, error = "Another plugin operation is already running" }
		end
		running = mare.task.token()
		mare.ui.working(label)
		local ok, r = pcall(fn, {
			cancel = running,
			on_line = function(line)
				mare.ui.working(label, line)
			end,
		})
		running = nil
		mare.ui.working(nil)
		if not ok then
			r = { ok = false, log = {}, error = tostring(r) }
		end
		return r
	end
	return function(action, name, quiet)
		if action == "fetch_catalog" then
			return call(name or "Fetching the plugins catalog", mare.plugins.fetch_catalog)
		end
		local what = action .. (name and (" " .. name) or "")
		local r = call("plugins " .. what, function(opts)
			if action == "check" then
				local rows = mare.plugins.check(name and { name } or nil, opts)
				local log = {}
				for _, row in ipairs(rows) do
					log[#log + 1] = row.name .. ": " .. row.status .. (row.error and (" · " .. row.error) or "")
				end
				return { ok = true, log = log }
			end
			return mare.plugins.run(action, name, opts)
		end)
		ctx.bus:emit("plugins/completed", { action = action, name = name, status = r.ok and "completed" or "failed" })
		if not quiet then
			M.show_log(("plugins %s — %s"):format(what, r.ok and "done" or "failed"), r)
			if action == "sync" or action == "update" or action == "restore" or action == "clean" then
				ui.notify(
					"Package changes load on /reload (new packages are installed and loaded, removed ones unloaded)"
				)
			end
		end
		return r
	end
end

return M
