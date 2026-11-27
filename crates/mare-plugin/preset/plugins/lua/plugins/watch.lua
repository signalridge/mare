-- plugins.watch: config/plugins change detection notices;
-- part of the `pkg` plugin (plugins/plugins.lua).
local M = {}

local function stamp(path)
	local st = mare.fs.stat(path)
	return st and (tostring(st.mtime) .. ":" .. tostring(st.size)) or "missing"
end

local function content(path)
	local ok, text = pcall(mare.fs.read, path)
	return ok and text or false
end

function M.setup(ctx, cfg)
	if mare.mode ~= "tui" or (cfg.watch_ms or 0) <= 0 then
		return
	end
	-- path → { stamp = mtime:size, text = content }; the stat is the cheap check, the
	-- content decides (a save without edits or a `touch` is not a change). Capture baseline
	-- stat stamps synchronously at setup() so we never miss edits between setup() and the
	-- background seed() completion, while keeping async mare.fs.read off plugins.apply().
	local baseline_stamps = {}
	for _, p in ipairs(mare.plugins.watched()) do
		baseline_stamps[p] = stamp(p)
	end
	local seen = {}
	local seeding = false
	local seeded = false
	local function notify_changed(p)
		local name = p:match("[^/\\]+$") or p
		ui.notify(("%s changed — /reload applies it"):format(name))
	end
	local function seed()
		if seeded or seeding then
			return
		end
		seeding = true
		local ok, err = pcall(function()
			for _, p in ipairs(mare.plugins.watched()) do
				local before = baseline_stamps[p]
				local now = stamp(p)
				local text = content(p)
				local after = stamp(p)
				if after ~= now then
					text = content(p)
					after = stamp(p)
				end
				seen[p] = { stamp = after, text = text }
				if (before ~= nil and (now ~= before or after ~= before)) or after ~= now then
					notify_changed(p)
				end
			end
		end)
		seeding = false
		if not ok then
			error(err, 0)
		end
		seeded = true
	end
	ctx:spawn(seed)
	ctx:every(cfg.watch_ms, function()
		if seeding then
			return
		end
		if not seeded then
			seed()
			return
		end
		-- watched() follows the config: modules the last evaluation required
		for _, p in ipairs(mare.plugins.watched()) do
			local now = stamp(p)
			local old = seen[p]
			if old == nil then
				seen[p] = { stamp = now, text = content(p) }
			elseif old.stamp ~= now then
				local text = content(p)
				if text ~= old.text then
					notify_changed(p)
				end
				seen[p] = { stamp = now, text = text }
			end
		end
	end)
end

return M
