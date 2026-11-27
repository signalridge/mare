-- commands.completion: input routing and completion for `/` commands and path arguments;
-- part of the `commands` plugin (plugins/commands.lua).
-- availability checks load on the first completion, not at startup
local avail_module = require("mare_lib").lazy(function()
	return require("commands.availability")
end)
local avail = setmetatable({}, {
	__index = function(_, k)
		return avail_module()[k]
	end,
})

local M = {}

local function complete_commands(ctx, commands, ev)
	local tok = ev.token or ""
	local items = {}
	local running = avail.busy(ctx)
	for _, d in ipairs(commands:list().data) do
		if string.find("/" .. d.name, tok, 1, true) == 1 then
			-- `args` = grey hint ("[model]"); `auto_execute` defaults to true for commands
			-- without arguments: Enter runs them, otherwise Enter just completes
			local auto = d.auto_execute
			if auto == nil then
				auto = d.args == nil
			end
			local _, blocked = avail.refusal(ctx, d, running)
			local detail = d.description
			if blocked == "idle" then
				detail = (d.description or "") .. " (after this turn)"
			elseif blocked then
				detail = (d.description or "") .. " (unavailable here)"
			end
			items[#items + 1] = {
				icon = ui.icon("command"),
				style = blocked and "MareDim" or "MareAccent",
				label = "/" .. d.name,
				insert = "/" .. d.name .. " ",
				detail = detail,
				hint = d.args,
				auto_execute = auto,
			}
		end
	end
	return items
end

--- Argument completion: a command's `complete(prefix)` answers Tab on its argument.
--- A failing `complete` offers nothing instead of breaking completion.
local function complete_arguments(reg, ev)
	if ev.line_index ~= 0 or (ev.start or 0) <= 0 then
		return nil
	end
	local name = string.match(ev.text or "", "^/([^/%s]+)%s")
	if not name then
		return nil
	end
	local def = reg.resolve(name)
	if not def or not def.complete then
		return nil
	end
	local ok, items = pcall(def.complete, ev.token or "")
	if not ok or type(items) ~= "table" then
		return nil
	end
	local out = {}
	for _, item in ipairs(items) do
		if type(item) == "table" and item.label then
			out[#out + 1] = {
				label = item.label,
				insert = item.insert or item.label,
				detail = item.detail,
				icon = item.icon or ui.icon("command"),
				style = item.style or "MareAccent",
			}
		end
	end
	return #out > 0 and out or nil
end

local function complete_paths(ev)
	local tok = ev.token or ""
	if tok == "" then
		return nil
	end
	local dir, base = string.match(tok, "^(.*/)([^/]*)$")
	if not dir then
		dir, base = "", tok
	end
	local list_dir = dir == "" and "." or dir
	if string.sub(list_dir, 1, 1) == "~" then
		list_dir = (os.getenv("HOME") or "") .. string.sub(list_dir, 2)
	end
	local items = {}
	for _, e in ipairs(mare.fs.list(list_dir) or {}) do
		if
			string.sub(e.name, 1, #base) == base
			and (string.sub(base, 1, 1) == "." or string.sub(e.name, 1, 1) ~= ".")
		then
			local p = dir .. e.name .. (e.kind == "dir" and "/" or "")
			items[#items + 1] = { label = p, insert = p, icon = ui.file_icon(p), style = ui.filetype_style(p) }
			if #items >= 50 then
				break
			end
		end
	end
	return #items > 0 and items or nil
end

function M.register(ctx, reg)
	local commands = reg.commands
	-- route input: a step of ui's ordered submit chain,
	-- registered while the ui service is up
	ctx:inject({ "ui" }, function(sub)
		sub.ui:handler("submit", "commands", { order = 20 }, function(ev)
			local text = ev.text or ""
			if string.sub(text, 1, 1) == "/" and not string.find(string.sub(text, 1, 40), "^/%S*/") then
				local args = string.match(text, "^/%S*%s*(.-)%s*$") or ""
				local content = { { type = "text", text = args } }
				for _, img in ipairs(ev.images or {}) do
					content[#content + 1] = img
				end
				local handled, res = reg.run_local(text, { content = content })
				ev.handled = handled
				if res ~= nil then
					ev.result = res
				end
			end
			return ev.handled == true
		end)

		-- completion for "/..."
		sub.ui:completion_register({
			name = "commands",
			triggers = { "/" },
			min_prefix = 0,
			score_offset = 10,
			enabled = function(ev)
				local tok = ev.token or ""
				return ev.start == 0 and ev.line_index == 0 and string.sub(tok, 1, 1) == "/"
			end,
			complete = function(ev)
				return complete_commands(ctx, commands, ev)
			end,
		})

		-- path completion for arguments (Tab): ./x, ../x, ~/x, /x, dir/x
		sub.ui:completion_register({
			name = "paths",
			triggers = {},
			score_offset = 0,
			enabled = function(ev)
				local tok = ev.token or ""
				if ev.start == 0 and ev.line_index == 0 then
					return false
				end
				if string.sub(tok, 1, 1) == "@" then
					return false
				end
				return string.find(tok, "/", 1, true) ~= nil
					or string.find(tok, "^~") ~= nil
					or string.find(tok, "^%.") ~= nil
			end,
			complete = function(ev)
				return complete_paths(ev)
			end,
		})
	end)

	-- argument completion for "/<command> <arg>"
	ctx:on("input/complete", function(ev)
		return complete_arguments(reg, ev)
	end)
end

return M
