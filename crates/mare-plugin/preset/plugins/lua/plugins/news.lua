-- plugins.news: update news extraction, tracking and display;
-- part of the `pkg` plugin (plugins/plugins.lua).
local M = {}

local function section(text, version)
	if not text then
		return nil
	end
	local out, on = {}, false
	for line in (text .. "\n"):gmatch("(.-)\n") do
		local v = line:match("^##%s+v?([%w%.%-]+)")
		if v then
			if on then
				break
			end
			on = (v == version)
		elseif on then
			out[#out + 1] = line
		end
	end
	local s = table.concat(out, "\n"):gsub("^%s+", ""):gsub("%s+$", "")
	return s ~= "" and s or nil
end

function M.lines(title, body)
	if mare.ui and mare.ui.transform_markdown then
		body = mare.ui.transform_markdown(body, {
			message_type = "changelog",
			is_streaming = false,
			available_width = mare.ui.width and mare.ui.width() or 80,
		})
	end
	local lines = { ui.Line({ ui.Span("◆ " .. title):hl("MareAccent") }) }
	for l in (body .. "\n"):gmatch("(.-)\n") do
		lines[#lines + 1] = ui.Line({ ui.Span("  " .. l) })
	end
	return lines
end

local function show_news(title, body)
	mare.ui.print(ui.Lines(M.lines(title, body)))
end

local function valid_news_doc(parsed, expected_version)
	if
		type(parsed) ~= "table"
		or mare.json.is_array(parsed)
		or math.type(parsed.version) ~= "integer"
		or parsed.version ~= expected_version
	then
		return false
	end
	if parsed.seen ~= nil then
		if type(parsed.seen) ~= "table" or mare.json.is_array(parsed.seen) then
			return false
		end
		for k, v in pairs(parsed.seen) do
			if type(k) ~= "string" or type(v) ~= "string" then
				return false
			end
		end
	end
	return true
end

--- news() -> titles shown: NEWS.md sections for mare and enabled plugins whose version
--- changed since last seen; a fresh install only records the versions
function M.news()
	local expected_version = assert(mare.plugins.news_version, "mare.plugins.news_version is required")
	local news_path = mare.paths.state .. "/news.json"
	local shown = {}
	local lock_ok, guard = pcall(mare.lock.file, news_path .. ".lock", {
		timeout_ms = 5000,
		root = mare.paths.state,
	})
	if not lock_ok then
		mare.log("warn", ("%s lock failed: %s"):format(news_path, tostring(guard)))
		return shown
	end
	local _lock_guard <close> = setmetatable({}, {
		__close = function()
			pcall(guard.release, guard)
		end,
	})

	local pending_shows = {}
	local write_ok = false
	local run_ok, run_err = pcall(function()
		local raw, read_err, code = mare.fs.read(news_path, { no_follow = true })
		local first = raw == nil and code == "not_found"
		local seen
		if raw == nil then
			if code ~= "not_found" then
				mare.log("warn", ("%s cannot be read: %s"):format(news_path, tostring(read_err)))
				return
			end
			seen = { version = expected_version, seen = {} }
		else
			local ok, parsed = pcall(mare.json.decode_lossless, raw)
			if not ok or not valid_news_doc(parsed, expected_version) then
				local detail = ok and "unsupported format or version" or tostring(parsed)
				mare.log(
					"warn",
					("%s is unreadable (%s); preserving file without resetting news state"):format(news_path, detail)
				)
				return
			end
			seen = parsed
			seen.seen = seen.seen or {}
		end
		local changed = false
		local function check(key, version, text_fn, title)
			if not version or seen.seen[key] == version then
				return
			end
			if not first and seen.seen[key] ~= nil then
				local text = type(text_fn) == "function" and text_fn() or text_fn
				local body = section(text, version)
				if body then
					pending_shows[#pending_shows + 1] = {
						label = title .. " " .. version,
						body = body,
					}
				end
			end
			seen.seen[key] = version
			changed = true
		end
		check("mare", mare.version, mare.plugins.mare_news, "What's new in mare")
		local eok, list = pcall(mare.plugins.catalog)
		for _, e in ipairs(eok and list or {}) do
			if e.enabled and e.news then
				check("plugin:" .. e.name, e.version, function()
					return mare.fs.read(e.news)
				end, "What's new in " .. e.name)
			end
		end
		if changed then
			local call_ok, wok, werr = pcall(mare.fs.write, news_path, mare.json.encode(seen), {
				atomic = true,
				mode = 384,
				root = mare.paths.state,
			})
			if not call_ok then
				mare.log("warn", ("failed to write %s: %s"):format(news_path, tostring(wok)))
				return
			elseif not wok then
				mare.log("warn", ("failed to write %s: %s"):format(news_path, tostring(werr)))
				return
			end
		end
		write_ok = true
	end)
	pcall(guard.release, guard)
	if not run_ok then
		mare.log("warn", ("failed to update %s: %s"):format(news_path, tostring(run_err)))
		return shown
	end
	if write_ok then
		for _, item in ipairs(pending_shows) do
			show_news(item.label, item.body)
			shown[#shown + 1] = item.label
		end
	end
	return shown
end

--- all_news() -> { {title, body} }: the current versions' sections again (`/news`, LazyVim
--- `util/news.lua`), whether seen or not
function M.all_news()
	local out = {}
	local body = section(mare.plugins.mare_news(), mare.version)
	if body then
		out[#out + 1] = { title = "What's new in mare " .. mare.version, body = body }
	end
	local eok, list = pcall(mare.plugins.catalog)
	for _, e in ipairs(eok and list or {}) do
		if e.enabled and e.news and e.version then
			local b = section(mare.fs.read(e.news), e.version)
			if b then
				out[#out + 1] = { title = "What's new in " .. e.name .. " " .. e.version, body = b }
			end
		end
	end
	return out
end

return M
