-- The extra writable roots of a preset for the `permissions` plugin (the
-- entry layer, plugins/permissions.lua).
--
-- Only existing absolute directories become writable roots; a configured one that is missing
-- (or not a directory) is skipped. Skipping silently hides a typo, so each such path is reported
-- once, naming it: always in the log, and as a warning notice once the UI has its first frame
-- (`ready()`, from `app/ready`; a path found later is noticed at once). The set of reported
-- paths lives in this module, so a preset switch or reload does not repeat a notice.
local seen = {} -- resolved path -> true (already reported)
local pending = {} -- notices waiting for the UI
local is_ready = false

local M = {}

--- The existing directories among `list` (resolved to absolute paths).
function M.existing(ctx, list)
	local out = {}
	for _, p in ipairs(list or {}) do
		local abs = mare.fs.resolve(p)
		local st = mare.fs.stat(abs)
		if st and st.kind == "dir" then
			out[#out + 1] = abs
		elseif not seen[abs] then
			seen[abs] = true
			local why = st and "is not a directory" or "does not exist"
			local msg = ("Writable root %s %s — skipped"):format(abs, why)
			mare.log("warn", msg)
			if is_ready and mare.ui.connected() then
				mare.ui.notify(msg, "warning")
			else
				pending[#pending + 1] = msg
			end
		end
	end
	return out
end

--- The entries of `list` that are not existing directories (resolved, in order, no repeats).
function M.missing(list)
	local out, seen_here = {}, {}
	for _, p in ipairs(list or {}) do
		local abs = mare.fs.resolve(p)
		local st = mare.fs.stat(abs)
		if not (st and st.kind == "dir") and not seen_here[abs] then
			seen_here[abs] = true
			out[#out + 1] = abs
		end
	end
	return out
end

--- The UI is up: the notices collected so far (and later ones are shown as they come).
function M.ready()
	is_ready = true
	local out = pending
	pending = {}
	return out
end

return M
