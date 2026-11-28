-- tools.approval: policy version, snapshot hashing and the one-at-a-time approval prompt;
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local registry = D.registry.registry

	-- bumped whenever the effective permission policy may have changed (preset switch, rule
	-- edits): approvals given under an older policy are void
	local policy_gen = 0
	ctx:on("approval/changed", function()
		policy_gen = policy_gen + 1
	end)
	ctx:on("permission/changed", function()
		policy_gen = policy_gen + 1
	end)

	--- the policy version: policy, sandbox, and host config/plugin reload state
	local function policy_version()
		local reload = mare.reload_version and mare.reload_version() or ""
		local permissions = D.svc.permissions
		local revision = permissions and permissions.policy_revision and permissions:policy_revision() or 0
		local approvals = D.svc.approvals
		local approval_version = approvals and approvals:version() or "disabled"
		return tostring(policy_gen)
			.. ":"
			.. tostring(revision)
			.. ":"
			.. approval_version
			.. ":"
			.. tostring(mare.proc.sandbox.generation())
			.. ":"
			.. reload
			.. ":"
			.. tostring(mare.projection.revision())
			.. ":"
			.. tostring(D.policy:epoch())
	end

	--- canonical serialization for hashing: sorted keys, exact numbers. Not mare.json.encode
	--- (which sorts keys too): encode turns NaN/±inf into `null` (so they'd hash like nil) and
	--- rewrites `properties` next to an `x-mare-order` key, dropping the hint — a change made
	--- after approval in either place would go unnoticed. This keeps every value distinct.
	local function canon(v, out)
		local t = type(v)
		if t == "table" then
			local n = #v
			local keys = {}
			for k in pairs(v) do
				keys[#keys + 1] = k
			end
			if n > 0 and #keys == n then
				out[#out + 1] = "["
				for i = 1, n do
					canon(v[i], out)
					out[#out + 1] = ","
				end
				out[#out + 1] = "]"
			else
				table.sort(keys, function(a, b)
					return tostring(a) < tostring(b)
				end)
				out[#out + 1] = "{"
				for _, k in ipairs(keys) do
					out[#out + 1] = ("%q:"):format(tostring(k))
					canon(v[k], out)
					out[#out + 1] = ","
				end
				out[#out + 1] = "}"
			end
		elseif t == "string" then
			out[#out + 1] = ("%q"):format(v)
		elseif t == "number" then
			out[#out + 1] = math.type(v) == "integer" and tostring(v) or ("%.17g"):format(v)
		else
			out[#out + 1] = tostring(v)
		end
		return out
	end
	local function snapshot_hash(snap)
		return mare.crypto.sha256(table.concat(canon({
			name = snap.name,
			arguments = mare.json.encode_strict(snap.arguments),
			cwd = snap.cwd,
			sandbox = snap.sandbox or false,
			policy = snap.policy_version,
			tool = snap.tool_version,
			schema = snap.schema,
			projection = snap.projection_version,
			inputs = snap.input_policy,
			access = snap.access,
			command = snap.command,
		}, {})))
	end

	local function resolve_command_display(call, info)
		local args = type(call and call.arguments) == "table" and call.arguments or {}
		local spec = type(info) == "table" and info.command or nil
		if type(spec) == "table" then
			if type(spec.shell) == "string" and type(spec.arg) == "string" then
				local shell = spec.shell == "pwsh" and "powershell" or spec.shell
				local raw = args[spec.arg]
				if type(raw) == "string" and raw ~= "" then
					return raw, shell
				end
				return nil, shell
			elseif type(spec.argv) == "table" then
				local ml = require("mare_lib")
				local argv
				if spec.dynamic then
					argv = ml.copy_deep(spec.argv)
				else
					argv = ml.expand_argv_template(spec.argv, spec.defaults, args)
				end
				if type(argv) == "table" and #argv > 0 then
					return table.concat(argv, " "), nil
				end
			end
		end
		if type(args.command) == "string" and args.command ~= "" then
			return args.command, nil
		end
		return nil, nil
	end

	local function resolve_location_path(raw_path, cwd)
		if type(raw_path) ~= "string" or raw_path == "" then
			return nil
		end
		if type(cwd) == "string" and cwd ~= "" and mare.fs.resolve_path then
			local ok, resolved = pcall(mare.fs.resolve_path, raw_path, cwd)
			if ok and type(resolved) == "table" and type(resolved.logical) == "string" and resolved.logical ~= "" then
				return resolved.logical
			end
		end
		if mare.fs.resolve then
			local ok, resolved = pcall(mare.fs.resolve, raw_path)
			if ok and type(resolved) == "string" and resolved ~= "" then
				return resolved
			end
		end
		return raw_path
	end

	local function flatten_spans(spans)
		if type(spans) == "string" or type(spans) == "number" then
			return tostring(spans)
		end
		if type(spans) ~= "table" then
			return ""
		end
		if spans.type == "span" then
			return tostring(spans.text or "")
		end
		if spans.type == "line" then
			return flatten_spans(spans.spans)
		end
		local parts = {}
		for _, sp in ipairs(spans) do
			if type(sp) == "string" or type(sp) == "number" then
				parts[#parts + 1] = tostring(sp)
			elseif type(sp) == "table" then
				if sp.text ~= nil then
					parts[#parts + 1] = tostring(sp.text)
				elseif sp.spans ~= nil then
					parts[#parts + 1] = flatten_spans(sp.spans)
				end
			end
		end
		return table.concat(parts)
	end

	--- Project a Lua `mare.Node` preview into JSON-safe structured blocks (`diff` | `text`)
	--- and record any diff target paths in `locations` so ACP, TUI, and RPC share one preview.
	local function project_preview_blocks(node, cwd, add_location)
		local blocks = {}
		local pending_lines = {}
		local function flush_text()
			if #pending_lines > 0 then
				local text = table.concat(pending_lines, "\n")
				pending_lines = {}
				if text:match("%S") then
					blocks[#blocks + 1] = { type = "text", text = text }
				end
			end
		end
		local function walk(n)
			if n == nil then
				return
			end
			if type(n) == "string" then
				pending_lines[#pending_lines + 1] = n
				return
			end
			if type(n) ~= "table" then
				return
			end
			local ty = n.type
			if ty == nil then
				for _, item in ipairs(n) do
					walk(item)
				end
			elseif ty == "diff" then
				local target = resolve_location_path(n.full_path or n.path, cwd)
				if target then
					add_location(target)
				end
				if type(n.unified) == "string" then
					if n.unified ~= "" then
						flush_text()
						blocks[#blocks + 1] = { type = "text", text = n.unified }
					end
				elseif type(n.new) == "string" and target ~= nil then
					flush_text()
					local old_for_diff = (n.is_new == true or type(n.old) ~= "string") and "" or n.old
					local uok, unified = pcall(mare.text.diff, old_for_diff, n.new, { path = n.path or target })
					local unified_text = (uok and type(unified) == "string" and unified ~= "") and unified or nil
					if n.is_new == true then
						blocks[#blocks + 1] =
							{ type = "diff", path = target, new = n.new, is_new = true, unified = unified_text }
					elseif type(n.old) == "string" then
						blocks[#blocks + 1] =
							{ type = "diff", path = target, old = n.old, new = n.new, unified = unified_text }
					else
						blocks[#blocks + 1] =
							{ type = "diff", path = target, new = n.new, is_new = true, unified = unified_text }
					end
				elseif type(n.new) == "string" and n.new ~= "" then
					pending_lines[#pending_lines + 1] = n.new
				end
			elseif ty == "lines" then
				for _, item in ipairs(n.items or {}) do
					walk(item)
				end
			elseif ty == "line" or ty == "paragraph" then
				pending_lines[#pending_lines + 1] = flatten_spans(n.spans)
			elseif ty == "span" then
				pending_lines[#pending_lines + 1] = tostring(n.text or "")
			elseif ty == "code" then
				if type(n.code) == "string" and n.code ~= "" then
					flush_text()
					blocks[#blocks + 1] = { type = "text", text = n.code }
				end
			elseif ty == "markdown" or ty == "presented_markdown" then
				if type(n.text) == "string" and n.text ~= "" then
					flush_text()
					blocks[#blocks + 1] = { type = "text", text = n.text }
				end
			elseif ty == "block" or ty == "indent" or ty == "clip" then
				walk(n.child)
			elseif ty == "row" then
				local row_parts = {}
				for _, item in ipairs(n.items or {}) do
					if type(item) == "string" then
						row_parts[#row_parts + 1] = item
					elseif type(item) == "table" and (item.type == "line" or item.type == "paragraph") then
						row_parts[#row_parts + 1] = flatten_spans(item.spans)
					elseif type(item) == "table" and item.type == "span" then
						row_parts[#row_parts + 1] = tostring(item.text or "")
					end
				end
				if #row_parts > 0 then
					pending_lines[#pending_lines + 1] = table.concat(row_parts, " ")
				end
			elseif ty == "gap" then
				pending_lines[#pending_lines + 1] = ""
			end
		end
		walk(node)
		flush_text()
		return blocks
	end

	local function project_value(value)
		if value == nil then
			return nil
		end
		local ok, projected = pcall(mare.projection.project, value)
		if ok then
			return projected
		end
		local ok_lossy, projected_lossy = pcall(mare.projection.project, value, { lossy = true })
		if ok_lossy then
			return projected_lossy
		end
		return nil
	end

	--- Compute one projected approval detail (command, preview, locations, scope) in the shared
	--- authorization path so TUI, ACP, and RPC render identical tool-derived facts.
	local function build_approval_detail(call, info, verdict)
		local copy = require("mare_lib").copy_deep
		local args = type(call and call.arguments) == "table" and call.arguments or {}
		local cmd_text, cmd_shell = resolve_command_display(call, info)

		local locations, seen_locs = {}, {}
		local function add_location(path)
			if type(path) == "string" and path ~= "" and not seen_locs[path] then
				seen_locs[path] = true
				locations[#locations + 1] = { path = path }
			end
		end
		if type(info) == "table" and info.access_state == "declared" and type(info.access) == "table" then
			for _, entry in ipairs(info.access) do
				if type(entry) == "table" and entry.kind == "file" then
					add_location(entry.logical or entry.path)
				end
			end
		end

		local def = registry[call.name]
		local raw_preview = nil
		local preview_blocks = {}
		local preview_shows_cwd = false
		if def and def.preview then
			local pok, pv = pcall(def.preview, copy(args), call.cwd)
			if pok and pv ~= nil then
				if type(pv) == "table" and pv.includes_cwd == true then
					preview_shows_cwd = true
				end
				raw_preview = project_value(pv)
				if raw_preview ~= nil then
					preview_blocks = project_preview_blocks(raw_preview, call.cwd, add_location)
				end
			end
		end

		local scoped = (type(info) == "table" and info.permission_request == true)
			or (type(verdict) == "table" and verdict.permission_scope ~= nil)
		local session_allowed = not call.protected_arguments
			and (not scoped or (type(verdict) == "table" and verdict.permission_scope == "session"))
		local has_additional = (type(verdict) == "table" and type(verdict.requested_permissions) == "table")
			or type(args.additional_permissions) == "table"
		local raw_sb = type(args.sandbox_permissions) == "string" and args.sandbox_permissions:match("^%s*(.-)%s*$")
			or nil
		local reusable_sandbox = (raw_sb ~= nil and raw_sb ~= "" and raw_sb ~= "all" and raw_sb ~= "danger-full-access")
				and raw_sb
			or nil
		local session_kind
		if scoped then
			session_kind = call.origin ~= nil and "scoped_subagent" or "scoped"
		elseif has_additional then
			session_kind = "additional_permissions"
		elseif reusable_sandbox ~= nil then
			session_kind = "sandbox_permissions"
		elseif cmd_text ~= nil then
			session_kind = "exact_command"
		else
			session_kind = "exact_call"
		end

		local detail = {
			command = cmd_text,
			command_shell = cmd_shell,
			preview_shows_cwd = preview_shows_cwd,
			scope = {
				scoped = scoped,
				session_allowed = session_allowed,
				kind = session_kind,
				requested_access = reusable_sandbox,
				permission_scope = type(verdict) == "table" and verdict.permission_scope or nil,
			},
		}
		if #locations > 0 then
			detail.locations = locations
		end
		if #preview_blocks > 0 then
			detail.preview = preview_blocks
		end
		return raw_preview, project_value(detail)
	end

	--- approval for an `ask` verdict. Decisions:
	---   allow (once) · allow_session · allow_amend (+ `amendment`, a rule the permissions
	---   plugin persists) · deny · abort (deny and end the turn).
	--- tool/approval listeners vote (policy; an RPC host answers via `approval.request`);
	--- without any, an interactive UI asks the user; otherwise (headless) it's `unavailable`.
	local function approve_now(call, info, verdict, cancel, raw_preview)
		if D.policy:has_voters("tool/approval-host") then
			local v = D.policy:decide(
				"tool/approval-host",
				table.pack(call, info, verdict, cancel),
				{ require_explicit_approval = true }
			)
			if v.unavailable then
				return { decision = "unavailable", reason = v.reason, source = v.source or "user" }
			end
			if v.decision == "allow" then
				return {
					decision = v.grant or "allow",
					amendment = v.amendment,
					sandbox_constraints = v.sandboxes,
					permission_constraints = v.permission_constraints,
					scope = v.scope,
					source = v.source or "policy",
					human_approval = v.human_approval,
				}
			end
			return {
				decision = v.abort and "abort" or "deny",
				reason = v.reason or (v.decision == "ask" and "nobody approved the request" or nil),
				source = v.source or "user",
			}
		end
		if mare.ui.connected() then
			-- the answer keys are on the items (the hint bar keeps Ctrl+C / Esc in view)
			local scope = type(info) == "table"
					and type(info.approval_detail) == "table"
					and type(info.approval_detail.scope) == "table"
					and info.approval_detail.scope
				or nil
			local scoped = scope and scope.scoped == true
				or (info.permission_request or verdict.permission_scope ~= nil)
			local session_allowed = scope and scope.session_allowed == true
				or (not call.protected_arguments and (not scoped or verdict.permission_scope == "session"))
			local choices = { "allow" }
			if session_allowed then
				choices[#choices + 1] = "allow_session"
			end
			if verdict.amendment and not call.protected_arguments then
				choices[#choices + 1] = "allow_amend"
			end
			choices[#choices + 1] = "deny"
			choices[#choices + 1] = "abort"
			local d, human_approval = mare.ui.approve({
				approval_binding = call.approval_binding,
				approval_arguments = call.arguments,
				tool = tostring(call.name),
				choices = choices,
				amendment_description = verdict.amendment and tostring(verdict.amendment.description or "this rule")
					or nil,
				reason = verdict.reason,
				unconfined = info.unconfined,
				protected_arguments = call.protected_arguments,
				scoped = scoped,
				preview = raw_preview,
				cancel = cancel,
				wait_kind = "approval", -- ui/wait-start {kind = "approval"} (status, notify, status hooks)
				wait_title = ("Allow %s?"):format(tostring(call.name)),
			})
			if cancel and cancel:is_cancelled() then
				return {
					decision = "deny",
					reason = "approval cancelled",
					source = "cancelled",
				}
			end
			local answered = d ~= nil
			d = d or "deny"
			local reason = (d == "deny" or d == "abort") and "declined by the user" or nil
			-- an explicit Deny (not Esc, not Ctrl+C) asks why: the model reads the answer in the
			-- tool result and can change course ("No, and tell the model what to do
			-- differently"); Enter on an empty line or Esc skips it
			if d == "deny" and answered then
				local why = mare.ui.input({
					title = "Tell the model why (optional — Enter to skip)",
					placeholder = "e.g. use the staging database instead",
					cancel = cancel,
				})
				if cancel and cancel:is_cancelled() then
					return {
						decision = "deny",
						reason = "approval cancelled",
						source = "cancelled",
					}
				end
				why = type(why) == "string" and mare.text.sanitize(why):match("^%s*(.-)%s*$") or ""
				if why ~= "" then
					reason = "declined by the user: " .. why
				end
			end
			return {
				decision = d,
				amendment = d == "allow_amend" and verdict.amendment or nil,
				reason = reason,
				source = "user",
				human_approval = human_approval,
			}
		end
		-- nobody can answer → `unavailable`, treated as deny and recorded
		return {
			decision = "unavailable",
			reason = "approval required but nobody can answer (headless)",
			source = "none",
		}
	end
	local function granted(d)
		return d == "allow" or d == "allow_session" or d == "allow_amend"
	end
	local function normalize_answer(ans)
		if type(ans) ~= "table" then
			return { decision = "unavailable", reason = "invalid approval answer", source = "error" }
		end
		if ans.abort ~= nil and type(ans.abort) ~= "boolean" then
			return { decision = "deny", reason = "invalid approval abort flag", source = "approval_policy" }
		end
		if ans.unavailable ~= nil and type(ans.unavailable) ~= "boolean" then
			return { decision = "deny", reason = "invalid approval unavailable flag", source = "approval_policy" }
		end
		if ans.abort and ans.unavailable then
			return { decision = "deny", reason = "contradictory approval flags", source = "approval_policy" }
		end
		local d = ans.decision
		if ans.grant ~= nil then
			if d ~= "allow" or not granted(ans.grant) then
				return { decision = "deny", reason = "invalid approval grant", source = "approval_policy" }
			end
			d = ans.grant
		end
		if granted(d) and (ans.abort or ans.unavailable) then
			return { decision = "deny", reason = "contradictory approval answer", source = "approval_policy" }
		end
		if ans.unavailable then
			d = "unavailable"
		elseif ans.abort then
			d = "abort"
		end
		ans.decision = d
		ans.grant = nil
		return ans
	end
	-- one approval at a time: concurrent calls never stack prompts
	local approval_tx, approval_rx = mare.task.channel()
	approval_tx:send(true)
	local function approve(call, info, verdict, custom, cancel, version, declaration, provided_opts)
		if cancel then
			while not approval_rx:try_recv() do
				if not mare.task.sleep(25, cancel) then
					return { decision = "deny", reason = "approval cancelled", source = "cancelled" }
				end
			end
		else
			approval_rx:recv()
		end
		local released, owned = false, true
		local function release()
			if not released then
				released = true
				approval_tx:send(true)
			end
		end
		-- A cancelled Lua task closes this variable, even if the approval callback was yielding.
		local lease <close> = setmetatable({}, {
			__close = function()
				if owned then
					release()
				end
			end,
		})
		if cancel and cancel:is_cancelled() then
			return { decision = "deny", reason = "approval cancelled", source = "cancelled" }
		end
		if version and policy_version() ~= version then
			return { decision = "retry" }
		end
		local req_opts = provided_opts or {}
		req_opts.emit_resolved = false
		local ok, a, human_receipt = pcall(function()
			local copy = require("mare_lib").copy_deep
			-- The compiled declaration is private to the execution pipeline. Policy observers
			-- receive their own call.schema copy and cannot rewrite what the reviewer sees.
			local review_call = copy(call)
			review_call.schema = mare.projection.project(copy(declaration))
			local review_info = copy(info)
			local raw_preview, approval_detail = build_approval_detail(review_call, review_info, verdict)
			if approval_detail ~= nil then
				review_info.approval_detail = approval_detail
			end
			local service = D.svc.approvals
			local policy
			local function check_policy()
				policy = D.policy:decide(
					"tool/approval",
					table.pack(copy(review_call), copy(review_info), copy(verdict), cancel)
				)
				return policy
			end
			local answer = function(binding)
				call.approval_binding = copy(binding)
				local has_host = D.policy:has_voters("tool/approval-host")
				local review_required = service and service.review_required and service:review_required() == true
				if
					not has_host
					and not review_required
					and policy.decision == "allow"
					and not call.protected_arguments
					and not custom
				then
					return {
						decision = policy.grant or "allow",
						amendment = policy.amendment,
						sandbox_constraints = policy.sandboxes,
						permission_constraints = policy.permission_constraints,
						scope = policy.scope,
						source = policy.source or "policy",
					}
				end
				-- Forward the authoritative schema through ordinary user/RPC/child answerers too.
				call.schema = mare.projection.project(copy(declaration))
				if has_host or not custom then
					return approve_now(copy(call), copy(review_info), copy(verdict), cancel, raw_preview)
				end
				local ans = normalize_answer(custom(copy(call), copy(review_info), copy(verdict), cancel))
				return ans
			end
			if not service then
				return { decision = "unavailable", reason = "approval service unavailable", source = "none" }
			end
			return service:request("tool", {
				call = review_call,
				info = review_info,
				verdict = verdict,
				schema = mare.projection.project(copy(declaration)),
				history = D.svc.agent and D.svc.agent:list_messages().data or {},
			}, check_policy, answer, cancel, req_opts)
		end)
		if cancel and cancel:is_cancelled() then
			return { decision = "deny", reason = "approval cancelled", source = "cancelled" }, nil, req_opts.approval_id
		end
		if not ok then
			return { decision = "deny", reason = tostring(a), source = "error" }, nil, req_opts.approval_id
		end
		return a, human_receipt, req_opts.approval_id
	end

	--- the tool result the model reads for a denied approval: the user's decline and the headless one say not to retry or work
	--- around it; the recorded `reason` stays as it is (events, session, sub-agent hosts)
	local DECLINED, DECLINED_WHY = "declined by the user", "^declined by the user: (.*)$"
	local HEADLESS = "approval required but nobody can answer (headless)"
	local NO_RETRY = "Do not retry it unchanged or work around it; adjust your approach or ask the user."
	local function denied_text(reason)
		reason = tostring(reason or "not approved")
		if reason == HEADLESS then
			return "Tool call denied: approval was required but nobody can answer in this mode;"
				.. " do not retry — report what you could not do."
		elseif reason == DECLINED then
			return "Tool call denied by the user. " .. NO_RETRY
		end
		local why = reason:match(DECLINED_WHY)
		if why then
			why = why:gsub("[%.%s]+$", "")
			return ("Tool call denied by the user: %s. %s"):format(why, NO_RETRY)
		end
		return "Tool call denied: " .. reason
	end

	return {
		policy_version = policy_version,
		snapshot_hash = snapshot_hash,
		approve = approve,
		granted = granted,
		denied_text = denied_text,
	}
end
