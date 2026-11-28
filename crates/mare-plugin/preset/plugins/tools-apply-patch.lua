-- tools-apply-patch: `apply_patch` format (default edit tool for GPT models).
local DESCRIPTION =
	[[Use the `apply_patch` tool to edit files. The patch language is a stripped-down, file-oriented diff format:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Each file section starts with one of:
*** Add File: <path>      — every following line starts with `+` (the new file's content)
*** Delete File: <path>   — nothing follows
*** Update File: <path>   — optionally followed by `*** Move to: <new path>`, then one or more hunks

Each hunk starts with `@@` (optionally followed by a class/function header to locate the change),
then lines prefixed with ` ` (context), `-` (removed) or `+` (added). Show ~3 lines of context
around each change; do not use line numbers. Paths are relative to the working directory.

Example:
*** Begin Patch
*** Update File: src/app.py
@@ def greet():
-    print("Hi")
+    print("Hello, world!")
*** End Patch]]

-- Lark grammar for the freeform apply_patch tool (Apache-2.0)
local APPLY_PATCH_LARK = [==[
start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
]==]

return {
	name = "tools-apply-patch",
	-- `bus` for fs/changes-inexact, a mare event with
	-- transition-layer listeners
	inject = { "tools", "bus" },
	apply = function(ctx)
		local root_ctx = ctx
		local tools = ctx.tools

		-- usage guide as a system-prompt section, only for models that use apply_patch (GPT family)
		-- registered while the optional `context` service is up (a child fiber on the cordis tree)
		ctx:inject({ "context" }, function(cctx)
			cctx.context:section({
				name = "apply_patch",
				order = 150,
				text = DESCRIPTION,
				when = function(model)
					return tools:edit_mode(model) == "apply_patch"
				end,
			})
			-- GPT family: editing and verification rules scoped to GPT/Codex declarations via
			-- `gpt_rules` so non-GPT `apply_patch` models omit them
			cctx.context:section({
				name = "gpt_rules",
				order = 210,
				-- General and editing constraints; no plan-tool rules since mare has no update_plan
				-- tool.
				text = table.concat({
					"## General",
					"- When searching for text or files, prefer `rg` or `rg --files`; they are much faster than grep/find.",
					"",
					"## Editing constraints",
					"- Default to ASCII when editing or creating files unless the file already uses other characters.",
					"- Use apply_patch for single-file edits; not for generated changes (formatters, lockfiles) or bulk search-and-replace.",
					"- You may be in a dirty git worktree. Never revert changes you did not make unless the user asks; if they are in files you must touch, work with them.",
					"- If you notice unexpected changes you didn't make, stop and ask the user how to proceed.",
					"- Never use destructive commands like `git reset --hard` or `git checkout --` unless the user asks. Do not amend commits unless asked.",
					"",
					"## Final answer",
					"- Plain text; be concise. Use short headers or `-` bullets only when they help scanning; no nested bullets.",
					"- Wrap commands, paths and identifiers in backticks; reference files as `path:line`.",
					"- Don't dump large files you wrote; summarize what changed and suggest next steps (tests, build) when relevant.",
				}, "\n"),
				when = function(model)
					if not model then
						return false
					end
					if model.gpt_rules ~= nil then
						return model.gpt_rules == true
					end
					local llm = root_ctx:get("llm")
					return llm ~= nil and llm:model_default(model, "gpt_rules") == true
				end,
			})
		end)
		tools:register("apply_patch", {
			access = function(a)
				local paths, err = mare.text.patch_paths(a.input)
				assert(paths, err)
				local out = {}
				local function add(path, op)
					out[#out + 1] = { kind = "file", op = op, path = path, recursive = false }
				end
				for _, item in ipairs(paths) do
					add(item.path, "read")
					if item.kind == "add" then
						add(item.path, "create")
					elseif item.kind == "delete" then
						add(item.path, "delete")
					elseif item.move_to then
						add(item.path, "delete")
						add(item.move_to, "write")
						add(item.move_to, "create")
					else
						add(item.path, "write")
					end
				end
				return out
			end,
			side_effect = "write",
			idempotent = false,
			enabled = function(_, mode)
				return mode == "apply_patch"
			end,
			snippet = "Edit files with patches (add, update, delete, move)",
			description = "Edit files with a patch (*** Begin Patch … *** End Patch). See the apply_patch section for the format.",
			-- Providers that support custom tools (OpenAI Responses) declare this as a freeform
			-- grammar tool; others fall back to the JSON schema below,
			-- with the patch text in `input`. The provider decides — nothing is hard-wired here.
			freeform = {
				description = "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
				syntax = "lark",
				definition = APPLY_PATCH_LARK,
			},
			parameters = {
				type = "object",
				properties = {
					input = { type = "string", description = "The entire patch, from *** Begin Patch to *** End Patch" },
				},
				required = { "input" },
			},
			summary = function(a)
				local files = {}
				for kind, path in string.gmatch(a.input or "", "%*%*%* (%a+) File: ([^\r\n]-)%s*\r?\n") do
					files[#files + 1] = path
				end
				return ("Patch(%s)"):format(table.concat(files, ", "))
			end,
			preview = function(a)
				if type(a) ~= "table" or type(a.input) ~= "string" or a.input == "" then
					return nil
				end
				local policy = mare.proc
					and mare.proc.sandbox
					and mare.proc.sandbox.current
					and mare.proc.sandbox.current()
				local policies = policy and { policy } or nil
				local parsed = mare.text.patch_paths(a.input, { resolve = true })
				if type(parsed) == "table" then
					local readable = true
					for _, item in ipairs(parsed) do
						local check_path = item.path
						if item.kind ~= "add" then
							local st = mare.fs.stat(check_path, { policies = policies })
							if not st or st.kind ~= "file" then
								readable = false
								break
							end
						end
					end
					if readable then
						local ok, sim = pcall(mare.text.apply_patch, a.input, { dry_run = true })
						if ok and type(sim) == "table" and type(sim.changes) == "table" and #sim.changes > 0 then
							local out = {}
							for _, ch in ipairs(sim.changes) do
								local target = ch.move_to or ch.path
								local display_path = mare.fs.relative(target)
								local project = mare.projection and mare.projection.project
									or function(s)
										return s
									end
								if ch.kind == "delete" then
									out[#out + 1] = ui.Line({ ui.Span("Delete " .. display_path):hl("MareMuted") })
								else
									out[#out + 1] = ui.Diff({
										old = project(ch.old or ""),
										new = project(ch.new or ""),
										path = display_path,
										full_path = target,
										is_new = ch.kind == "add" or nil,
									})
								end
							end
							if #out > 0 then
								return ui.Lines(out)
							end
						end
					end
				end
				local project = mare.projection and mare.projection.project or function(s)
					return s
				end
				return ui.Code(project(a.input), "diff")
			end,
			execute = function(a)
				-- (1) preflight — simulate the whole patch in memory and collect every refusal; any
				-- refusal → nothing is written; (2) commit in order, stop at the first failure, no
				-- rollback; every file gets a status: completed / failed / unknown (the write
				-- errored and the file changed — re-read) / skipped (after the stop). Mutations of
				-- the same file are queued.
				local function rejected(err)
					return {
						content = "Patch rejected before any change: " .. tostring(err) .. "\nNo files were modified.",
						is_error = true,
					}
				end
				-- first parse only names the files to lock (resolved literally like apply_patch); check read access before probing any identity
				local parsed, perr = mare.text.patch_paths(a.input, { resolve = true })
				if not parsed then
					return rejected(perr)
				end
				local paths = {}
				for _, ch in ipairs(parsed) do
					mare.exec.check("read", ch.path)
					paths[#paths + 1] = ch.path
					if ch.move_to then
						mare.exec.check("read", ch.move_to)
						paths[#paths + 1] = ch.move_to
					end
				end
				return tools:with_file_lock(paths, function()
					local edit_max = (
						mare.config
						and mare.config.plugins
						and mare.config.plugins.tools
						and mare.config.plugins.tools.edit_max_bytes
					) or (10 * 1024 * 1024)
					for _, p in ipairs(paths) do
						local st = mare.fs.stat(p)
						if st then
							if st.kind ~= "file" then
								return rejected(("%s: not a regular file"):format(p))
							end
							if st.size and st.size > edit_max then
								return rejected(("%s: file is %d bytes (limit %d bytes)"):format(p, st.size, edit_max))
							end
						end
					end
					-- simulate again under the lock: the plan is computed from what is on disk now,
					-- not from before another edit of these files finished
					local res, err = mare.text.apply_patch(a.input, { dry_run = true })
					if not res then
						return rejected(err)
					end
					local changes = res.changes
					local function key(path)
						return mare.fs.identity(path) or mare.fs.resolve(path)
					end
					local function verb(ch)
						return ch.kind == "add" and "write" or "edit"
					end
					-- the file must hold exactly what the simulation started from: a later section
					-- of a file chains on the earlier section's result (compute_changes), and
					-- anything else that changed it is stale
					local function check(ch)
						local cur, gerr = tools:fs_guard(ch.path, verb(ch), "apply_patch")
						if cur and ch.kind ~= "add" and cur.data ~= ch.old then
							return nil,
								("apply_patch %s: the file changed while patching — read it again (stale)"):format(
									ch.path
								)
						end
						return cur, gerr
					end
					-- (1) preflight: every problem, not just the first. Each file is checked at its
					-- first section; later sections of it depend on the earlier ones being written
					local plan, refusals, seen = {}, {}, {}
					for i, ch in ipairs(changes) do
						local p = { change = ch }
						local k = key(ch.path)
						if not seen[k] then
							seen[k] = true
							local cur, gerr = check(ch)
							if not cur then
								refusals[#refusals + 1] = gerr
							end
						end
						if ch.move_to then
							local dk = key(ch.move_to)
							if not seen[dk] then
								seen[dk] = true
								local dest, derr = tools:fs_guard(ch.move_to, "write", "apply_patch")
								if not dest then
									refusals[#refusals + 1] = derr
								end
							end
						end
						-- a change that changes nothing is a mistake, not a success
						if ch.kind == "update" and not ch.move_to and ch.old == ch.new then
							refusals[#refusals + 1] = ("apply_patch %s: the update changes nothing"):format(ch.path)
						end
						plan[i] = p
					end
					if #refusals > 0 then
						return {
							content = ("Patch rejected before any change (%d problem%s):\n- %s\nNo files were modified."):format(
								#refusals,
								#refusals == 1 and "" or "s",
								table.concat(refusals, "\n- ")
							),
							is_error = true,
							error_code = "fs_guard",
						}
					end

					-- (2) commit in order, stop at the first failure. The guard is taken again right
					-- before each file: an earlier section may have rewritten it (and recorded the
					-- new version), so the check runs against the chained state
					local outcomes = {}
					for i, ch in ipairs(changes) do
						outcomes[i] = { path = ch.move_to or ch.path, kind = ch.kind, status = "skipped" }
					end
					local function changed_since(path, before)
						local st = mare.fs.stat(path)
						if before == nil then
							return st ~= nil
						end
						return not st or st.size ~= before.size or st.mtime ~= before.mtime
					end
					local stop
					for i, ch in ipairs(changes) do
						local o = outcomes[i]
						local touched = { ch.path }
						if ch.move_to then
							touched[2] = ch.move_to
						end
						local before = {}
						for _, t in ipairs(touched) do
							before[t] = mare.fs.stat(t) or false
						end
						local ok, werr
						local cur, gerr = check(ch)
						local dest
						if cur and ch.move_to then
							dest, gerr = tools:fs_guard(ch.move_to, "write", "apply_patch")
						end
						if not cur or (ch.move_to and not dest) then
							ok, werr = nil, gerr
						elseif ch.kind == "delete" then
							ok, werr = pcall(mare.fs.remove, ch.path)
							tools:observe(ch.path)
						elseif ch.move_to then
							-- the moved file keeps its mode (an executable script stays executable)
							local st = mare.fs.stat(ch.path)
							ok, werr = tools:fs_commit(
								ch.move_to,
								ch.new or "",
								dest,
								"apply_patch",
								{ mode = st and st.mode or nil }
							)
							-- the source goes unless it names the destination's own directory entry
							-- (`a` → `./a`, or a case-only rename on a case-insensitive file system);
							-- a hard link to the destination is a different entry and does go
							if ok and mare.fs.realpath(ch.path) ~= mare.fs.realpath(ch.move_to) then
								ok, werr = pcall(mare.fs.remove, ch.path)
								tools:observe(ch.path)
							end
						else
							ok, werr = tools:fs_commit(ch.path, ch.new or "", cur, "apply_patch")
						end
						if ok then
							o.status = "completed"
						else
							-- unknown: the write errored but the file is no longer what it was
							local uncertain = false
							for _, t in ipairs(touched) do
								if changed_since(t, before[t] or nil) then
									uncertain = true
								end
							end
							o.status = uncertain and "unknown" or "failed"
							o.error = tostring(werr)
								.. (uncertain and " — the file changed; read it again before retrying" or "")
							stop = i
							break
						end
					end

					local exact = true
					for _, o in ipairs(outcomes) do
						if o.status == "unknown" then
							exact = false
						end
					end
					if not exact then
						-- dependent state (change summaries, /diff) must not trust these files (`exact`
						-- false): listeners drop or recompute what they derived
						local unsure = {}
						for _, o in ipairs(outcomes) do
							if o.status == "unknown" then
								unsure[#unsure + 1] = o.path
							end
						end
						ctx.bus:emit("fs/changes-inexact", { tool = "apply_patch", paths = unsure })
					end
					-- the result keeps each written file's diff, not its old and new text (
					-- `version = 2`; older results carry `old` / `new` per change)
					local stored = {}
					for i, ch in ipairs(changes) do
						local s = { path = ch.path, move_to = ch.move_to, kind = ch.kind }
						if ch.kind ~= "delete" and outcomes[i].status == "completed" then
							local diff, _, _, lines =
								mare.text.diff(ch.old or "", ch.new or "", { path = outcomes[i].path })
							s.diff, s.lines = diff, lines
						end
						stored[i] = s
					end
					local details = { version = 2, changes = stored, outcomes = outcomes, exact = exact }
					if not stop then
						return { content = "Success. Updated the following files:\n" .. res.summary, details = details }
					end
					local lines = {
						("Patch stopped at file %d of %d (%d completed, not rolled back)."):format(
							stop,
							#changes,
							stop - 1
						),
					}
					for _, o in ipairs(outcomes) do
						lines[#lines + 1] = ("%s %s%s"):format(o.status, o.path, o.error and (": " .. o.error) or "")
					end
					return { content = table.concat(lines, "\n"), is_error = true, details = details }
				end)
			end,
			render = function(a, res)
				if not res.details or not res.details.changes then
					return nil
				end
				local changes = res.details.changes
				local outcomes = res.details.outcomes or {}
				local multi = #changes > 1
				local out = {}
				for i, ch in ipairs(changes) do
					local status = outcomes[i] and outcomes[i].status or "completed"
					local path = mare.fs.relative(ch.move_to or ch.path)
					if status ~= "completed" then
						-- a stopped patch: only what was actually written is shown as a diff
						out[#out + 1] = ui.Line({
							ui.Span(status .. " " .. path):hl(status == "skipped" and "MareDim" or "MareError"),
						})
					elseif ch.kind == "delete" then
						out[#out + 1] = ui.Line({ ui.Span("Deleted " .. path):hl("MareMuted") })
					else
						if multi then
							-- `ui.Diff` already renders `+A −D` on row 1, so only the path header is needed
							out[#out + 1] = ui.Line({ ui.Span(path):hl("MareToolName") })
						end
						out[#out + 1] = ch.diff and ui.Diff({ unified = ch.diff, path = path, lines = ch.lines })
							or ui.Diff({ old = ch.old or "", new = ch.new or "", path = path })
					end
				end
				return ui.Lines(out)
			end,
		})
	end,
}
