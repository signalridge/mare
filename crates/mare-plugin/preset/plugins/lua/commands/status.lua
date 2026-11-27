-- /status: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /status. Runs in the commands plugin's env.
-- Runtime state at a glance (`/status`): version, model,
-- session, tokens and cost, context use, permission preset, subscription quota, MCP servers and
-- every provider's login.
local function svc(ctx, name)
	local ok, s = pcall(ctx.get, ctx, name)
	return ok and s or nil
end

local function k(n)
	if n >= 1e6 then
		return ("%.1fM"):format(n / 1e6)
	end
	return n >= 1000 and ("%.1fk"):format(n / 1000) or tostring(n)
end

return function(ctx)
	local m = ctx:get("llm") and ctx:get("llm"):current()
	local s = ctx:get("session") and ctx:get("session"):info() or {}
	local p = m and ctx:get("llm"):provider(m.provider)
	local usage, perms = svc(ctx, "usage"), svc(ctx, "permissions")
	local function row(key, v, style)
		return ui.Line({
			ui.Span(("%-12s"):format(key)):hl("MareMuted"),
			ui.Span(tostring(v or "—")):hl(style or "MareText"),
		})
	end
	local function heading(t)
		return ui.Line({ ui.Span(t):hl("MareTitle") })
	end
	local dirs = mare.workspace and table.concat(mare.workspace.dirs(), ", ") or mare.cwd
	---@type mare.Node[]
	local lines = {
		row("version", "mare " .. mare.version),
		row("model", m and m.ref or "none (/login, /model)"),
		row(
			"effort",
			ctx:get("llm") and m and m.reasoning and ctx:get("llm"):effort()
				or (m and "— (no reasoning effort)")
				or nil
		),
		row("auth", p and p.auth_mode and p.auth_mode() or (p and "configured") or nil),
		row("session", (s.name and (s.name .. "  ") or "") .. (s.session_id or "")),
		row("file", s.path or "(not saved yet)"),
		row("directory", dirs),
		row("plugins", #ctx.kernel:plugins()),
	}
	-- plugins and config files that failed at startup, until a full /reload
	-- succeeds; the tracebacks are in /plugins and `mare doctor`
	for _, f in ipairs(mare.plugins and mare.plugins.failures() or {}) do
		lines[#lines + 1] = row(
			"",
			("%s — %s"):format(
				f.summary,
				f.blocks and "requests are blocked; /reload" or "requests continue without it; /reload"
			),
			f.blocks and "MareError" or "MareWarning"
		)
	end
	-- every recorded problem: startup failures, handler errors (the latest per handler)
	local problems = ctx.kernel:problems()
	if #problems > 0 then
		lines[#lines + 1] = row("problems", #problems .. " · /plugins for details", "MareWarning")
	end
	-- permission preset; without the plugin everything runs with full access
	if perms then
		local preset = perms:preset()
		local d = perms.describe and perms:describe(preset)
		lines[#lines + 1] = row("permissions", preset .. (d and d.desc and ("  — " .. d.desc) or ""))
	else
		lines[#lines + 1] = row("permissions", "off — no sandbox, no approvals", "MareWarning")
	end
	-- SYSTEM.md (or `system`) replaces the whole prompt: the sandbox and untrusted-content
	-- sections are not sent
	local cx = svc(ctx, "context")
	local replaced = cx and cx.replaced and cx:replaced()
	if replaced then
		lines[#lines + 1] = row("prompt", replaced, "MareWarning")
	end
	-- tokens, cost and the context window
	if usage then
		local t = usage:totals()
		lines[#lines + 1] = ui.Gap()
		lines[#lines + 1] = heading("Usage")
		lines[#lines + 1] = row(
			"tokens",
			("%s in · %s out · %s cache read · %s cache write · %d request%s"):format(
				k(t.input),
				k(t.output),
				k(t.cache_read),
				k(t.cache_write),
				t.requests,
				t.requests == 1 and "" or "s"
			)
		)
		lines[#lines + 1] = row("cost", usage.cost_text and usage:cost_text() or ("$%.4f"):format(t.cost))
		local used = usage:context_tokens() or 0
		local window = m and m.context_window
		if window and window > 0 then
			local frac = used / window
			lines[#lines + 1] = row(
				"context",
				("%s %s / %s (%d%%) · /context for the breakdown"):format(
					ui.Gauge(math.min(1, frac), 10),
					k(used),
					k(window),
					math.floor(frac * 100 + 0.5)
				),
				frac >= 0.85 and "MareWarning" or nil
			)
		else
			lines[#lines + 1] = row("context", k(used) .. " tokens")
		end
		-- subscription quota windows (rate-limit headers), when a provider reported them
		local q = usage.quota_lines and usage:quota_lines() or {}
		for i, l in ipairs(q) do
			lines[#lines + 1] = row(i == 1 and "rate limits" or "", l.text, l.warning and "MareWarning" or nil)
		end
	end
	-- MCP servers: the mcp plugin's list_servers() snapshot (never connects a server, unlike a
	-- full `doctor` round over every plugin)
	local mcp = {}
	local mcp_svc = svc(ctx, "mcp")
	local mok, mst = false, nil
	if mcp_svc and mcp_svc.list_servers then
		mok, mst = pcall(mcp_svc.list_servers, mcp_svc)
	end
	if mok and type(mst) == "table" then
		for _, err in ipairs(mst.errors or {}) do
			mcp[#mcp + 1] = { name = "config", ok = false, detail = err }
		end
		for _, sv in ipairs(mst.data or {}) do
			mcp[#mcp + 1] = {
				name = sv.name,
				ok = sv.error == nil,
				detail = sv.error or ("%s, %s, %d tools"):format(sv.tier, sv.state, sv.tools),
			}
		end
	end
	if #mcp > 0 then
		lines[#lines + 1] = ui.Gap()
		lines[#lines + 1] = heading("MCP")
		for _, c in ipairs(mcp) do
			lines[#lines + 1] = row(c.name, tostring(c.detail or ""), c.ok and nil or "MareWarning")
		end
	end
	-- every provider's login: method, account (masked), OAuth expiry, and sources that
	-- are set but overridden
	if ctx:get("llm") and ctx:get("llm").status_lines then
		lines[#lines + 1] = ui.Gap()
		lines[#lines + 1] = heading("Logins")
		for _, l in ipairs(ctx:get("llm"):status_lines()) do
			lines[#lines + 1] = ui.Line({
				ui.Span(l.text):hl(l.warning and "MareWarning" or (l.logged_in and "MareText" or "MareMuted")),
			})
		end
	end
	ui.overlay("Status", ui.Lines(lines))
end
