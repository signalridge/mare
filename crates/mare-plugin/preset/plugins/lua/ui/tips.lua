-- ui.tips: short usage tips shared by the working line (next to the spinner while waiting)
-- and the statusline `tips` segment.
local M = {}

M.DEFAULT_TIPS = {
	"Ctrl+T opens the full transcript",
	"/undo rewinds the conversation and file edits",
	"? lists all key bindings",
	"@ attaches files to your prompt",
	"! runs a shell command directly",
	"Ctrl+B moves a running command to the background",
	"/compact summarizes long conversations",
	"/model switches model or reasoning effort",
}

local function format_tip(text)
	if type(text) ~= "string" or text == "" then
		return nil
	end
	if string.sub(text, 1, 4) == "Tip:" then
		return text
	end
	return "Tip: " .. text
end

local function resolve_list(opts)
	if opts == false then
		return nil, 3, 12
	end
	local items = M.DEFAULT_TIPS
	local delay_s = 3
	local interval_s = 12
	if type(opts) == "table" then
		if type(opts.items) == "table" then
			items = opts.items
		elseif #opts > 0 then
			items = opts
		end
		if type(opts.delay_s) == "number" and opts.delay_s >= 0 then
			delay_s = opts.delay_s
		end
		if type(opts.interval_s) == "number" and opts.interval_s >= 1 then
			interval_s = opts.interval_s
		end
	end
	if #items == 0 then
		return nil, delay_s, interval_s
	end
	return items, delay_s, interval_s
end

--- Tip to show next to the working line after `secs` seconds, or nil when disabled / too early.
function M.pick(secs, opts)
	local items, delay_s, interval_s = resolve_list(opts)
	if not items then
		return nil
	end
	local s = tonumber(secs) or 0
	if s < delay_s then
		return nil
	end
	local idx = (math.floor((s - delay_s) / interval_s) % #items) + 1
	return format_tip(items[idx])
end

--- Current rotating tip for the statusline `tips` segment.
function M.current(opts)
	local items, _, interval_s = resolve_list(opts)
	if not items then
		return nil
	end
	local step = math.max(1, interval_s)
	local idx = (math.floor(os.time() / step) % #items) + 1
	return format_tip(items[idx])
end

return M
