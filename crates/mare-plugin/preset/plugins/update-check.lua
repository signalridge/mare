-- update-check: at most once a day, after the first frame, ask
-- GitHub releases whether a newer mare exists and say so in one line. Never installs anything.
-- The only data sent is the request itself (no identifiers beyond the generic `mare/<version>`
-- user agent). Off with `mare.o.update_check = false` (or `MARE_NO_UPDATE_CHECK=1`,
-- `MARE_OFFLINE=1`, or `[plugins.update-check] enabled = false`).
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
		local function policy()
			local o = mare.o or mare.config or {}
			if mare.install and mare.install.policy then
				return mare.install.policy(o.update_check, cfg.enabled)
			end
			if cfg.enabled == false or o.update_check == false then
				return "disabled"
			end
			return "notify"
		end
		local function enabled()
			return policy() ~= "disabled"
		end

		local svc = {}
		--- check() -> {latest, current, newer, channel, managed?, command?, manual_hint?, release_url, policy} | nil, reason. Makes no request when disabled.
		function svc.check()
			local p = policy()
			if p == "disabled" then
				return nil, "disabled"
			end
			local ok, r = pcall(mare.http.request, {
				method = "GET",
				url = cfg.url,
				headers = { accept = "application/vnd.github+json" },
				timeout_ms = cfg.timeout_ms,
			})
			local now = os.time()
			local next_check = now
			if ok and type(r) == "table" and type(r.headers) == "table" then
				local ra = tonumber(r.headers["retry-after"] or r.headers["Retry-After"])
				if ra and ra > 0 then
					local interval_secs = (tonumber(cfg.interval_hours) or 24) * 3600
					local backoff = math.min(ra, 7 * 86400)
					if backoff > interval_secs then
						next_check = now + (backoff - interval_secs)
					end
				end
			end
			-- Always record attempted checks (including HTTP errors / rate limits) so failures stay throttled.
			pcall(mare.state.set, "update_check_at", next_check)
			if not ok or not r.ok then
				return nil, ok and ("HTTP " .. tostring(r.status)) or tostring(r)
			end
			local body = mare.json.decode(r:text() or "")
			local tag = type(body) == "table" and body.tag_name
			if type(tag) ~= "string" then
				return nil, "no tag_name"
			end
			local info = mare.install and mare.install.info()
				or {
					channel = "other",
					managed = false,
					release_url = "https://github.com/signalridge/mare/releases/latest",
				}
			local res = {
				latest = tag,
				current = mare.version,
				newer = mare.semver.is_newer_release(tag, mare.version),
				channel = info.channel,
				managed = info.managed,
				command = info.command,
				manual_hint = info.manual_hint,
				release_url = info.release_url,
				policy = p,
			}
			if res.newer then
				ctx.bus:emit("update-check/available", res)
			end
			return res
		end
		-- a mare.Service
		require("mare_lib").service(ctx, "update-check", svc)

		local function format_self_notice(res)
			local display = tostring(res.latest):gsub("^v", "")
			if res.managed and type(res.command) == "string" and res.command ~= "" then
				return ("mare %s is published on GitHub (update via: %s)"):format(display, res.command)
			end
			if type(res.command) == "string" and res.command ~= "" then
				return ("mare %s is available (%s)"):format(display, res.command)
			end
			if type(res.manual_hint) == "string" and res.manual_hint ~= "" then
				return ("mare %s is available (%s)"):format(display, res.manual_hint)
			end
			return ("mare %s is available (download at %s)"):format(
				display,
				res.release_url or "https://github.com/signalridge/mare/releases/latest"
			)
		end

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
					if res.policy == "auto" and res.channel == "standalone" and mare.install.stage_update then
						local ok, stage = pcall(mare.install.stage_update, cfg.url)
						if ok and type(stage) == "table" and stage.ok and stage.staged then
							mare.ui.notify(
								("mare %s staged (restart mare to apply)"):format(tostring(stage.staged.version)),
								"info"
							)
							return
						end
					end
					mare.ui.notify(format_self_notice(res), "info")
				end
			end)
		end)
	end,
}
