-- First-open plugins selection, using the same recommendations and sync as /plugins.
local M = {}
local extras_list = require("plugins.extras_list")
local function record(result, tries)
	mare.state.update(function(state)
		state.onboarding = state.onboarding or {}
		state.onboarding.plugins = { v = 1, result = result, tries = tries, at = os.time() }
	end)
end

--- Whether the first-open offer is due, the one check the offer and `mare doctor` share:
--- "due" (never answered, or the catalog was unavailable fewer than three times), "chosen" (an
--- plugins.json exists: plugins were chosen some other way), "answered", or nil when the state
--- cannot be read. The second result is the recorded step.
local function offer_status()
	local state = mare.state.read()
	if not state then
		return nil
	end
	local step = state.onboarding and state.onboarding.plugins
	if step and (step.result ~= "unavailable" or (step.tries or 0) >= 3) then
		return "answered", step
	end
	if mare.fs.exists(mare.paths.config .. mare.fs.sep .. "plugins.json") then
		return "chosen", step
	end
	return "due", step
end

--- The offer is still due (doctor's "plugins: not chosen yet").
function M.due()
	return offer_status() == "due"
end

local function last_line(value)
	local last
	for line in tostring(value or ""):gmatch("[^\r\n]+") do
		if line:match("%S") then
			last = line
		end
	end
	return last
end

local function fetch_error(r)
	local ok, checks = pcall(mare.plugins.doctor)
	if ok then
		for _, check in ipairs(checks) do
			if check.name == "git" and not check.ok and tostring(check.detail):match("^not usable") then
				return "git is required"
			end
		end
	end
	local line = last_line((r.log or {})[#(r.log or {})]) or last_line(r.error) or "unknown error"
	return line:match(" failed: (.+)$") or line
end

local function print_summary(summary, installed, log)
	local lines = {
		ui.Line({ ui.Span("Plugins selection"):hl("MareTitle") }),
		ui.Line({ ui.Span("Enabled: " .. (#summary.enabled > 0 and table.concat(summary.enabled, ", ") or "none")) }),
		ui.Line({ ui.Span("Installed: " .. (#installed > 0 and table.concat(installed, ", ") or "none")) }),
		ui.Line({ ui.Span("Failed: " .. (#summary.failed > 0 and table.concat(summary.failed, ", ") or "none")) }),
	}
	for _, refused in ipairs(summary.refused) do
		lines[#lines + 1] = ui.Line({ ui.Span(refused.name .. ": " .. refused.reason) })
	end
	if #summary.failed > 0 then
		local tail = last_line(log)
		if tail then
			lines[#lines + 1] = ui.Line({ ui.Span(tail) })
		end
	end
	lines[#lines + 1] = ui.Line({ ui.Span("/plugins → Discover changes it later") })
	mare.ui.print(ui.Lines(lines))
end

--- Expand selected names over their catalog plugin dependencies.
local function expand_extras(names, by_name)
	local out, seen, queue = {}, {}, {}
	for _, name in ipairs(names) do
		queue[#queue + 1] = name
	end
	local i = 1
	while i <= #queue do
		local name = queue[i]
		i = i + 1
		if not seen[name] then
			seen[name] = true
			local e = by_name[name]
			if e then
				out[#out + 1] = e
				for _, dep in ipairs(e.plugins or {}) do
					if not seen[dep] then
						queue[#queue + 1] = dep
					end
				end
			end
		end
	end
	return out
end

--- The packages the enabled plugins (and their plugin dependencies) need (only these are reported
--- as installed or failed: other packages init.lua declares are not this selection's).
local function wanted_packages(enabled, by_name)
	local wanted = {}
	for _, e in ipairs(expand_extras(enabled, by_name)) do
		for _, pkg_name in ipairs(e.required or {}) do
			wanted[pkg_name] = true
		end
	end
	return wanted
end

local function sync_summary(summary, sync, wanted)
	local installed = {}
	if not sync then
		return installed, ""
	end
	local log = table.concat(sync.log or {}, "\n") .. "\n" .. (sync.error or "")
	local status_ok, status = pcall(mare.plugins.list_installed)
	if status_ok then
		for _, row in ipairs(status) do
			if wanted[row.name] or wanted[row.display] then
				if row.installed then
					installed[#installed + 1] = row.name
				elseif not row.disabled then
					summary.failed[#summary.failed + 1] = row.name
				end
			end
		end
	end
	if not sync.ok and #summary.failed == 0 then
		summary.failed[#summary.failed + 1] = sync.error or "sync failed"
	end
	return installed, log
end

local function has_conversation(ctx, chat_started)
	if chat_started then
		return true
	end
	local agent = ctx:get("agent")
	if agent then
		local st = agent:get()
		if st and (st.busy or (st.queued or 0) > 0 or (st.message_count or 0) > 0) then
			return true
		end
	end
	return false
end

local function defer_without_focus()
	mare.ui.notify("Optional plugins are available — run /plugins to open Discover.", "info")
	return nil
end

function M.create(ctx, run)
	local chat_started = false
	ctx:on("turn/started", function()
		chat_started = true
	end)
	return function()
		local status, previous = offer_status()
		if status == "chosen" then
			record("chosen", 0)
			return nil
		elseif status ~= "due" then
			return nil
		end
		if has_conversation(ctx, chat_started) then
			return defer_without_focus()
		end
		local summary = { result = "skipped", enabled = {}, refused = {}, failed = {}, synced = false }
		local tries = previous and previous.tries or 0
		local r = run("fetch_catalog", "Fetching the plugins catalog")
		if not r.ok then
			summary.result = "unavailable"
			record(summary.result, tries + 1)
			mare.ui.notify("Plugins catalog unavailable: " .. fetch_error(r), "warning")
			return summary
		end
		if has_conversation(ctx, chat_started) then
			return defer_without_focus()
		end

		local list = mare.plugins.catalog()
		local items = extras_list.discover_items(list)
		for _, item in ipairs(items) do
			if item.plugin and item.plugin.managed == false then
				item.disabled = true
			end
		end
		local chosen = mare.ui.select({
			title = "Discover · Choose optional plugins",
			body = "Plugins run with your permissions and can read your files and run commands.",
			items = items,
			multi = true,
			accept_label = "Install selected",
		})
		if type(chosen) ~= "table" then
			record("skipped", tries)
			return summary
		end
		summary.result = "chosen"
		record("chosen", tries) -- Save the answer before installation or reload.
		local by_name = {}
		for _, e in ipairs(list) do
			by_name[e.name] = by_name[e.name] or e
		end
		local selected_sources = {}
		for _, index in ipairs(chosen) do
			local e = items[index] and items[index].plugin
			if e then
				if selected_sources[e.name] then
					summary.refused[#summary.refused + 1] = {
						name = e.name,
						reason = "choose one source; already selected from " .. selected_sources[e.name],
					}
				else
					local accepted, err = mare.plugins.set_enabled(e.name, true, e.source)
					if accepted then
						by_name[e.name] = e
						selected_sources[e.name] = e.source
						summary.enabled[#summary.enabled + 1] = e.name
						ctx.bus:emit("plugins/plugin", { name = e.name, enabled = true })
					else
						summary.refused[#summary.refused + 1] = { name = e.name, reason = tostring(err) }
					end
				end
			end
		end
		local sync
		if #summary.enabled > 0 then
			sync = run("sync", nil, true)
		end
		summary.synced = sync and sync.ok == true or false
		local installed, log = sync_summary(summary, sync, wanted_packages(summary.enabled, by_name))
		print_summary(summary, installed, log)
		return summary
	end
end

return M
