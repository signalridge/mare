-- permissions.rules: rule lookup (strictest wins), permission/parse tightening, the shell
-- verdict and the prefixes offered for "always allow"; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local copy, all_rules = D.copy, D.all_rules
	local norm_argv, argv_match, exe_key, cwd_ok = D.argv.norm_argv, D.argv.argv_match, D.argv.exe_key, D.argv.cwd_ok

	-- Validate the original rule from every source before matching it. In particular, a
	-- condition this version cannot evaluate must never turn into a tool-only allow.
	local FIELDS = {
		decision = true,
		argv = true,
		tool = true,
		tool_version = true,
		cwd = true,
		deny_args = true,
		scope = true,
		justification = true,
		description = true,
		name = true,
		expires = true,
		path = true,
		op = true,
	}
	local SEVERITY = { forbidden = 3, prompt = 2, allow = 1 }
	local OP = { read = true, write = true, create = true, delete = true }
	local glob_revision, glob_cache
	local function dedup_push(list, value)
		if type(value) ~= "string" or value == "" then
			return
		end
		for _, existing in ipairs(list) do
			if existing == value then
				return
			end
		end
		list[#list + 1] = value
	end
	local function path_suffix_after(path, root)
		if path == root then
			return ""
		end
		local n = #root
		local next_ch = path:sub(n + 1, n + 1)
		if path:sub(1, n) == root and (next_ch == "/" or (mare.os == "windows" and next_ch == "\\")) then
			return path:sub(n + 1)
		end
		return nil
	end
	local function anchor_roots_for_project(proj)
		local roots = {}
		local ok_p, proj_res = pcall(mare.fs.resolve_path, proj)
		local proj_phys
		if ok_p and type(proj_res) == "table" then
			dedup_push(roots, proj_res.logical)
			dedup_push(roots, proj_res.physical)
			proj_phys = proj_res.physical
		else
			dedup_push(roots, proj)
			proj_phys = mare.fs.resolve(proj)
		end
		-- `mare.fs.project_root()` canonicalizes `cwd` when `.mare/` exists; recover the
		-- logical symlink root from `mare.cwd` so relative rules match both spellings.
		local ok_c, cwd_res = pcall(mare.fs.resolve_path, mare.cwd)
		if ok_c and type(cwd_res) == "table" and cwd_res.logical ~= cwd_res.physical then
			local suffix = path_suffix_after(cwd_res.physical, proj_phys)
			if suffix == "" then
				dedup_push(roots, cwd_res.logical)
			elseif suffix and cwd_res.logical:sub(-#suffix) == suffix then
				local cand = cwd_res.logical:sub(1, #cwd_res.logical - #suffix)
				local ok_cand, cand_res = pcall(mare.fs.resolve_path, cand)
				if cand ~= "" and ok_cand and type(cand_res) == "table" and cand_res.physical == proj_phys then
					dedup_push(roots, cand_res.logical)
				end
			end
		end
		return roots
	end
	local function anchor_roots_for_home()
		local home = mare.fs.home
		local roots = {}
		if type(home) == "string" and home ~= "" then
			local ok, res = pcall(mare.fs.resolve_path, home)
			if ok and type(res) == "table" then
				dedup_push(roots, res.logical)
				dedup_push(roots, res.physical)
			else
				dedup_push(roots, home)
			end
		end
		return roots
	end
	local function path_globs(pattern)
		if type(pattern) ~= "string" or pattern == "" then
			return nil, "invalid path pattern"
		end
		local revision = D.policy_revision()
		if glob_revision ~= revision then
			glob_revision, glob_cache = revision, {}
		end
		local proj = mare.fs.project_root()
		local is_home = pattern == "~"
			or pattern:sub(1, 2) == "~/"
			or (mare.os == "windows" and pattern:sub(1, 2) == "~\\")
		local roots
		if is_home then
			roots = anchor_roots_for_home()
		elseif D.lib.is_project_relative(pattern) then
			roots = anchor_roots_for_project(proj)
		end
		if not roots or #roots == 0 then
			local key = proj .. "\0\2" .. pattern
			if not glob_cache[key] then
				local glob, err = mare.fs.compile_path_glob(pattern, proj)
				if not glob then
					return nil, err
				end
				glob_cache[key] = { glob }
			end
			return glob_cache[key]
		end
		local key = proj .. "\0" .. table.concat(roots, "\1") .. "\0" .. pattern
		if not glob_cache[key] then
			local compiled = {}
			for _, anchor in ipairs(roots) do
				local p = is_home and (pattern == "~" and anchor or (anchor .. pattern:sub(2))) or pattern
				local glob, err = mare.fs.compile_path_glob(p, anchor)
				if not glob then
					return nil, err
				end
				compiled[#compiled + 1] = glob
			end
			glob_cache[key] = compiled
		end
		return glob_cache[key]
	end
	local function string_list(value, nonempty)
		local dense, count = D.lib.is_dense_list(value)
		if not dense or (nonempty and count == 0) then
			return false
		end
		for _, v in ipairs(value) do
			if type(v) ~= "string" or v == "" then
				return false
			end
		end
		return true
	end
	local function validate_rule(r)
		if type(r) ~= "table" then
			return "rule must be an object"
		end
		-- Unknown decisions have no meaning and never match.
		if not SEVERITY[r.decision] then
			return nil, true
		end
		for k in pairs(r) do
			if not FIELDS[k] then
				return "unknown rule field " .. tostring(k)
			end
		end
		for _, k in ipairs({ "tool", "tool_version", "cwd", "scope", "justification", "description", "name", "expires" }) do
			if r[k] ~= nil and (type(r[k]) ~= "string" or r[k] == "") then
				return "invalid rule field " .. k
			end
		end
		if r.argv ~= nil and not string_list(r.argv, true) then
			return "argv must be a non-empty dense string list"
		end
		if r.deny_args ~= nil and (not r.argv or not string_list(r.deny_args, false)) then
			return "deny_args requires a dense string list and an argv rule"
		end
		if not r.tool and not r.argv and not r.path and not r.op then
			return "rule requires a tool, argv, path or op"
		end
		if r.tool and r.argv then
			return "combining tool and argv is not supported"
		end
		if r.tool_version and not r.tool then
			return "tool_version requires a tool"
		end
		if r.expires and r.expires ~= "session" and r.expires ~= "never" then
			return "invalid rule expiry"
		end
		if r.cwd then
			local globs, err = path_globs(r.cwd)
			if not globs then
				return "invalid cwd pattern: " .. tostring(err)
			end
		end
		if
			r.scope
			and not (
				r.scope == "all"
				or r.scope == "danger-full-access"
				or r.scope == "network"
				or r.scope == "workspace-write"
				or r.scope:match("^network:.+$")
				or r.scope:match("^write:.+$")
			)
		then
			return "invalid rule scope"
		end
		if r.argv and (r.path ~= nil or r.op ~= nil) then
			return "path/op cannot be combined with an argv rule"
		end
		if r.path ~= nil then
			if type(r.path) ~= "string" or r.path == "" then
				return "invalid path pattern"
			end
			local globs, err = path_globs(r.path)
			if not globs then
				return "invalid path pattern: " .. tostring(err)
			end
		end
		if r.op ~= nil and not OP[r.op] then
			return "invalid file operation"
		end
	end
	local function validate_source(src)
		local dense, count = D.lib.is_dense_list(src)
		if not dense then
			return nil, "rules must be a dense list"
		end
		for i = 1, count do
			local err, ignored = validate_rule(src[i])
			if ignored then
				return nil, ("rule %d: invalid rule decision"):format(i)
			end
			if err then
				return nil, ("rule %d: %s"):format(i, err)
			end
		end
		return true
	end
	local function invalid_deny(call)
		if D.shared.error then
			return "permission rules file is invalid: " .. D.shared.error
		end
		for i, r in ipairs(all_rules()) do
			local err, ignored = validate_rule(r)
			if err and not ignored then
				local decision = type(r) == "table" and r.decision or nil
				local tool = type(r) == "table" and r.tool or nil
				if decision ~= "allow" and (type(tool) ~= "string" or tool == "" or tool == call.name) then
					return ("invalid permission rule %d: %s"):format(i, err)
				end
			end
		end
	end

	--- the first argv rule matching one segment's argv
	-- several rules can match one command: the strictest wins, whatever their order or
	-- specificity — forbidden > prompt > allow
	local function strictest(pred)
		local best
		for _, r in ipairs(all_rules()) do
			local err, ignored = validate_rule(r)
			if not err and not ignored and pred(r) and (not best or SEVERITY[r.decision] > SEVERITY[best.decision]) then
				best = r
			end
		end
		return best
	end
	local tighten_parse
	local function rule_for(seg, nested_shell, cwd, shell)
		return strictest(function(r)
			if not r.argv or r.path or r.op or (nested_shell and r.decision == "allow") then
				return false
			end
			if r.decision == "allow" then
				return argv_match(r, seg.argv, seg.argv, cwd, shell)
			end
			return argv_match(r, seg.argv, seg.argv, cwd, shell)
				or argv_match(r, seg.argv, seg.unwrapped or seg.argv, cwd, shell)
		end)
	end
	--- tool-level rule (unconfined tools allow-listed by name; session rules bind to tool_version)
	local function tool_rule(call, info)
		local ver = tostring(info and info.tool_version or "")
		return strictest(function(r)
			return r.tool
				and not r.path
				and not r.op
				and r.tool == call.name
				and (r.tool_version == nil or r.tool_version == ver)
				and cwd_ok(r.cwd, call.cwd, r.decision ~= "allow")
		end)
	end
	local function operation_matches(rule, op)
		return rule.op == nil or rule.op == op or (rule.op == "write" and (op == "create" or op == "delete"))
	end
	local function scope_covers(rule, item)
		local scope = rule.scope
		return not scope
			or scope == "all"
			or scope == "danger-full-access"
			or (scope == "workspace-write" and mare.proc.sandbox.within(item.physical, { mare.cwd }))
			or (scope:match("^write:(.+)$") and mare.proc.sandbox.within(item.physical, { scope:sub(7) }))
	end
	local function sensitive_exempt(call, item, tool_version)
		if item.recursive then
			return false
		end
		for _, r in ipairs(all_rules()) do
			local err, ignored = validate_rule(r)
			if
				not err
				and not ignored
				and r.decision == "allow"
				and r.tool == call.name
				and r.path
				and not r.argv
				and operation_matches(r, item.op)
				and scope_covers(r, item)
				and (not r.tool_version or r.tool_version == tostring(tool_version or ""))
				and cwd_ok(r.cwd, call.cwd, false)
			then
				local globs = path_globs(r.path)
				if globs then
					local logical_hit, physical_hit = false, false
					for _, glob in ipairs(globs) do
						if glob:literal() then
							logical_hit = logical_hit or glob:matches(item.logical)
							physical_hit = physical_hit or glob:matches(item.physical)
						end
					end
					if logical_hit and physical_hit then
						return true
					end
				end
			end
		end
		return false
	end
	local function path_verdict(call, info)
		local candidates = {}
		for _, r in ipairs(all_rules()) do
			local err, ignored = validate_rule(r)
			if
				not err
				and not ignored
				and (r.path or r.op)
				and not r.argv
				and (not r.tool or r.tool == call.name)
				and (not r.tool_version or r.tool_version == tostring(info.tool_version or ""))
				and cwd_ok(r.cwd, call.cwd, r.decision ~= "allow")
			then
				candidates[#candidates + 1] = r
			end
		end
		if #candidates == 0 then
			return nil
		end
		if info.access_state ~= "declared" or type(info.access) ~= "table" then
			for _, r in ipairs(candidates) do
				if r.decision ~= "allow" then
					return { decision = "forbidden", justification = "tool access declaration is required" }
				end
			end
			return nil
		end
		if #info.access == 0 then
			return nil
		end
		local all_allowed, prompt, has_write, matched_rules = true, nil, false, {}
		local scope, scope_seen, scope_ok = nil, false, true
		for _, item in ipairs(info.access) do
			local logical_allowed, physical_allowed = false, false
			for _, r in ipairs(candidates) do
				if operation_matches(r, item.op) then
					local globs = r.path and path_globs(r.path) or nil
					if r.path and not globs then
						return { decision = "forbidden", justification = "path pattern could not be evaluated" }
					end
					local function hit(path)
						if not globs then
							return not item.recursive or r.decision ~= "allow"
						end
						for _, glob in ipairs(globs) do
							if item.recursive then
								if r.decision == "allow" then
									if glob:covers_recursive(path) then
										return true
									end
								elseif glob:overlaps(path) then
									return true
								end
							elseif glob:matches(path, r.decision ~= "allow") then
								return true
							end
						end
						return false
					end
					local logical, physical = hit(item.logical), hit(item.physical)
					if (logical or physical) and r.decision == "forbidden" then
						return r
					end
					if (logical or physical) and r.decision == "prompt" then
						prompt = r
					end
					if r.decision == "allow" then
						if scope_covers(r, item) then
							logical_allowed = logical_allowed or logical
							physical_allowed = physical_allowed or physical
							if logical or physical then
								matched_rules[#matched_rules + 1] = r
								if not scope_seen then
									scope, scope_seen = r.scope, true
								elseif r.scope ~= scope then
									scope_ok = false
								end
							end
						end
					end
				end
			end
			all_allowed = all_allowed and logical_allowed and physical_allowed
			if item.op ~= "read" then
				has_write = true
			end
		end
		if prompt then
			return prompt
		end
		if all_allowed and not info.unconfined then
			return {
				decision = "allow",
				path_rule = true,
				has_write = has_write,
				scope = scope_ok and scope or nil,
				rules = matched_rules,
			}
		end
	end
	--- permission/parse: listeners see a copy of the authoritative parse; only tightening
	--- survives (analyzable → not, a segment → unanalyzable, deny reasons, notes)
	function tighten_parse(parsed, command, tool, shell)
		local wf = ctx.bus:waterfall("permission/parse", {
			parsed = copy(parsed),
			command = command,
			tool = tool,
			shell = shell,
		})
		local got = (type(wf) == "table" and type(wf.parsed) == "table" and wf.parsed) or wf
		if type(got) ~= "table" then
			return parsed
		end
		local out = copy(parsed)
		out.notes = {}
		if type(got.notes) == "table" then
			for _, n in ipairs(got.notes) do
				out.notes[#out.notes + 1] = tostring(n)
			end
		end
		if type(got.deny_reasons) == "table" and #got.deny_reasons > 0 then
			out.deny_reasons = {}
			for _, d in ipairs(got.deny_reasons) do
				out.deny_reasons[#out.deny_reasons + 1] = tostring(d)
			end
		end
		if parsed.analyzable and got.analyzable == false then
			out.analyzable = false
			out.reason = tostring(got.reason or "marked not analyzable by a plugin")
			out.segments = {}
			return out
		end
		if out.analyzable and type(got.segments) == "table" then
			for i, seg in ipairs(out.segments) do
				local g = got.segments[i]
				if type(g) == "table" then
					if g.unanalyzable == true then
						seg.unanalyzable = true
					end
					if g.note ~= nil then
						seg.note = tostring(g.note)
					end
				end
			end
		end
		return out
	end
	local function bin_name(w)
		return type(w) == "string" and w:match("[^/\\]+$") or nil
	end
	local BASH_BINS = { bash = true, sh = true, zsh = true, dash = true, ksh = true, ash = true }
	local PS_BINS = { pwsh = true, powershell = true, cmd = true }
	local WRAPPER_VAL_OPTS = {
		sudo = {
			["-u"] = 1,
			["-g"] = 1,
			["-U"] = 1,
			["-C"] = 1,
			["-D"] = 1,
			["-h"] = 1,
			["-p"] = 1,
			["-r"] = 1,
			["-t"] = 1,
			["-T"] = 1,
			["--user"] = 1,
			["--group"] = 1,
		},
		doas = { ["-u"] = 1, ["-C"] = 1 },
		env = { ["-u"] = 1, ["--unset"] = 1, ["-C"] = 1, ["--chdir"] = 1 },
		nice = { ["-n"] = 1, ["--adjustment"] = 1 },
		timeout = { ["-s"] = 1, ["--signal"] = 1, ["-k"] = 1, ["--kill-after"] = 1 },
		xargs = {
			["-I"] = 1,
			["-L"] = 1,
			["-n"] = 1,
			["-P"] = 1,
			["-s"] = 1,
			["-d"] = 1,
			["-E"] = 1,
			["-a"] = 1,
			["--delimiter"] = 1,
			["--max-args"] = 1,
		},
		exec = { ["-a"] = 1 },
		stdbuf = { ["-i"] = 1, ["-o"] = 1, ["-e"] = 1 },
	}
	local WRAPPER_BINS = {
		sudo = true,
		doas = true,
		env = true,
		command = true,
		exec = true,
		nohup = true,
		nice = true,
		timeout = true,
		xargs = true,
		time = true,
		stdbuf = true,
		chronic = true,
		busybox = true,
	}
	local function argv_shell(argv)
		local i = 1
		while i <= #argv do
			local b = bin_name(argv[i])
			local exe = b and b:lower():gsub("%.exe$", "") or ""
			if PS_BINS[exe] then
				return "powershell"
			elseif BASH_BINS[exe] then
				return "bash"
			elseif not WRAPPER_BINS[exe] then
				break
			end
			i = i + 1
			local takes = WRAPPER_VAL_OPTS[exe]
			while i <= #argv do
				local tok = argv[i]
				if tok == "--" then
					i = i + 1
					break
				elseif takes and takes[tok] then
					i = i + 2
				elseif
					exe == "env"
					and (
						tok == "-S"
						or tok == "--split-string"
						or tok:match("^%-S.")
						or tok:match("^%-%-split%-string=")
					)
				then
					break
				elseif tok:sub(1, 1) == "-" or (exe == "env" and tok:find("^[%w_]+=")) then
					i = i + 1
				elseif exe == "timeout" and tok:match("^%d") then
					i = i + 1
					break
				else
					break
				end
			end
		end
		return mare.os == "windows" and "powershell" or "bash"
	end
	local function resolve_command(call, info)
		if not (info and info.command_tool == true) then
			return nil
		end
		local args = type(call and call.arguments) == "table" and call.arguments or {}
		local spec = info.command
		if type(spec) ~= "table" then
			return { kind = "unanalyzable", reason = "command representation is not declared" }
		end
		if type(spec.shell) == "string" and type(spec.arg) == "string" then
			local shell = spec.shell
			local raw = args[spec.arg]
			local text = type(raw) == "string" and raw or nil
			if shell ~= "bash" and shell ~= "powershell" and shell ~= "pwsh" then
				return {
					kind = "unanalyzable",
					text = text,
					reason = ("unsupported command shell `%s`"):format(shell),
				}
			end
			if shell == "pwsh" then
				shell = "powershell"
			end
			if text == nil then
				return {
					kind = "unanalyzable",
					shell = shell,
					reason = ("command argument `%s` is missing or not a string"):format(spec.arg),
				}
			end
			return { kind = "shell", shell = shell, text = text }
		end
		if type(spec.argv) == "table" then
			local argv, err = require("mare_lib").expand_argv_template(spec.argv, spec.defaults, args)
			if not argv then
				return { kind = "unanalyzable", reason = err }
			end
			return {
				kind = "argv",
				shell = argv_shell(argv),
				argv = argv,
				text = table.concat(argv, " "),
			}
		end
		return { kind = "unanalyzable", reason = "command representation is not declared" }
	end

	local function command_text(call, info)
		local repr = resolve_command(call, info)
		if repr then
			return repr.text
		end
		local args = type(call and call.arguments) == "table" and call.arguments or nil
		return args and type(args.command) == "string" and args.command or nil
	end

	--- Evaluate shell rules on the parsed command. Returns
	--- { parsed, segs = {{argv, op, rule}}, decision = "forbidden"|"prompt"|"allow"|nil,
	---   rule = the deciding rule, unmatched = n }
	local function shell_verdict(call, repr)
		local shell = repr.shell or (mare.os == "windows" and "powershell" or "bash")
		local popt = cfg.parse or {}
		local command = repr.text
		local parsed
		if repr.kind == "shell" then
			parsed = mare.text.shell_parse(command, { shell = shell, safe_sinks = popt.safe_sinks })
		elseif repr.kind == "argv" then
			parsed = mare.text.shell_parse(repr.argv, { shell = shell, safe_sinks = popt.safe_sinks })
		else
			parsed = {
				analyzable = false,
				reason = repr.reason or "command representation is not analyzable",
				segments = {},
			}
		end
		if parsed.analyzable and popt.deny_commands and #popt.deny_commands > 0 then
			local deny = {}
			for _, w in ipairs(popt.deny_commands) do
				deny[w] = true
			end
			for _, seg in ipairs(parsed.segments) do
				local w = bin_name(seg.argv[1])
				if w and deny[w] then
					parsed = {
						analyzable = false,
						reason = ("`%s` is configured as not analyzable"):format(w),
						segments = {},
						script = parsed.script,
						literal_paths = parsed.literal_paths,
					}
					break
				end
			end
		end
		-- the built-in parse is the only authority ( fixes its parser, a complex
		-- parse switches auto-approval off). `permission/parse` listeners get a copy and may
		-- only tighten: mark the command or single segments not analyzable, add deny reasons,
		-- add notes for the dialog. A different parse (other argv, ops, more segments) or
		-- turning "not analyzable" into segments is ignored.
		parsed = tighten_parse(parsed, command or "", call.name, shell)
		local v = { parsed = parsed, segs = {}, unmatched = 0 }
		if parsed.deny_reasons and #parsed.deny_reasons > 0 then
			v.decision = "forbidden"
			v.rule = { decision = "forbidden", justification = table.concat(parsed.deny_reasons, "; ") }
			return v
		end
		if not parsed.analyzable then
			-- already-parsed outer segments (e.g. a definite command paired with a wrapped
			-- shell or a literal command with a file redirection) still enforce an exact
			-- `forbidden` rule as `deny` instead of downgrading it to a word-scan `ask`
			local seg_prompt
			for _, seg in ipairs(parsed.segments or {}) do
				local r = rule_for(seg, parsed.script ~= nil, call.cwd, shell)
				if r and r.decision == "forbidden" then
					v.decision, v.rule = "forbidden", r
					return v
				end
				if r and r.decision == "prompt" and not seg_prompt then
					seg_prompt = r
				end
			end
			-- A plain literal command with only a literal file redirection (`printf x > out`)
			-- still blocks `allow` rules (`analyzable == false`), but every executable and
			-- redirect target is statically known and already checked above against `forbidden`
			-- and `prompt` rules.
			if parsed.reason == "redirection" and #(parsed.segments or {}) > 0 and not parsed.script then
				if seg_prompt then
					v.decision, v.rule = "prompt", seg_prompt
				end
				return v
			end
			-- allow rules never apply. Gate hard `forbidden` denial on a word scan for a
			-- cwd-matching forbidden executable (including assignment RHS); when no forbidden
			-- executable word appears, downgrade to `prompt` (ask without amendment) so ordinary
			-- glob/variable/loop commands stay approvable rather than hard-denied across presets.
			local seen = {}
			local scan_cmd = parsed.script or command
			if type(scan_cmd) == "string" and scan_cmd ~= "" then
				local _, commands = mare.text.dangerous(scan_cmd, { shell = shell })
				for _, argv in ipairs(commands or {}) do
					if argv[1] then
						seen[exe_key(argv[1], shell)] = true
					end
				end
				-- A syntax error or unsupported shell may have no literal parse. Over-reporting
				-- command-like words is safe for deny/prompt rules; it must never grant an allow.
				for word in scan_cmd:gmatch("[^%s;&|()<>\"'`]+") do
					seen[exe_key(word, shell)] = true
					local val = word:match("^[^=]+=(.+)$")
					if val then
						seen[exe_key(val, shell)] = true
					end
				end
			end
			local best, best_decision
			for _, r in ipairs(all_rules()) do
				local err, ignored = validate_rule(r)
				-- keep the rule's directory scope: a rule for /repos/prod never applies in /repos/dev
				if
					not err
					and not ignored
					and r.argv
					and r.argv[1]
					and r.decision ~= "allow"
					and cwd_ok(r.cwd, call.cwd, true)
				then
					local decision = (r.decision == "forbidden" and seen[exe_key(r.argv[1], shell)]) and "forbidden"
						or "prompt"
					if not best or SEVERITY[decision] > SEVERITY[best_decision] then
						best, best_decision = r, decision
					end
					if best_decision == "forbidden" then
						break
					end
				end
			end
			if best then
				local resolved = copy(best)
				resolved.decision = best_decision
				if best_decision == "forbidden" then
					local why = ("not analyzable: %s — cannot verify against forbidden rule"):format(
						tostring(parsed.reason or "dynamic shell syntax")
					)
					resolved.justification = best.justification and (best.justification .. " · " .. why) or why
				end
				v.decision, v.rule = best_decision, resolved
			end
			return v
		end
		local all_allow, scope_ok, scope = true, true, nil
		for i, seg in ipairs(parsed.segments) do
			-- a segment a plugin marked not analyzable never matches an allow rule
			local r = rule_for(seg, parsed.script ~= nil, call.cwd, shell)
			if seg.unanalyzable and r and r.decision == "allow" then
				r = nil
			end
			v.segs[i] = { argv = seg.argv, op = seg.op, rule = r, note = seg.note }
			if r and r.decision == "forbidden" then
				v.decision, v.rule = "forbidden", r
				return v
			end
			if r and r.decision == "prompt" and v.decision ~= "prompt" then
				v.decision, v.rule = "prompt", r
			end
			if not (r and r.decision == "allow") then
				all_allow = false
				v.unmatched = v.unmatched + 1
			else
				-- one sandbox applies to the whole command: widen only when every segment's
				-- rule names the same scope (allow never widens beyond what rules state)
				if i == 1 then
					scope = r.scope
				elseif r.scope ~= scope then
					scope_ok = false
				end
			end
		end
		if v.decision == nil and all_allow then
			v.decision = "allow"
			v.rule = { decision = "allow", scope = scope_ok and scope or nil }
		end
		return v
	end
	--- "✓ git status && ✗ cargo test" — segments for the approval dialog
	local function segments_text(v)
		if not v or not v.parsed then
			return nil
		end
		local notes = (v.parsed.notes and #v.parsed.notes > 0) and ("  [" .. table.concat(v.parsed.notes, "; ") .. "]")
			or ""
		if not v.parsed.analyzable then
			return "not analyzable: " .. tostring(v.parsed.reason) .. notes
		end
		local has_note = false
		for _, sg in ipairs(v.segs) do
			has_note = has_note or sg.note ~= nil
		end
		if #v.segs < 2 and v.unmatched == #v.segs and not has_note then
			return notes ~= "" and notes or nil
		end
		local parts = {}
		for _, sg in ipairs(v.segs) do
			local mark = (sg.rule and sg.rule.decision == "allow") and "✓" or "✗"
			parts[#parts + 1] = (sg.op and (sg.op .. " ") or "")
				.. mark
				.. " "
				.. table.concat(sg.argv, " ")
				.. (sg.note and (" (" .. sg.note .. ")") or "")
		end
		return "segments: " .. table.concat(parts, " ") .. notes
	end

	--- "git push" — the command prefix offered for "always allow" rules (after normalizing the
	--- executable without peeling wrappers). nil when the command is too general to become a
	--- rule: interpreters and wrappers run anything, and a bare `git`/`npm`… covers every
	--- subcommand
	local NEVER_RULE = {}
	for _, w in ipairs({
		"bash",
		"sh",
		"zsh",
		"fish",
		"dash",
		"ksh",
		"pwsh",
		"powershell",
		"cmd",
		"python",
		"python3",
		"python2",
		"node",
		"deno",
		"bun",
		"ruby",
		"perl",
		"php",
		"lua",
		"osascript",
		"env",
		"sudo",
		"doas",
		"su",
		"xargs",
		"eval",
		"exec",
		"command",
		"nohup",
		"timeout",
		"nice",
		"ssh",
		"docker",
		"kubectl",
		"npx",
		"uvx",
		"pipx",
	}) do
		NEVER_RULE[w] = true
	end
	local NEEDS_SUBCOMMAND = {}
	for _, w in ipairs({ "git", "npm", "pnpm", "yarn", "cargo", "go", "make", "uv", "pip", "pip3", "gh", "brew" }) do
		NEEDS_SUBCOMMAND[w] = true
	end
	local function prefix_of(argv, shell)
		if not argv or #argv == 0 then
			return nil
		end
		local key = exe_key(argv[1], shell)
		if not key or NEVER_RULE[key] then
			return nil
		end
		argv = norm_argv(argv, false, shell)
		if not argv[1] then
			return nil
		end
		local out = { argv[1] }
		if argv[2] and not tostring(argv[2]):match("^%-") then
			out[2] = argv[2]
		elseif NEEDS_SUBCOMMAND[key] then
			return nil -- `git -c x status`: no safe prefix
		end
		return out
	end
	-- flags that change what an allowed prefix does (run code, change config or directory):
	-- a saved rule refuses them by default
	local DEFAULT_DENY_ARGS =
		{ "-c", "-C", "-e", "--exec", "--eval", "--config", "--config-env", "--upload-pack", "--receive-pack" }

	return {
		validate_source = validate_source,
		invalid_deny = invalid_deny,
		path_verdict = path_verdict,
		path_globs = path_globs,
		path_suffix_after = path_suffix_after,
		sensitive_exempt = sensitive_exempt,
		resolve_command = resolve_command,
		command_text = command_text,
		tool_rule = tool_rule,
		shell_verdict = shell_verdict,
		segments_text = segments_text,
		prefix_of = prefix_of,
		DEFAULT_DENY_ARGS = DEFAULT_DENY_ARGS,
	}
end
