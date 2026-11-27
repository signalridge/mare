-- Report mare's state to herdr (https://herdr.dev) — configuration only, no plugin needed.
-- Copy into ~/.config/mare/init.lua. Flags follow `herdr pane <sub> --help`.
--
-- mare.status.hook runs the command in the background whenever the unified state changes
-- (coalesced to at most one run per throttle_ms, latest state wins), on session start / resume /
-- fork / switch, and once on exit (mare waits for it, bounded by timeout_ms).

if os.getenv("HERDR_ENV") == "1" and os.getenv("HERDR_PANE_ID") then
	local pane = os.getenv("HERDR_PANE_ID")
	local herdr = os.getenv("HERDR_BIN_PATH") or "herdr"
	local who = { "--source", "mare", "--agent", "mare" }

	local function argv(sub, s, extra)
		local a = { herdr, "pane", sub, pane }
		for _, x in ipairs(who) do
			a[#a + 1] = x
		end
		for _, x in ipairs(extra or {}) do
			a[#a + 1] = x
		end
		a[#a + 1] = "--seq"
		a[#a + 1] = tostring(s.seq)
		if s.session.session_id then
			a[#a + 1] = "--agent-session-id"
			a[#a + 1] = s.session.session_id
		end
		if s.session.path then
			a[#a + 1] = "--agent-session-path"
			a[#a + 1] = s.session.path
		end
		-- how herdr restarts this session after it restarts: `mare -r <id>`
		if s.session.resume then
			a[#a + 1] = "--"
			for _, x in ipairs(s.session.resume) do
				a[#a + 1] = x
			end
		end
		return a
	end

	mare.status.hook({
		on = { "status" },
		throttle_ms = 200,
		cmd = function(s)
			return argv("report-agent", s, { "--state", s.state, "--message", s.message or "" })
		end,
	})
	mare.status.hook({
		on = { "session" },
		cmd = function(s)
			return argv("report-agent-session", s)
		end,
	})
	mare.status.hook({
		on = { "exit" },
		cmd = function(s)
			return {
				herdr,
				"pane",
				"release-agent",
				pane,
				"--source",
				"mare",
				"--agent",
				"mare",
				"--seq",
				tostring(s.seq),
			}
		end,
	})
end
