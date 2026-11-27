-- Plugin discovery, installed packages, and catalog diagnostics.
local M = {}
local extras = require("plugins.extras_list")

local function short(value)
	return value and value:sub(1, 12) or nil
end

local function state(p)
	if p.disabled then
		return "disabled · " .. (p.scope or "user")
	elseif p["local"] then
		return "local directory · " .. (p.scope or "init.lua")
	elseif not p.installed then
		return "missing · " .. (p.scope or "user")
	elseif p.problem then
		return "not loaded: " .. p.problem
	end
	local s = (p.scope or "user") .. (p.commit and (" · " .. short(p.commit)) or "")
	if p.manifest and p.manifest.version then
		s = s .. " · " .. p.manifest.version
	end
	if p.commit and p.locked and p.commit ~= p.locked then
		s = s .. " · lock " .. short(p.locked) .. " (drift)"
	end
	if p.branch then
		s = s .. " · " .. p.branch
	end
	if p.dirty then
		s = s .. " · dirty"
	end
	return s
end

local function installed_items(catalog)
	local items = {
		{ label = "Check updates", detail = "fetch and compare without changing checkouts or locks", action = "check" },
		{ label = "Sync", detail = "install missing packages at their locked revisions", action = "sync" },
		{ label = "Update", detail = "update declared packages", action = "update" },
		{ label = "Restore", detail = "restore locked revisions", action = "restore" },
		{ label = "Clean", detail = "remove unused checkouts and stale lock entries", action = "clean" },
		{ label = "News", detail = "show plugin and mare update notes", action = "news" },
	}
	local count = 0
	local ok, rows = pcall(mare.plugins.list_installed, { manifest = true })
	local installed_sources = {}
	if ok then
		for _, p in ipairs(rows) do
			if p.installed then
				installed_sources[p.name] = true
			end
		end
	end
	for _, e in ipairs(catalog) do
		if e.enabled or installed_sources[e.source] then
			count = count + 1
			items[#items + 1] = {
				item_id = "plugin:" .. extras.id(e),
				label = (e.enabled and "✓ " or "○ ") .. e.name,
				detail = e.scope .. (e.version and (" · " .. e.version) or "") .. " · " .. e.source,
				preview = extras.preview(e),
				action = "plugin",
				plugin = e,
			}
		end
	end
	if not ok then
		items[#items + 1] = { label = "Installed plugins unavailable: " .. tostring(rows), disabled = true }
		return items, count
	end
	for _, p in ipairs(rows) do
		count = count + 1
		local detail = state(p)
		if not p["local"] and p.url ~= p.name then
			detail = detail .. " · " .. p.url
		end
		items[#items + 1] = {
			item_id = "package:" .. p.scope .. ":" .. p.name,
			label = (p.disabled and "○ " or "● ") .. (p.display or p.name),
			detail = detail,
			action = "package",
			pkg = p,
		}
	end
	return items, count
end

local function sources_items(catalog_error)
	local items = {
		{ label = "Refresh catalogs", detail = "fetch all configured sources", action = "fetch" },
		{ label = "Sources", header = true },
		{ label = mare.plugins.default_source .. " (default)", disabled = true },
	}
	local seen = { [mare.plugins.default_source] = true }
	for _, source in ipairs((mare.config.plugins or {}).sources or {}) do
		if not seen[source] then
			seen[source] = true
			items[#items + 1] = { label = source, disabled = true }
		end
	end
	items[#items + 1] = { label = "Failed / Errors", header = true }
	if catalog_error then
		items[#items + 1] = { label = "Catalog: " .. catalog_error, disabled = true }
	end
	local ok, diagnostics = pcall(mare.plugins.diagnostics)
	if ok then
		for _, d in ipairs(diagnostics) do
			items[#items + 1] = { label = d.plugin .. ": " .. d.message, detail = d.path, disabled = true }
		end
	else
		items[#items + 1] = { label = "Diagnostics: " .. tostring(diagnostics), disabled = true }
	end
	local dok, checks = pcall(mare.plugins.doctor)
	if dok then
		for _, check in ipairs(checks) do
			if not check.ok then
				items[#items + 1] = { label = check.name .. ": " .. check.detail, disabled = true }
			end
		end
	end
	return items
end

local function show_news()
	local news = require("plugins.news")
	local lines = {}
	for _, entry in ipairs(news.all_news()) do
		for _, line in ipairs(news.lines(entry.title, entry.body)) do
			lines[#lines + 1] = line
		end
	end
	if #lines == 0 then
		ui.notify("No update notes for this version")
	else
		mare.ui.overlay("News", ui.Lines(lines))
	end
end

local function row_actions(p, is_plugin)
	local items = {}
	local function add(label, action)
		items[#items + 1] = { label = label, action = action }
	end
	if is_plugin then
		if p.managed ~= false then
			if p.enabled then
				add("Disable", "disable")
				add("Remove", "remove")
			else
				add("Enable", "enable")
			end
		end
		add("Check updates", "check")
		add("Update", "update")
		add("Restore", "restore")
	elseif p.disabled then
		if p.installed then
			add("Clean", "clean")
		end
	elseif not p.installed then
		add("Sync", "sync")
	elseif not p["local"] then
		add("Check updates", "check")
		add("Update", "update")
		add("Restore", "restore")
		add("Log", "log")
		add("Diff", "diff")
	end
	return items
end

function M.create(ctx, run)
	run = run or extras.runner(ctx)
	return function(initial_tab)
		local selected, initialized = {}, false
		local tab, filter, selected_index = initial_tab or 1, nil, 1
		while true do
			local ok, catalog = pcall(mare.plugins.catalog)
			local catalog_error = not ok and tostring(catalog) or nil
			if not ok then
				catalog = {}
			end
			if not initialized then
				for _, e in ipairs(catalog) do
					selected[extras.id(e)] = e.enabled
				end
				initialized = true
			end
			local discover = extras.discover_items(catalog, selected)
			for _, item in ipairs(discover) do
				if item.plugin then
					item.label = (selected[item.item_id] and "[x] " or "[ ] ") .. item.label
					item.disabled = item.plugin.managed == false
				end
			end
			if #discover == 0 then
				discover[1] = { label = "Fetch catalogs in Sources", disabled = true }
			end
			local installed, count = installed_items(catalog)
			local sources = sources_items(catalog_error)
			local tabs = {
				{ name = "Discover", items = discover },
				{ name = "Installed (" .. count .. ")", items = installed },
				{ name = "Sources", items = sources },
			}
			tabs[tab].selected = selected_index
			local index, active, key, query = ui.select({
				title = "Plugins",
				tabs = tabs,
				tab = tab,
				filter = filter,
				keys = { { key = " ", label = "toggle" } },
			})
			if not index then
				return
			end
			tab, filter, selected_index = active, query ~= "" and query or nil, index
			local item = tabs[tab].items[index]
			if tab == 1 and item and item.plugin then
				if key == " " then
					local on = not selected[item.item_id]
					if on then
						for _, e in ipairs(catalog) do
							if e.name == item.plugin.name then
								selected[extras.id(e)] = false
							end
						end
					end
					selected[item.item_id] = on
				else
					local changes, enabled = false, {}
					for _, e in ipairs(catalog) do
						if selected[extras.id(e)] then
							enabled[e.name] = true
						end
					end
					for _, e in ipairs(catalog) do
						if e.enabled and not selected[extras.id(e)] and not enabled[e.name] then
							local accepted, err = mare.plugins.set_enabled(e.name, false, e.source)
							if accepted then
								changes = true
								ctx.bus:emit("plugins/plugin", { name = e.name, enabled = false })
							else
								ui.notify(tostring(err), "warning")
							end
						end
					end
					for _, e in ipairs(catalog) do
						if selected[extras.id(e)] and not e.enabled then
							local accepted, err = mare.plugins.set_enabled(e.name, true, e.source)
							if accepted then
								changes = true
								ctx.bus:emit("plugins/plugin", { name = e.name, enabled = true })
							else
								ui.notify(tostring(err), "warning")
							end
						end
					end
					if changes then
						run("sync")
					end
					return
				end
			elseif item and item.action == "fetch" then
				local result = run("fetch_catalog", "Fetching the plugins catalog")
				if not result.ok then
					extras.show_log("Plugins catalog — failed", result)
				end
				initialized, filter = false, nil
			elseif item and item.action == "news" then
				show_news()
			elseif item and (item.action == "package" or item.action == "plugin") then
				local is_plugin = item.action == "plugin"
				local p = is_plugin and item.plugin or item.pkg
				local actions = row_actions(p, is_plugin)
				if #actions > 0 then
					local choice = ui.select({ title = p.name, items = actions })
					local action = choice and actions[choice] and actions[choice].action
					if action == "enable" or action == "disable" or action == "remove" then
						local on = action == "enable"
						local accepted, err = mare.plugins.set_enabled(p.name, on, p.source)
						if not accepted then
							ui.notify(tostring(err), "warning")
						else
							ctx.bus:emit("plugins/plugin", { name = p.name, enabled = on })
							if on then
								run("sync")
							elseif action == "remove" then
								run("clean")
							end
						end
						initialized = false
					elseif action then
						run(action, is_plugin and (action == "restore" and p.source or p.name) or p.name)
					end
				end
			elseif item and item.action == "clean" then
				local preview = mare.plugins.run("clean", nil, { dry_run = true })
				if not preview.ok then
					ui.notify("Cannot preview clean: " .. tostring(preview.error), "warning")
				else
					local candidates = {}
					for _, line in ipairs(preview.log) do
						if line:match("^would ") then
							candidates[#candidates + 1] = line
						end
					end
					if #candidates == 0 then
						ui.notify("Nothing unused to clean")
					elseif
						mare.ui.confirm(
							"Clean unused checkouts and lock entries?\n" .. table.concat(candidates, "\n"),
							false
						)
					then
						run("clean")
					end
				end
			elseif item and item.action then
				run(item.action)
			end
		end
	end
end

return M
