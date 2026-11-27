-- permissions.command: /permission (log, caps), /yolo and the `permissions` service;
-- part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local P, copy, shared, quiet = D.P, D.copy, D.shared, D.quiet
	local plugin_rules, all_rules = D.plugin_rules, D.all_rules
	local Pr = D.presets
	local RANK, LEVEL, LABEL, DESC, LIKE, PRESETS = Pr.RANK, Pr.LEVEL, Pr.LABEL, Pr.DESC, Pr.LIKE, Pr.PRESETS
	local is_partial, set_preset, base_spec = Pr.is_partial, Pr.set_preset, Pr.base_spec
	-- the vote module loads on the first vote, not while this module registers
	local function vote(...)
		return D.vote.vote(...)
	end
	local cycle = { "read-only", "auto", "full-access" }
	local action = mare.keymap.action("permissions.cycle", function()
		ctx:spawn(function()
			local next_preset = cycle[1]
			for i, name in ipairs(cycle) do
				if name == P.preset then
					next_preset = cycle[i % #cycle + 1]
					break
				end
			end
			if not mare.trusted and next_preset ~= "read-only" then
				mare.ui.notify("Permission mode cannot be raised while this folder is untrusted.", "warning")
				return
			end
			local ok, err = set_preset(next_preset, { source = "user", confirm = true })
			if not ok and err ~= "cancelled" then
				mare.ui.notify("permission: " .. tostring(err), "warning")
			end
		end)
	end, {
		desc = "cycle permission mode",
		keys = { editor = { key = "<S-Tab>", when = { "!completion_open" } } },
	})
	ctx:effect(function()
		return action.remove
	end, "permissions cycle action")

	---------------------------------------------------------------------------
	-- commands
	---------------------------------------------------------------------------
	local function show_log()
		local lines = {}
		local recs = {}
		if D.svc.session then
			for _, kind in ipairs({
				"permission/preset",
				"permission/request",
				"permission/result",
				"permission/auto",
				"permission/grant",
			}) do
				for _, r in ipairs(require("mare_lib").all_records(D.svc.session, kind) or {}) do
					recs[#recs + 1] = { kind = kind, data = r }
				end
			end
		end
		for _, r in ipairs(recs) do
			local d = r.data
			local ok, profile = pcall(D.escalation.inc_text, d.permissions or {})
			local detail = ok and profile ~= "" and ("  " .. mare.text.sanitize(profile)) or ""
			if r.kind == "permission/preset" then
				lines[#lines + 1] = ui.Line({ ui.Span("preset → " .. tostring(d.preset)):hl("MareAccent") })
			elseif r.kind == "permission/request" then
				lines[#lines + 1] = ui.Line({
					ui.Span("request "):hl("MareDim"),
					ui.Span(tostring(d.command or d.tool)),
					ui.Span((d.scope and ("  " .. tostring(d.scope)) or "") .. detail):hl("MareMuted"),
					ui.Span(d.result and ("  " .. d.result) or ""):hl("MareWarning"),
				})
			elseif r.kind == "permission/auto" then
				lines[#lines + 1] = ui.Line({
					ui.Span("auto    "):hl("MareDim"),
					ui.Span(tostring(d.command or d.tool)),
					ui.Span("  allowed: " .. tostring(d.basis)):hl("MareSuccess"),
					ui.Span((d.scope and ("  " .. tostring(d.scope)) or "") .. detail):hl("MareMuted"),
				})
			elseif r.kind == "permission/grant" then
				lines[#lines + 1] = ui.Line({
					ui.Span("grant   "):hl("MareDim"),
					ui.Span(tostring(d.tool)),
					ui.Span("  " .. tostring(d.scope or "call") .. detail):hl("MareSuccess"),
				})
			else
				lines[#lines + 1] = ui.Line({
					ui.Span("result  "):hl("MareDim"),
					ui.Span(tostring(d.command or d.tool)),
					ui.Span("  " .. tostring(d.decision)):hl(d.decision == "deny" and "MareError" or "MareSuccess"),
					ui.Span((d.scope and ("  " .. tostring(d.scope)) or "") .. detail):hl("MareMuted"),
				})
			end
		end
		if #lines == 0 then
			lines[1] = ui.Line({ ui.Span("No permission decisions in this session"):hl("MareDim") })
		end
		mare.ui.overlay("Permission log", ui.Lines(lines))
	end

	-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
	ctx:inject({ "commands" }, function(ctx)
		ctx.commands:register("permission", {
			description = "Permission preset (read-only / auto / full-access) · /permission log · /permission caps",
			args = "[read-only|auto|full-access|log|caps]",
			run = function(args)
				args = args and args:match("^%s*(.-)%s*$") or ""
				if args == "log" then
					return show_log()
				end
				if args == "caps" then
					local caps = mare.proc.sandbox.capabilities(mare.cwd)
					-- Windows, workspace containing %TEMP%: say so
					local temp = caps.temp and ("\n" .. caps.temp) or ""
					return mare.ui.notify(
						"sandbox "
							.. tostring(caps.summary)
							.. "\n"
							.. tostring(caps.escalation_reads)
							.. temp
							.. "\nnetwork rules: "
							.. tostring(caps.network_scope),
						"info"
					)
				end
				if args ~= "" then
					local ok, err = set_preset(args, { source = "user", confirm = true })
					if not ok then
						mare.ui.notify("permission: " .. tostring(err), "warning")
					end
					return
				end
				local items, sel = {}, 1
				for i, name in ipairs(PRESETS) do
					if name == P.preset then
						sel = i
					end
					items[i] = {
						label = ("%s  %s"):format(LABEL[name], name),
						detail = DESC[name] .. (name == P.preset and "  (current)" or ""),
					}
				end
				local title = P.preset == "custom" and "Permissions — current: custom (not a preset)" or "Permissions"
				-- what this host's sandbox can actually enforce
				local caps = mare.proc.sandbox.capabilities()
				title = title .. "  ·  sandbox " .. tostring(caps.summary or caps.mechanism)
				if is_partial() then
					title = title .. "  ·  PARTIAL (some sandbox controls unavailable)"
				end
				local i = mare.ui.select({ title = title, items = items, selected = sel })
				if i then
					local ok, err = set_preset(PRESETS[i], { source = "user", confirm = true })
					if not ok and err ~= "cancelled" then
						mare.ui.notify("permission: " .. tostring(err), "warning")
					end
				end
			end,
		})
		ctx.commands:register("yolo", {
			description = "Switch to full-access (no sandbox; explicit prompt rules and dangerous-command checks still ask) — /permission full-access",
			run = function()
				local ok, err = set_preset("full-access", { source = "user", confirm = true })
				if not ok and err ~= "cancelled" then
					mare.ui.notify("permission: " .. tostring(err), "warning")
				end
			end,
		})
	end)

	---------------------------------------------------------------------------
	-- service
	---------------------------------------------------------------------------
	local function current_state(include_sensitive)
		shared.refresh()
		local active = P.sandbox_ready and P.preset or "off"
		local partial = P.sandbox_ready and is_partial() or false
		local label = active == "off" and "OFF" or (LABEL[active] or string.upper(active))
		local state = {
			preset = active,
			label = label,
			partial = partial,
			policy_revision = D.policy_revision(),
		}
		if include_sensitive ~= false then
			local rules = mare.json.array()
			for _, r in ipairs(all_rules()) do
				rules[#rules + 1] = r
			end
			state.rules = rules
			state.grants = D.request.stores() or {}
		end
		if active ~= "off" then
			local cur_sb = mare.proc.sandbox.current()
			local level = LEVEL[active] or (cur_sb and cur_sb.level) or nil
			if level ~= nil then
				state.level = level
			end
			if LIKE[active] ~= nil then
				state.like = LIKE[active]
			end
			if DESC[active] ~= nil then
				state.desc = DESC[active]
			end
		end
		return state
	end

	require("mare_lib").service(ctx, "permissions", {
		get = function(_, caller)
			if D.sync_external_sandbox then
				D.sync_external_sandbox()
			end
			local is_peer = mare.contracts and mare.contracts.is_peer(caller)
			return current_state(not is_peer)
		end,
		list = function()
			if D.sync_external_sandbox then
				D.sync_external_sandbox()
			end
			local active = P.sandbox_ready and P.preset or "off"
			local out = mare.json.array()
			for _, name in ipairs(PRESETS) do
				out[#out + 1] = {
					name = name,
					label = LABEL[name],
					desc = DESC[name],
					like = LIKE[name],
					level = LEVEL[name],
					current = name == active,
				}
			end
			return { data = out }
		end,
		grants = function()
			if D.sync_external_sandbox then
				D.sync_external_sandbox()
			end
			return D.request.stores()
		end,
		request = function(_, _, profile, opts)
			opts = opts or {}
			assert(D.svc.tools, "permission requests require the tools service")
			return D.svc.tools:execute({
				id = opts.request_id or ("permissions-" .. mare.crypto.random_hex(16)),
				name = "request_permissions",
				arguments = {
					additional_permissions = profile,
					justification = opts.justification,
					scope = opts.scope,
				},
			}, { cancel = opts.cancel, approve = opts.approve, on_state = opts.on_state })
		end,
		commit_request = function(_, _, call)
			return D.commit_permissions(call)
		end,
		describe_request = function(_, _, call, expected)
			return D.describe_permissions(call, expected)
		end,
		resolve_request = function(_, _, call, constraints, scope, expected)
			return D.resolve_permissions(call, constraints, scope, expected)
		end,
		policy_revision = function()
			return D.policy_revision()
		end,
		is_partial = function()
			return is_partial()
		end,
		--- preset() -> "off" | "read-only" | "auto" | "full-access" | "custom"
		preset = function()
			if D.sync_external_sandbox then
				D.sync_external_sandbox()
			end
			return P.sandbox_ready and P.preset or "off"
		end,
		--- set(name, {confirm?}) -> permission_state (throws coded error on failure)
		set = function(_, caller, name, opts)
			local cname = require("mare_lib").caller_name(caller)
			local source = (cname ~= nil and string.sub(cname, 1, 4) == "rpc:") and "rpc" or "api"
			local ok, err, code = set_preset(name, { source = source, confirm = opts and opts.confirm })
			if not ok then
				error({ code = code or "permissions.sandbox_failed", message = tostring(err) }, 0)
			end
			return current_state()
		end,
		presets = function()
			return copy(PRESETS)
		end,
		--- describe(name) -> {label, desc, like, level} of a preset (built-in or added)
		describe = function(_, _, name)
			if not RANK[name] then
				return nil
			end
			return { label = LABEL[name], desc = DESC[name], like = LIKE[name], level = LEVEL[name] }
		end,
		--- add_rules(rules): a rule source owned by the calling plugin (dropped on its dispose)
		add_rules = function(_, caller, rules)
			local valid, err = D.rules.validate_source(rules)
			assert(valid, err)
			local src = copy(rules or {})
			-- Invalidate approvals before the rule table changes, including in child VMs.
			D.bump_policy_revision()
			plugin_rules[#plugin_rules + 1] = src
			require("mare_lib").owned(caller, function()
				D.bump_policy_revision()
				for i, x in ipairs(plugin_rules) do
					if x == src then
						table.remove(plugin_rules, i)
						break
					end
				end
			end, "permission rules")
		end,
		rules = function()
			shared.refresh()
			return all_rules()
		end,
		--- check(call, info) -> the vote this plugin would cast (for UIs / tests)
		check = function(_, _, call, info)
			shared.refresh()
			return vote(call, info or {})
		end,
		--- rule_verdict(call, info) -> {decision = "allow", sandbox} | {decision = "deny", reason}
		--- | nil: what the configured and remembered rules decide about a call (this agent's
		--- session-only rules aside); the host escalates a sub-agent sharing this configuration
		--- by it
		rule_verdict = function(_, _, call, info)
			shared.refresh()
			local co = coroutine.running()
			quiet[co] = true
			local v = vote(call, info or {})
			quiet[co] = nil
			if v.decision == "deny" then
				return { decision = "deny", reason = v.reason }
			elseif v.decision == "allow" and v.basis == "rule" and not v.own_rule then
				return { decision = "allow", sandbox = v.sandbox or base_spec() }
			end
		end,
	})
end
