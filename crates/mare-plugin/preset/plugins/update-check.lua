-- update-check: at most once a day, after the first frame, ask
-- GitHub releases whether a newer mare exists and say so in one line. Never installs anything.
-- The only data sent is the request itself (no identifiers beyond the generic `mare/<version>`
-- user agent). Off with `mare.o.update_check = false` (or `[plugins.update-check] enabled = false`).
-- Background version check, quiet on failure.

return {
	name = "update-check",
	provide = { "update-check" },
	-- `task` gives ctx:spawn, `bus` the mare event. It checks on ui/ready, once per launch: a
	-- reload does not check again (the check is once per interval anyway)
	inject = { "task", "bus" },
	config_schema = {
		enabled = { type = "boolean", default = true, desc = "Check for a newer release." },
		url = {
			type = "string",
			default = "https://api.github.com/repos/signalridge/mare/releases/latest",
			desc = "Release API endpoint to query.",
		},
		interval_hours = { type = "number", default = 24, desc = "Hours between checks." },
		timeout_ms = { type = "integer", default = 5000, desc = "Request timeout in milliseconds." },
	},
	apply = function(ctx, cfg)
		local function enabled()
			local o = mare.o or mare.config or {}
			return cfg.enabled ~= false and o.update_check ~= false
		end

		local svc = {}
		--- check() -> {latest, current, newer} | nil, reason. Makes no request when disabled.
		function svc.check()
			if not enabled() then
				return nil, "disabled"
			end
			local ok, r = pcall(mare.http.request, {
				method = "GET",
				url = cfg.url,
				headers = { accept = "application/vnd.github+json" },
				timeout_ms = cfg.timeout_ms,
			})
			if not ok or not r.ok then
				return nil, ok and ("HTTP " .. tostring(r.status)) or tostring(r)
			end
			local body = mare.json.decode(r:text() or "")
			local tag = type(body) == "table" and body.tag_name
			if type(tag) ~= "string" then
				return nil, "no tag_name"
			end
			pcall(mare.state.set, "update_check_at", os.time()) -- locked, atomic; never over a corrupt file
			local res =
				{ latest = tag, current = mare.version, newer = mare.semver.is_newer_release(tag, mare.version) }
			if res.newer then
				ctx.bus:emit("update-check/available", res)
			end
			return res
		end
		-- a mare.Service
		require("mare_lib").service(ctx, "update-check", svc)

		-- automatically: interactive sessions only, after the UI is up, at most every interval
		ctx:on("ui/ready", function()
			-- not from development builds (tests, `cargo run`) skipping dev versions
			if mare.mode ~= "tui" or mare.debug_build or not enabled() then
				return
			end
			local last = tonumber((mare.state.read() or {}).update_check_at) or 0
			local age = os.time() - last
			if age >= 0 and age < cfg.interval_hours * 3600 then
				return
			end
			ctx:spawn(function()
				local res = svc.check()
				if res and res.newer and mare.ui.connected() then
					mare.ui.notify(("mare %s is available (you have %s)"):format(res.latest, res.current), "info")
				end
			end)
		end)
	end,
}
