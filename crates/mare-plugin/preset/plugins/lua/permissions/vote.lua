-- permissions.vote: the policy vote cast on tool/before; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local cfg = D.cfg
	local P, child, record = D.P, D.child, D.record
	local LIKE, is_network_partial, base_spec = D.presets.LIKE, D.presets.is_network_partial, D.presets.base_spec
	local E = D.escalation
	local widen, inc_remainder, inc_union, inc_text = E.widen, E.inc_remainder, E.inc_union, E.inc_text
	local R = D.rules
	local resolve_command, command_text, shell_verdict, tool_rule, segments_text, prefix_of, DEFAULT_DENY_ARGS =
		R.resolve_command,
		R.command_text,
		R.shell_verdict,
		R.tool_rule,
		R.segments_text,
		R.prefix_of,
		R.DEFAULT_DENY_ARGS
	local norm_argv = D.argv.norm_argv
	local SEVERITY = { forbidden = 3, prompt = 2, allow = 1 }

	---------------------------------------------------------------------------
	-- the policy vote (tool/before)
	---------------------------------------------------------------------------
	local function label_of(call, repr)
		if repr and repr.text and repr.text ~= "" then
			if repr.dynamic and repr.kind == "shell" and repr.shell and repr.shell ~= "" then
				return tostring(repr.shell) .. ": " .. repr.text
			end
			return repr.text
		end
		local a = call.arguments or {}
		return call.name .. (a.path and (" " .. tostring(a.path)) or "")
	end
	local function source_tag()
		if child then
			return "[" .. tostring(mare.args and mare.args.agent_label or "sub-agent") .. "] "
		end
		return ""
	end

	--- the dangerous-command rule a command representation hits (built-in or added), or nil
	local function dangerous_match(repr)
		if not (repr and (repr.kind == "shell" or repr.kind == "argv")) then
			return nil
		end
		local d = cfg.dangerous or {}
		if d.enabled == false then
			return nil
		end
		local input = repr.kind == "argv" and repr.argv or repr.text
		local name, cmds = mare.text.dangerous(input, { shell = repr.shell })
		local off = {}
		for _, n in ipairs(d.disable or {}) do
			off[n] = true
		end
		if name and not off[name] then
			return name
		end
		for _, rule in ipairs(d.rules or {}) do
			local want = type(rule) == "table" and (rule.argv or rule) or nil
			if type(want) == "table" and #want > 0 then
				for _, argv in ipairs(cmds or {}) do
					local hit = #argv >= #want
					for i, w in ipairs(want) do
						hit = hit and argv[i] == w
					end
					if hit then
						return type(rule.name) == "string" and rule.name or table.concat(want, " ")
					end
				end
			end
		end
	end
	--- every segment is matched by an `allow` rule naming exactly that command (the whole argv)
	local function exact_allow(sv)
		if not (sv and sv.parsed and sv.parsed.analyzable and #sv.segs > 0) then
			return false
		end
		for _, sg in ipairs(sv.segs) do
			local r = sg.rule
			if not (r and r.decision == "allow" and type(r.argv) == "table" and #r.argv == #sg.argv) then
				return false
			end
		end
		return true
	end

	local function vote(call, info)
		if D.sync_external_sandbox then
			D.sync_external_sandbox()
		end
		local invalid = R.invalid_deny(call)
		if invalid then
			return { decision = "deny", reason = invalid }
		end
		local args = type(call.arguments) == "table" and call.arguments or {}
		local cwd = type(call.cwd) == "string" and call.cwd or mare.cwd
		local repr = resolve_command(call, info)
		local cmd_text = repr and repr.text or command_text(call, info)
		local missing_repr = repr ~= nil and repr.kind == "unanalyzable"
		local sv, argv
		if repr then
			sv = shell_verdict(call, repr)
			-- the segment a new rule would cover: the only segment, or the only unmatched one
			if sv.parsed.analyzable and not sv.parsed.script then
				for _, sg in ipairs(sv.segs) do
					if #sv.segs == 1 or not (sg.rule and sg.rule.decision == "allow") then
						argv = argv == nil and sg.argv or false
					end
				end
				argv = argv or nil
			end
		end
		local sr = sv and (sv.decision and sv.rule) or nil
		local tr = tool_rule(call, info)
		local pr = R.path_verdict(call, info)
		if repr ~= nil and not (sv and sv.parsed.analyzable) and tr and tr.decision == "allow" then
			tr = nil
		end
		local rule = sr
		if tr and (not rule or (SEVERITY[tr.decision] or 0) > (SEVERITY[rule.decision] or 0)) then
			rule = tr
		end
		if pr and (not rule or (SEVERITY[pr.decision] or 0) > (SEVERITY[rule.decision] or 0)) then
			rule = pr
		end
		if rule and rule.decision == "forbidden" then
			record("request", { tool = call.name, command = cmd_text, result = "forbidden" })
			return {
				decision = "deny",
				reason = "forbidden by a permission rule"
					.. (rule.justification and (": " .. rule.justification) or ""),
			}
		end
		local sensitive = D.sensitive.collect(call, info, repr, sv and sv.parsed)
		local sensitive_reason
		if sensitive then
			local paths = {}
			for _, item in ipairs(sensitive) do
				paths[#paths + 1] = (item.op or "access") .. " " .. item.path
			end
			sensitive_reason = "credential path reminder: " .. table.concat(paths, ", ")
		end
		local structured = D.request.vote(call, info)
		if structured then
			if rule and rule.decision == "prompt" and structured.decision ~= "deny" then
				structured.decision = "ask"
				structured.categories = structured.categories or {}
				structured.categories[#structured.categories + 1] = "rules"
				structured.reason = (structured.reason and (structured.reason .. " · ") or "")
					.. "a rule asks before this call"
			end
			if structured.decision == "deny" then
				return structured
			end
			if sensitive_reason then
				structured.decision = "ask"
				structured.categories = structured.categories or {}
				structured.categories[#structured.categories + 1] = "tool"
				structured.reason = (structured.reason and (structured.reason .. " · ") or "") .. sensitive_reason
				structured.amendment = nil
			end
		end
		-- dangerous commands: after forbidden rules (deny wins), before the preset — full-access still asks
		local danger = dangerous_match(repr)
		if danger and not exact_allow(sv) then
			record("request", { tool = call.name, command = cmd_text, result = "dangerous", rule = danger })
			local reason = ("%s%s · dangerous command (%s) — asks in every preset"):format(
				source_tag(),
				label_of(call, repr),
				danger
			)
			local st = segments_text(sv)
			reason = st and (reason .. " · " .. st) or reason
			if sensitive_reason then
				reason = reason .. " · " .. sensitive_reason
			end
			if structured then
				structured.decision = "ask"
				structured.categories = structured.categories or {}
				if rule and rule.decision == "prompt" then
					local has_rules = false
					for _, c in ipairs(structured.categories) do
						if c == "rules" then
							has_rules = true
							break
						end
					end
					if not has_rules then
						structured.categories[#structured.categories + 1] = "rules"
					end
				end
				structured.reason = (structured.reason and (structured.reason .. " · ") or "") .. reason
				return structured
			end
			local danger_categories = {}
			if sensitive_reason then
				danger_categories[#danger_categories + 1] = "tool"
			end
			if rule and rule.decision == "prompt" then
				danger_categories[#danger_categories + 1] = "rules"
			end
			local granted = D.request.stores()
			local danger_sandbox = not E.inc_empty(granted) and E.apply_inc(base_spec(), granted) or nil
			return {
				decision = "ask",
				category = danger_categories[1],
				categories = #danger_categories > 0 and danger_categories or nil,
				reason = reason,
				sandbox = danger_sandbox,
			}
		end
		if structured then
			return structured
		end
		-- `like` decides only the approval behaviour; the sandbox is always the preset's own
		-- effective policy (the session's current one) — never switched off by a vote
		local preset = LIKE[P.preset] or P.preset
		-- every automatic allow (full-access, a matching rule, this session's approvals) goes
		-- into /permission log with its basis
		local function auto_allow(basis, sandbox, extra)
			local d = { tool = call.name, command = cmd_text, basis = basis }
			for k, v in pairs(extra or {}) do
				d[k] = v
			end
			record("auto", d)
			return { decision = "allow", sandbox = sandbox, basis = basis }
		end
		local args_scope, why = args.sandbox_permissions, args.justification
		local granted = D.request.stores()
		local base = base_spec()
		local sandbox = base ~= false and not E.inc_empty(granted) and E.apply_inc(base, granted) or nil
		-- a forbidden or prompt rule may name what a not-analyzable command runs (`pu\sh`,
		-- `p{ush,}` are not their argv): it asks in every preset, full-access too, instead of
		-- running what a rule may forbid (PERM-ESCAPE; rules.lua shell_verdict marks it "prompt").
		-- Command tools with no analyzable command representation at all also never auto-allow.
		local unresolved = rule and rule.decision == "prompt" and sv and not sv.parsed.analyzable
		if
			preset == "full-access"
			and not unresolved
			and not missing_repr
			and not (rule and rule.decision == "prompt")
			and (base == false or (args_scope == nil and why == nil))
		then
			return auto_allow("full-access", sandbox)
		end

		-- escalation request (sandbox_permissions + justification travel together)
		local scope = args_scope
		local needs, request = false, nil
		local reused_scope = nil -- a request fully inside an approved scope (logged below)
		if scope ~= nil or why ~= nil then
			local function refuse(reason)
				record("result", {
					tool = call.name,
					command = cmd_text,
					scope = scope,
					decision = "deny",
					source = "policy",
					reason = reason,
				})
				return { decision = "deny", reason = reason }
			end
			if type(scope) ~= "string" or scope == "" then
				return refuse("justification is only valid together with sandbox_permissions")
			end
			if type(why) ~= "string" or not why:match("%S") then
				return refuse("sandbox_permissions requires a justification")
			end
			local s, err, wider, inc = widen(scope, cwd)
			if s == nil and err then
				return refuse(err)
			end
			needs = wider
			if inc then
				if wider then
					-- inside an already approved scope (this turn / this session): no question;
					-- otherwise only the part outside it is asked about
					request = inc_remainder(inc, granted)
					needs = request ~= nil
					if not needs then
						reused_scope = scope
					end
					sandbox = E.apply_inc(base, inc_union(granted, inc))
				elseif E.inc_empty(granted) then
					sandbox = s
				end
			end
		end

		local shell = repr and repr.shell or (mare.os == "windows" and "powershell" or "bash")
		local prefix = prefix_of(argv, shell)
		-- "this session" approvals key on the full command (every segment's whole argv), the
		-- directory and the requested scope: approving
		-- `git push` doesn't approve `git push --force`, nor the same command elsewhere. Other
		-- tools key on their exact arguments: one approved edit never approves all edits (that
		-- is the separate "always allow" option below)
		local key_cmd
		if sv and sv.parsed.analyzable and not sv.parsed.script then
			local ps = {}
			for _, sg in ipairs(sv.segs) do
				ps[#ps + 1] = tostring(sg.op or "") .. "\0" .. table.concat(norm_argv(sg.argv, false, shell), "\0")
			end
			key_cmd = table.concat(ps, " ; ")
		elseif repr and repr.kind == "argv" and type(repr.argv) == "table" and not missing_repr then
			key_cmd = "=" .. mare.json.encode(repr.argv)
		elseif repr and repr.text and not missing_repr then
			key_cmd = "=" .. tostring(repr.text)
		else
			key_cmd = "=" .. mare.json.encode(args)
		end
		-- the key names the tool implementation too (the pipeline's definition version): a
		-- same-named replacement, e.g. a peer's unconfined `write`, is asked about afresh
		local tool_id = call.name .. "@" .. tostring(info.tool_version or "")
		local repr_key = repr and (tostring(repr.kind or "") .. ":" .. tostring(repr.shell or "")) or ""
		local key = tool_id .. "|" .. repr_key .. "|" .. tostring(cwd) .. "|" .. key_cmd .. "|" .. tostring(scope or "")
		-- Reuse an explicit grant of unrestricted execution, never an ordinary allow rule or
		-- a cached call that did not itself request all access.
		local approved_all = (scope == "all" or scope == "danger-full-access")
			and (
				P.session_cache[key]
				or (rule and rule.decision == "allow" and (rule.scope == "all" or rule.scope == "danger-full-access"))
			)
		-- read-only promises "no network without asking"; where the backend can't block the
		-- network (Windows default tier, accepted as partial) approval has to keep that promise:
		-- every shell or command tool asks unless this exact unrestricted access was approved.
		local net_note = preset == "read-only"
			and info.command_tool == true
			and is_network_partial()
			and not approved_all
			and not (sandbox and sandbox.network == true)
			and "network is not controlled; this command may access the network"
		local function ask(reason, category)
			if net_note then
				reason = reason and (reason .. " · " .. net_note) or net_note
			end
			local text = ("%s%s · %s%s · cwd %s%s"):format(
				source_tag(),
				label_of(call, repr),
				request and ("requests " .. inc_text(request))
					or scope and ("requests " .. scope)
					or (info.unconfined and "unconfined tool" or "write"),
				why and (" · because: " .. why) or "",
				-- one display rule for directories: `~` for home; the dialog
				-- title shortens the rest to its width
				mare.ui.display_path(cwd),
				reason and (" · " .. reason) or ""
			)
			local st = segments_text(sv)
			if st then
				text = text .. " · " .. st
			end
			if scope and scope:match("%.git") then
				text = text .. " · careful: write access to .git allows changing hooks that run outside the sandbox"
			elseif request and type(request.write) == "table" then
				for _, dir in ipairs(request.write) do
					if not E.is_dot_git_path(dir) and mare.fs.stat(dir .. "/.git") ~= nil then
						text = text
							.. " · note: .git inside granted directory stays read-only unless write:<repo>/.git is granted"
						break
					end
				end
			end
			if scope and not (request and request.all) then
				text = text
					.. " · scope: Allow once = this call only; Allow for this session = reused until session or permissions change"
			end
			local amendment
			if info.unconfined then
				amendment = { tool = call.name, decision = "allow", description = "always allow " .. call.name }
			elseif not info.command_tool then
				-- an explicit, separate choice — never implied by approving one edit
				amendment = {
					tool = call.name,
					decision = "allow",
					expires = "session",
					description = ("allow all %s calls this session"):format(call.name),
				}
			elseif prefix then
				amendment = {
					argv = prefix,
					deny_args = DEFAULT_DENY_ARGS,
					decision = "allow",
					scope = scope,
					cwd = cwd,
					expires = "never",
					description = ("always allow `%s`%s"):format(
						table.concat(prefix, " "),
						scope and (" (" .. scope .. ")") or ""
					),
				}
			end
			record("request", { tool = call.name, command = cmd_text, scope = scope, justification = why })
			return {
				decision = "ask",
				category = category or "sandbox",
				reason = text,
				sandbox = sandbox,
				amendment = amendment,
				key = key,
				request = request,
			}
		end

		-- read-only: an approved write runs with the workspace writable, this call only; nothing
		-- else (network, reads) widens with it. Decided before the rule and session checks, so
		-- a repeat covered by "allow for this session" / a remembered rule runs the same way
		-- as the call that was approved (instead of hitting the read-only sandbox)
		local granted_write = sandbox
			and type(args.path) == "string"
			and mare.proc.sandbox.check("write", mare.fs.resolve(args.path), sandbox) == true
		if
			not granted_write
			and preset == "read-only"
			and (
				info.side_effect == "write" or (rule and rule.decision == "allow" and rule.path_rule and rule.has_write)
			)
		then
			sandbox = base and E.apply_inc(base, inc_union(granted, { write = { E.workspace_root() } })) or nil
		end

		if sensitive_reason then
			local amendment
			if
				#sensitive == 1
				and not sensitive[1].shell
				and not sensitive[1].recursive
				and not (rule and rule.decision == "prompt")
				and not needs
				and mare.fs.path_within(sensitive[1].logical, sensitive[1].physical)
				and mare.fs.path_within(sensitive[1].physical, sensitive[1].logical)
			then
				local item = sensitive[1]
				amendment = {
					tool = call.name,
					path = item.logical,
					op = item.op,
					decision = "allow",
					expires = "never",
					description = "allow this tool and file",
				}
			end
			local categories = { "tool" }
			local reason = (repr and repr.dynamic)
					and ("%s%s · %s"):format(source_tag(), label_of(call, repr), sensitive_reason)
				or sensitive_reason
			if rule and rule.decision == "prompt" then
				categories[#categories + 1] = "rules"
				reason = reason
					.. " · "
					.. (
						unresolved and "a permission rule may apply to this command, which could not be analyzed"
						or "a rule asks before this command"
					)
			end
			if
				needs
				or net_note
				or (
					not granted_write
					and preset == "read-only"
					and info.side_effect == "write"
					and not (rule and rule.decision == "allow")
				)
			then
				categories[#categories + 1] = "sandbox"
				if request then
					reason = reason .. " · requests " .. inc_text(request)
				elseif scope and needs then
					reason = reason .. " · requests " .. scope
				end
				if why and needs then
					reason = reason .. " · because: " .. why
				end
				if net_note then
					reason = reason .. " · " .. net_note
				end
			end
			record("request", { tool = call.name, command = cmd_text, scope = scope, justification = why })
			return {
				decision = "ask",
				category = "tool",
				categories = categories,
				reason = reason,
				amendment = amendment,
				sandbox = sandbox,
				request = request,
			}
		end

		local function rule_covers_needs()
			if not needs then
				return true
			end
			if type(rule.scope) ~= "string" or rule.scope == "" then
				return false
			end
			local s, _, wider, rinc = widen(rule.scope, cwd)
			if s == nil or not wider or not rinc or not request then
				return false
			end
			return inc_remainder(request, rinc) == nil
		end

		-- rule allow / prompt
		if
			rule
			and rule.decision == "allow"
			and not net_note
			and not missing_repr
			and (sv == nil or sv.parsed.analyzable)
			and (rule.tool_version == nil or rule.tool_version == tostring(info.tool_version or ""))
			and rule_covers_needs()
		then
			if not needs and rule.scope then
				local s, _, wider_r, rinc = widen(rule.scope, cwd)
				if wider_r and rinc then
					sandbox = E.apply_inc(base, inc_union(granted, rinc))
				elseif E.inc_empty(granted) and s ~= nil then
					sandbox = s
				end
			end
			local v = auto_allow("rule", sandbox, { rule = rule.description or rule.argv or rule.tool })
			-- decided by this agent's own session-only rules (they never reach other agents)
			for _, r in ipairs(P.session_rules) do
				v.own_rule = v.own_rule or r == rule
				for _, pr in ipairs(rule.rules or {}) do
					v.own_rule = v.own_rule or pr == r
				end
				for _, sg in ipairs(sv and sv.segs or {}) do
					v.own_rule = v.own_rule or sg.rule == r
				end
			end
			return v
		end
		if P.session_cache[key] and not net_note and (not needs or approved_all) then
			return auto_allow("session approval", sandbox)
		end
		if rule and rule.decision == "prompt" then
			return ask(
				unresolved and "a permission rule may apply to this command, which could not be analyzed"
					or "a rule asks before this command",
				"rules"
			)
		end
		if info.unconfined then
			return ask("not confined by the local sandbox", "tool")
		end
		if missing_repr then
			return ask(repr.reason or "command representation is not analyzable", "tool")
		end
		if needs or net_note then
			return ask()
		end
		if not granted_write and preset == "read-only" and info.side_effect == "write" then
			return ask() -- with the workspace writable for this call (above)
		end
		if reused_scope then
			return auto_allow("approved scope", sandbox, { scope = reused_scope })
		end
		return { decision = "allow", sandbox = sandbox }
	end

	return { vote = vote }
end
