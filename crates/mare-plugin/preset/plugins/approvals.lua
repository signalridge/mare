-- Independent approval policy. Sandbox policy still determines what requires approval.
return {
	name = "approvals",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "approvals" },
	inject = { "bus", "policy", "task" },
	config_schema = {
		policy = {
			type = "union",
			default = "ask",
			env = "MARE_APPROVAL_POLICY",
			apply = "live",
			-- a wrong restrictive policy must not fall back to `ask`: the plugin, an
			-- essential service, refuses to start
			on_invalid = "fail",
			desc = "Approval policy: ask, never, untrusted, or a granular table.",
			variants = {
				{ type = "enum", choices = { "ask", "never", "untrusted" } },
				{
					type = "object",
					fields = {
						mode = { type = "enum", choices = { "granular" } },
						categories = { type = "map", values = { type = "boolean" } },
					},
				},
			},
		},
		reviewer = {
			type = "string",
			default = "user",
			desc = 'Name of the approval reviewer; "user" asks the user.',
		},
		review_timeout_ms = { type = "integer", default = 30000, desc = "Time an automatic reviewer has to decide." },
		fallback = {
			type = "boolean",
			default = false,
			desc = "Fall back to asking the user when the automatic reviewer fails.",
		},
		review_all = { type = "boolean", default = false, desc = "Require review of every request." },
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "session", "agent" })
		local copy = require("mare_lib").copy_deep
		local policy, revision, reviewers = nil, 0, {}
		local incarnation = mare.crypto.random_hex(16)
		local selected_reviewer = mare.mode == "child" and "user" or cfg.reviewer
		local review_required = cfg.review_all or (mare.args and mare.args.approval_review_required) == true
		assert(
			math.type(cfg.review_timeout_ms) == "integer"
				and cfg.review_timeout_ms > 0
				and cfg.review_timeout_ms <= 3600000,
			"review timeout must be positive"
		)
		local function validate(value)
			if value == "ask" or value == "never" or value == "untrusted" then
				return copy(value)
			end
			if type(value) ~= "table" or value.mode ~= "granular" or type(value.categories) ~= "table" then
				error("approval policy must be ask, never, untrusted, or {mode='granular', categories={...}}")
			end
			for key in pairs(value) do
				if key ~= "mode" and key ~= "categories" then
					error("unknown approval policy field: " .. tostring(key))
				end
			end
			for key, allowed in pairs(value.categories) do
				if type(key) ~= "string" or key == "" or type(allowed) ~= "boolean" then
					error("invalid approval category")
				end
			end
			return copy(value)
		end
		local ceilings = copy(mare.args and mare.args.approval_policy_ceilings or {})
		for i, value in ipairs(ceilings) do
			ceilings[i] = validate(value)
		end
		local inherited_untrusted, inherited_never = false, false
		for _, value in ipairs(ceilings) do
			inherited_untrusted = inherited_untrusted or value == "untrusted"
			inherited_never = inherited_never or value == "never"
		end
		-- explicit CLI policy snapshotted from the validated `cli` config layer
		local function explicit_cli_policy()
			if mare.config_cli then
				return mare.config_cli("plugins.approvals.policy")
			end
			return nil
		end
		-- the default a new or unrecorded session starts from: the command line over the current
		-- `plugins.approvals.policy` (a runtime write changes `cfg` in place, so this is read anew)
		local function default()
			return validate(explicit_cli_policy() or cfg.policy)
		end
		local function changed()
			revision = revision + 1
			ctx.bus:emit("approval/changed", { policy = copy(policy), revision = revision })
		end
		local function restore()
			local session = svc.session
			local records = session and require("mare_lib").all_records(session, "approval/policy") or {}
			local last = records[#records]
			policy = validate(explicit_cli_policy() or (last and last.policy) or default())
			changed()
		end
		local function set(value)
			value = validate(value)
			-- Commit before changing effective state. Failed writes cannot authorize anything.
			if svc.session then
				local ok, err = pcall(svc.session.add_record, svc.session, "approval/policy", { policy = value }, {
					persist = true,
				})
				if not ok then
					error("approval policy could not be saved: " .. tostring(err))
				end
			end
			policy = value
			changed()
			return copy(policy)
		end
		restore()
		ctx:on("session/loaded", restore)
		ctx:on("session/new", function()
			policy = default()
			changed()
		end)
		-- the live policy (a `/approval` or session choice can differ from the config): what a
		-- refused runtime write restores and a dotted write builds on
		ctx:on("option/current", function(ev)
			if ev.name == "plugins.approvals.policy" then
				return copy(policy)
			end
		end)
		ctx:on("option/apply", function(ev)
			-- a runtime write of `plugins.approvals.policy` is applied here and waited for: a failed
			-- save fails the write
			if ev.name == "plugins.approvals.policy" then
				set(ev.value)
			end
		end)
		local function gate(category)
			for _, ceiling in ipairs(ceilings) do
				if ceiling == "never" or (type(ceiling) == "table" and ceiling.categories[category] ~= true) then
					return {
						decision = "deny",
						reason = "inherited approval policy rejects this category",
						source = "approval_policy",
					}
				end
			end
			if policy == "never" then
				return {
					decision = "deny",
					reason = "approval policy never rejects this request",
					source = "approval_policy",
				}
			end
			if type(policy) == "table" and policy.categories[category] ~= true then
				return {
					decision = "deny",
					reason = "approval policy disables category " .. category,
					source = "approval_policy",
				}
			end
			return nil
		end
		local function valid_review(value)
			if type(value) ~= "table" or (value.decision ~= "allow" and value.decision ~= "deny") then
				return false
			end
			if value.reason ~= nil and type(value.reason) ~= "string" then
				return false
			end
			for key in pairs(value) do
				if key ~= "decision" and key ~= "reason" then
					return false
				end
			end
			return true
		end
		local function check_admission(cancel)
			if selected_reviewer == "user" then
				return true
			end
			local entry = reviewers[selected_reviewer]
			if not entry then
				return nil, "automatic approval reviewer unavailable"
			end
			if entry.admit then
				local ok, admitted, reason = pcall(entry.admit, cancel)
				if not ok then
					return nil, tostring(admitted)
				end
				if admitted ~= true then
					return nil, tostring(reason or "automatic approval reviewer admission failed")
				end
			end
			return true
		end
		local function deadline(cancel)
			if not mare.task.sleep(cfg.review_timeout_ms, cancel) then
				return "cancelled"
			end
			return "timeout"
		end
		local function admit(cancel)
			if cancel and cancel:is_cancelled() then
				return nil, "automatic approval admission cancelled"
			end
			local _, ok, reason = mare.task.select(function()
				return check_admission(cancel)
			end, function()
				return nil, "automatic approval admission " .. deadline(cancel)
			end)
			if cancel and cancel:is_cancelled() then
				return nil, "automatic approval admission cancelled"
			end
			return ok, reason
		end
		local function is_allow(d)
			return d == "allow" or d == "allow_session" or d == "allow_amend"
		end
		local function normalize_result(result)
			if type(result) ~= "table" then
				return { decision = "unavailable", reason = "invalid approval answer", source = "error" }
			end
			if result.abort ~= nil and type(result.abort) ~= "boolean" then
				return { decision = "deny", reason = "invalid approval abort flag", source = "approval_policy" }
			end
			if result.unavailable ~= nil and type(result.unavailable) ~= "boolean" then
				return { decision = "deny", reason = "invalid approval unavailable flag", source = "approval_policy" }
			end
			if result.abort and result.unavailable then
				return { decision = "deny", reason = "contradictory approval flags", source = "approval_policy" }
			end
			local d = result.decision
			if result.grant ~= nil then
				if d ~= "allow" or not is_allow(result.grant) then
					return { decision = "deny", reason = "invalid approval grant", source = "approval_policy" }
				end
				d = result.grant
			end
			if is_allow(d) and (result.abort or result.unavailable) then
				return { decision = "deny", reason = "contradictory approval answer", source = "approval_policy" }
			end
			if result.unavailable then
				d = "unavailable"
			elseif result.abort then
				d = "abort"
			end
			local outcomes = {
				allow = true,
				allow_session = true,
				allow_amend = true,
				deny = true,
				abort = true,
				unavailable = true,
			}
			if not outcomes[d] then
				return { decision = "unavailable", reason = "invalid approval answer", source = "error" }
			end
			result.decision = d
			result.grant = nil
			return result
		end
		local function apply_constraints(result, restrictions)
			if type(result) ~= "table" or type(restrictions) ~= "table" then
				return result
			end
			for _, field in ipairs({ "sandbox_constraints", "permission_constraints" }) do
				local constraints = restrictions[field == "sandbox_constraints" and "sandboxes" or field] or {}
				if #constraints > 0 then
					result[field] = result[field] or {}
					for _, value in ipairs(constraints) do
						result[field][#result[field] + 1] = copy(value)
					end
				end
			end
			if restrictions.scope == "turn" then
				result.scope = "turn"
			end
			return result
		end
		local function check_gates(category, action)
			local blocked = gate(category)
			if action.info and action.info.permission_request then
				blocked = blocked or gate("request_permissions")
			end
			for _, extra in ipairs(action.verdict and action.verdict.categories or {}) do
				blocked = blocked or gate(extra)
			end
			return blocked
		end
		local function decide(category, action, policy_check, answer, cancel, binding)
			assert(type(category) == "string" and category ~= "", "approval category required")
			local human_required = action.call and action.call.protected_arguments == true
			if cancel and cancel:is_cancelled() then
				return { decision = "deny", reason = "approval cancelled", source = "cancelled", outcome = "cancelled" }
			end
			local blocked = check_gates(category, action)
			if blocked then
				return blocked
			end
			local restrictions = policy_check(copy(binding))
			assert(
				type(restrictions) == "table"
					and (
						restrictions.decision == "allow"
						or restrictions.decision == "deny"
						or restrictions.decision == "ask"
					),
				"invalid mandatory approval policy result"
			)
			if restrictions.decision == "deny" then
				return {
					decision = restrictions.unavailable and "unavailable" or restrictions.abort and "abort" or "deny",
					reason = restrictions.reason,
					source = "policy",
				}
			end
			local function human_answer()
				local answer_result = answer(copy(binding))
				local result = normalize_result(copy(answer_result))
				local decision = result.decision
				local human = mare.approvals.check_human(
					answer_result and answer_result.human_approval,
					binding,
					action.call and action.call.arguments,
					decision
				)
				local receipt = human
						and { token = answer_result.human_approval, binding = copy(binding), decision = decision }
					or nil
				result.human_approval = nil -- receipt travels separately; result is always safe to audit/emit
				if human then
					result.source = "user"
				elseif result.source == "user" then
					result.source = "policy"
				end
				if human_required and is_allow(result.decision) and not human then
					return {
						decision = "deny",
						reason = "hidden values require human approval for this exact action",
						source = "approval_policy",
					}
				end
				return result, receipt
			end
			if selected_reviewer == "user" or human_required then
				return human_answer()
			end
			local entry = reviewers[selected_reviewer]
			local reviewed_at = revision
			local result
			if not entry then
				result = {
					decision = "unavailable",
					reason = "automatic approval reviewer unavailable",
					source = "reviewer",
				}
			else
				-- Each reviewer gets a detached snapshot, never the pipeline's execution arguments.
				local which, ok, value = mare.task.select(function()
					local admitted, reason = check_admission(cancel)
					if not admitted then
						return false, reason
					end
					if cancel and cancel:is_cancelled() then
						return false, "approval cancelled"
					end
					if reviewers[selected_reviewer] ~= entry or revision ~= reviewed_at then
						return false, "approval policy or reviewer changed during admission"
					end
					return pcall(entry.review, copy(action), cancel)
				end, function()
					return deadline(cancel)
				end)
				if which == 2 then
					result = {
						decision = "deny",
						reason = "automatic approval review " .. tostring(ok),
						source = "reviewer",
						outcome = ok,
					}
				elseif not ok or not valid_review(value) then
					result = {
						decision = "unavailable",
						reason = "invalid automatic approval review result",
						source = "reviewer",
					}
				else
					result = { decision = value.decision, reason = value.reason, source = "reviewer" }
				end
				if reviewers[selected_reviewer] ~= entry or revision ~= reviewed_at then
					result = {
						decision = "deny",
						reason = "approval policy or reviewer changed during review",
						source = "reviewer",
					}
				end
			end
			if cancel and cancel:is_cancelled() then
				return { decision = "deny", reason = "approval cancelled", source = "cancelled", outcome = "cancelled" }
			end
			ctx.bus:emit("pipeline/approval-reviewed", {
				category = category,
				snapshot = action.call and action.call.snapshot,
				result = mare.text.redact(copy(result)),
			})
			local audit = mare.approvals.public_result({
				call = action.call and { tool_call_id = action.call.id, name = action.call.name }
					or { tool_call_id = binding.request, name = category },
				decision = result.decision,
				source = result.source,
				outcome = result.outcome,
			})
			ctx.bus:emit("approval/review-result", { category = category, result = audit })
			-- Fallback is only for reviewer unavailability, never for a rejection or timeout.
			if result.decision == "unavailable" and cfg.fallback then
				return human_answer()
			end
			return result
		end
		local function current_turn()
			local agent = svc.agent
			local state = agent and agent:activity()
			return state and state.turn_id or "manual"
		end
		local function request(category, action, policy_check, answer, cancel, opts)
			assert(
				type(policy_check) == "function" and type(answer) == "function",
				"approval requires separate policy and answer callbacks"
			)
			local captured = revision
			local function record(kind, value)
				if svc.session then
					local ok, err = pcall(svc.session.add_record, svc.session, kind, value)
					if not ok then
						error("approval audit could not be saved: " .. tostring(err))
					end
				end
			end
			local opts_tbl = type(opts) == "table" and opts or nil
			local emit_resolved = not (opts_tbl and opts_tbl.emit_resolved == false)
			local id = (opts_tbl and type(opts_tbl.approval_id) == "string" and opts_tbl.approval_id ~= "")
					and opts_tbl.approval_id
				or mare.crypto.random_hex(16)
			local call_id = action.call and action.call.id or nil
			local snapshot = action.call and action.call.snapshot or nil
			if snapshot == nil and category == "network" and action.host ~= nil then
				snapshot = { host = action.host, port = action.port }
			end
			local child_approval_id = opts_tbl and opts_tbl.child_approval_id or nil
			local binding = {
				owner = incarnation,
				turn = current_turn(),
				request = id,
				snapshot = action.call and action.call.snapshot or mare.crypto.sha256(mare.json.encode(action)),
			}
			local public_call = { tool_call_id = call_id or "", name = (action.call and action.call.name) or category }
			local function emit_resolved_event(event)
				ctx.bus:emit("approval/resolved", {
					approval_id = id,
					category = category,
					tool_call_id = call_id,
					result = mare.approvals.public_result(event),
				})
			end
			record("approval/requested", {
				approval_id = id,
				category = category,
				tool_call_id = call_id,
				snapshot = snapshot,
				child_approval_id = child_approval_id,
			})
			if opts_tbl then
				opts_tbl.approval_id = id
				opts_tbl.requested = true
			end
			local resolved_emitted = false
			-- Ensure default-emit callers (e.g. mcp_elicitation) pair hard cancellations with resolved.
			local resolved_guard <close> = setmetatable({}, {
				__close = function(_, err)
					if emit_resolved and not resolved_emitted then
						resolved_emitted = true
						local cancelled = (cancel and cancel:is_cancelled()) or err == nil
						local fallback = {
							decision = "deny",
							reason = cancelled and "approval cancelled" or tostring(err),
							source = cancelled and "cancelled" or "error",
							outcome = cancelled and "cancelled" or nil,
						}
						pcall(ctx.spawn, ctx, function()
							if svc.session then
								pcall(svc.session.add_record, svc.session, "approval/resolved", {
									approval_id = id,
									category = category,
									tool_call_id = call_id,
									child_approval_id = child_approval_id,
									result = fallback,
								})
							end
							emit_resolved_event({
								call = public_call,
								decision = fallback.decision,
								source = fallback.source,
								outcome = fallback.outcome,
								child_approval_id = child_approval_id,
							})
						end)
					end
				end,
			})
			ctx.bus:emit("approval/requested", {
				approval_id = id,
				category = category,
				tool_call_id = call_id,
				snapshot = snapshot ~= nil and mare.text.redact(snapshot) or nil,
				child_approval_id = child_approval_id,
			})
			local snapshot_lease <close> = mare.approvals.register_snapshot(binding, action)
			local ok, result, receipt = pcall(decide, category, action, policy_check, answer, cancel, binding)
			snapshot_lease:release()
			if not ok then
				result = { decision = "unavailable", reason = tostring(result), source = "error" }
			end
			result = normalize_result(result)
			if is_allow(result.decision) then
				local post_blocked = check_gates(category, action)
				if post_blocked then
					result = post_blocked
				else
					local pok, post_restrictions = pcall(policy_check, copy(binding))
					if
						not pok
						or type(post_restrictions) ~= "table"
						or (
							post_restrictions.decision ~= "allow"
							and post_restrictions.decision ~= "deny"
							and post_restrictions.decision ~= "ask"
						)
					then
						result = {
							decision = "deny",
							reason = pok and "invalid mandatory approval policy result" or tostring(post_restrictions),
							source = "policy",
						}
					elseif post_restrictions.decision == "deny" then
						result = {
							decision = post_restrictions.unavailable and "unavailable"
								or post_restrictions.abort and "abort"
								or "deny",
							reason = post_restrictions.reason or "denied by policy",
							source = "policy",
						}
					else
						result = apply_constraints(result, post_restrictions)
					end
				end
			end
			if captured ~= revision or binding.turn ~= current_turn() then
				result = {
					decision = "deny",
					reason = "approval policy or request owner changed while waiting",
					source = "approval_policy",
				}
			end
			if cancel and cancel:is_cancelled() then
				result =
					{ decision = "deny", reason = "approval cancelled", source = "cancelled", outcome = "cancelled" }
			end
			local rec_ok, rec_err = pcall(record, "approval/resolved", {
				approval_id = id,
				category = category,
				tool_call_id = call_id,
				child_approval_id = child_approval_id,
				result = result,
			})
			if not rec_ok then
				result = {
					decision = "deny",
					reason = tostring(rec_err),
					source = "error",
				}
			end
			if emit_resolved then
				resolved_emitted = true
				emit_resolved_event({
					call = public_call,
					decision = result.decision,
					source = result.source,
					outcome = result.outcome,
					child_approval_id = child_approval_id,
					scope = result.scope,
				})
			end
			if not rec_ok then
				error(rec_err)
			end
			if not is_allow(result.decision) then
				receipt = nil
			end
			return result, receipt
		end
		require("mare_lib").service(ctx, "approvals", {
			delegation_services = function()
				return { "approvals" }
			end,
			review_required = function()
				return review_required
			end,
			inheritance = function()
				local out = copy(ceilings)
				out[#out + 1] = copy(policy)
				return out
			end,
			policy = function()
				return copy(policy)
			end,
			version = function()
				return incarnation .. ":" .. tostring(revision)
			end,
			revision = function()
				return revision
			end,
			set = function(_, _, value)
				return set(value)
			end,
			gate = function(_, _, category)
				return gate(category)
			end,
			request = function(_, _, category, action, policy_check, answer, cancel, opts)
				return request(category, action, policy_check, answer, cancel, opts)
			end,
			admit = function(_, _, cancel)
				return admit(cancel)
			end,
			register_reviewer = function(_, caller, name, review, opts)
				assert(type(name) == "string" and name ~= "user" and type(review) == "function", "invalid reviewer")
				assert(not reviewers[name], "reviewer already registered")
				assert(
					not opts or not opts.admit or type(opts.admit) == "function",
					"reviewer admission must be a function"
				)
				local entry = { review = review, admit = opts and opts.admit }
				reviewers[name] = entry
				changed()
				local function remove()
					if reviewers[name] == entry then
						reviewers[name] = nil
						changed()
					end
				end
				return require("mare_lib").owned(caller, remove, "approval reviewer " .. name)
			end,
		})
		-- Untrusted is conservative: only declared read operations bypass an extra ask.
		-- Explicit deny votes elsewhere still win; no tool/provider names are special.
		local policy_svc = ctx.policy
		policy_svc:voter("tool/before", function(_, info)
			if
				(review_required or policy == "untrusted" or inherited_untrusted)
				and (review_required or not info or info.side_effect ~= "read" or info.unconfined)
			then
				return { decision = "ask", reason = "approval policy requires review" }
			end
		end)
		policy_svc:voter("request/ready", function()
			if review_required then
				local ok, reason = admit()
				if not ok then
					return { decision = "deny", reason = reason }
				end
			end
		end)
		require("mare_lib").prompt_section(ctx, {
			name = "approval-policy",
			order = 10011,
			interpolate = false,
			text = function()
				local name = type(policy) == "table" and "granular" or policy
				return "Approval policy: "
					.. name
					.. (#ceilings > 0 and ("; inherited constraints: " .. mare.json.encode(ceilings)) or "")
					.. (review_required and "; every tool call requires review" or "")
					.. ". "
					.. (
						(policy == "never" or inherited_never)
							and "Prompts are disabled; operations requiring approval are rejected. Do not request sandbox escalation."
						or "Approval requests require an available answerer and may be refused."
					)
			end,
		})
		ctx:inject({ "commands" }, function(sub)
			local commands = sub.commands
			commands:register("approval", {
				description = "Independent approval policy: ask / never / untrusted",
				run = function(args)
					args = (args or ""):match("^%s*(.-)%s*$")
					if args == "" then
						mare.ui.notify("Approval policy: " .. mare.json.encode(policy), "info")
					else
						set(args)
					end
				end,
			})
		end)
	end,
}
