-- tools.file_tools: the read / write / edit tools and `ls` (registered after the shell tools
-- through register_ls); part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local MAX_LINES, MAX_BYTES = cfg.max_lines, cfg.max_bytes
	local tools = D.registry.tools
	local function abs(p)
		return D.file_guard.abs(p)
	end
	local function rel(p)
		return D.file_guard.rel(p)
	end
	local function short(p)
		return D.file_guard.short(p)
	end
	local function record(...)
		return D.file_guard.record(...)
	end
	local function version_of(...)
		return D.file_guard.version_of(...)
	end
	local function guard(...)
		return D.file_guard.guard(...)
	end
	local function commit(...)
		return D.file_guard.commit(...)
	end

	local function preview_read(path)
		local policy = mare.proc and mare.proc.sandbox and mare.proc.sandbox.current and mare.proc.sandbox.current()
		local policies = policy and { policy } or nil
		local st = mare.fs.stat(path, { policies = policies })
		if not st or st.kind ~= "file" then
			return nil
		end
		local cap = cfg.edit_max_bytes or cfg.max_bytes
		if st.size and st.size > cap then
			return nil
		end
		return mare.fs.read(path, { policies = policies, max_bytes = cap })
	end

	local function resolve_preview_path(raw_path, call_cwd)
		if type(raw_path) ~= "string" or raw_path == "" then
			return nil
		end
		if type(call_cwd) == "string" and call_cwd ~= "" and mare.fs.resolve_path then
			local ok, resolved = pcall(mare.fs.resolve_path, raw_path, call_cwd)
			if ok and type(resolved) == "table" and type(resolved.logical) == "string" and resolved.logical ~= "" then
				return resolved.logical
			end
		end
		return abs(raw_path)
	end

	local function edit_result_display(path, old, new, extra)
		local node = { old = mare.projection.project(old or ""), new = mare.projection.project(new), path = path }
		if type(extra) == "table" then
			if extra.full_path ~= nil then
				node.full_path = extra.full_path
			end
			if extra.is_new == true then
				node.is_new = true
			end
		end
		return ui.Diff(node)
	end

	-- edit / write results store the change, not both texts (one 8 MB edit wrote
	-- 17 MB into the session). `details.version = 2`: `diff` = mare.text.diff of the old and new
	-- text (3 lines of context — what the diff view shows), `lines` = the longer side's line
	-- count (the gutter width). A result without `version` (older sessions) carries `old` / `new`.
	-- Returns the details, added and removed line counts (one diff for all three).
	local function stored_change(path, old, new)
		local diff, add, del, lines =
			mare.text.diff(mare.projection.project(old), mare.projection.project(new), { path = rel(path) })
		return { version = 2, path = path, diff = diff, lines = lines }, add, del
	end
	--- the diff view of a stored result, either version — identical for the same change
	local function stored_display(path, d)
		if d.version == 2 then
			return ui.Diff({ unified = d.diff or "", path = path, lines = d.lines })
		end
		return edit_result_display(path, d.old, d.new)
	end

	local IMAGE_EXT =
		{ png = "image/png", jpg = "image/jpeg", jpeg = "image/jpeg", gif = "image/gif", webp = "image/webp" }

	tools.register(tools, ctx, "read", {
		access = function(a)
			return { { kind = "file", op = "read", path = a.path, recursive = false } }
		end,
		side_effect = "read",
		idempotent = true,
		snippet = "Read file contents",
		guidelines = { "Use read to examine files instead of cat or sed." },
		description = function(_, mode)
			return "Read a file. Output is truncated to "
				.. MAX_LINES
				.. " lines or 50KB; use offset/limit for large files. Read a file before editing it. "
				.. "After a successful edit or write, do not re-read the file solely to confirm the change landed."
		end,
		parameters = {
			type = "object",
			properties = {
				path = { type = "string", description = "Path to the file (relative or absolute)" },
				offset = { type = "integer", description = "Line to start from (1-indexed)" },
				limit = { type = "integer", description = "Maximum number of lines to read" },
			},
			required = { "path" },
		},
		summary = function(a)
			return ("Read(%s)"):format(short(a.path))
		end,
		execute = function(a, t)
			if a.offset ~= nil and (type(a.offset) ~= "number" or a.offset < 1 or math.floor(a.offset) ~= a.offset) then
				return { content = "read: offset must be a positive integer (>= 1)", is_error = true }
			end
			if a.limit ~= nil and (type(a.limit) ~= "number" or a.limit < 1 or math.floor(a.limit) ~= a.limit) then
				return { content = "read: limit must be a positive integer (>= 1)", is_error = true }
			end
			local path = abs(a.path)
			local st = mare.fs.stat(path)
			if not st then
				record(path, { kind = "absent" }) -- a later write may create it
				error("File not found: " .. a.path)
			end
			if st.kind == "dir" then
				local names = {}
				for _, e in ipairs(mare.fs.list(path) or {}) do
					names[#names + 1] = e.name .. (e.kind == "dir" and "/" or "")
				end
				return { content = table.concat(names, "\n") }
			end
			if st.kind ~= "file" then
				return { content = ("Cannot read %s: not a regular file"):format(a.path), is_error = true }
			end
			local ext = string.lower(string.match(path, "%.(%w+)$") or "")
			if IMAGE_EXT[ext] and mare.image then
				tools.observe(tools, ctx, path)
				local img, err = mare.image.load(path, { max_dim = cfg.image_max_dim })
				if not img then
					return { content = "Cannot load image: " .. tostring(err), is_error = true }
				end
				return {
					content = {
						{
							type = "text",
							text = ("Image %s (%dx%d)"):format(rel(path), img.width or 0, img.height or 0),
						},
						{ type = "image", data = img.data, mime = img.mime, path = a.path },
					},
					details = { image = true, lines = 0 },
				}
			end
			-- binary files are refused with their type and size, before loading them
			local sn = mare.fs.sniff(path)
			if sn.binary then
				return {
					content = ("Cannot read %s: it is a binary file (%s, %d bytes). Use a tool that understands this format."):format(
						a.path,
						sn.kind,
						sn.size
					),
					is_error = true,
					error_code = "binary_file",
				}
			end
			local offset = a.offset or 1
			local limit = a.limit or MAX_LINES
			local not_text = {
				content = ("Cannot read %s: invalid UTF-8 text. read shows UTF-8 text files only; convert it (e.g. iconv) or inspect it with a tool that knows its encoding."):format(
					a.path
				),
				is_error = true,
				error_code = "not_text",
			}
			local sel, total
			if (st.size or 0) >= cfg.read_stream_min_bytes then
				-- large file: stream it, keep only the window
				local w, werr = mare.fs.read_window(path, { offset = offset, limit = limit })
				if not w then
					if string.find(tostring(werr), "(not_text)", 1, true) then
						return not_text
					end
					error(werr, 0)
				end
				record(path, { kind = "present", size = w.size, mtime = st.mtime, hash = w.hash })
				sel, total = w.lines, w.total
			else
				local data = assert(mare.fs.read(path, { max_bytes = cfg.read_stream_min_bytes }))
				if not mare.text.is_utf8(data) then
					return not_text
				end
				record(path, (version_of(path, data)))
				-- only the window's lines become strings
				sel, total = mare.text.split_lines(data, offset, offset + limit - 1)
			end
			local MAX_LINE_BYTES = cfg.read_max_line_bytes
			for i = 1, #sel do
				local line = sel[i]
				if #line > MAX_LINE_BYTES then
					sel[i] = mare.text.cut(line, MAX_LINE_BYTES)
						.. " ... [line truncated at "
						.. MAX_LINE_BYTES
						.. " bytes]"
				end
			end
			local body = table.concat(sel, "\n")
			-- head truncation by bytes too
			local cut, info = mare.text.truncate(body, { max_lines = limit, max_bytes = MAX_BYTES, keep = "head" })
			local last = math.min(total, offset + limit - 1)
			if info.truncated then
				body = string.gsub(cut, "\n%[… %d+ lines omitted …%]$", "")
				last = offset + info.shown_lines - 1
			end
			if last < total then
				body = body
					.. ("\n\n[lines %d-%d of %d; continue with offset=%d]"):format(offset, last, total, last + 1)
			end
			return {
				content = body,
				no_truncate = true,
				details = { lines = math.max(0, last - offset + 1), total = total },
			}
		end,
		render = function(a, res)
			local n = res.details and res.details.lines or 0
			return ui.Line({ ui.Span(("Read %d line%s"):format(n, n == 1 and "" or "s")):hl("MareMuted") })
		end,
	})

	tools.register(tools, ctx, "write", {
		access = function(a)
			return {
				{ kind = "file", op = "read", path = a.path, recursive = false },
				{ kind = "file", op = "write", path = a.path, recursive = false },
				{ kind = "file", op = "create", path = a.path, recursive = false },
			}
		end,
		sensitive_inputs = { "content" },
		side_effect = "write",
		idempotent = true, -- writing the same content twice leaves the same file
		snippet = "Create or overwrite files",
		guidelines = { "Use write only for new files or complete rewrites." },
		-- the second sentence prevents truncated writes
		description = "Write content to a file, creating parent directories. To overwrite an existing file, read it first."
			.. " `content` is written exactly as given — always the complete file, never a placeholder or an excerpt.",
		parameters = {
			type = "object",
			properties = {
				path = { type = "string", description = "File path" },
				content = { type = "string", description = "The complete file content." },
			},
			required = { "path", "content" },
		},
		summary = function(a)
			return ("Write(%s)"):format(short(a.path))
		end,
		execute = function(a)
			local path = abs(a.path)
			return tools.with_file_lock(tools, ctx, { path }, function()
				local cur, gerr = guard(path, "write", "write")
				if not cur then
					return { content = gerr, is_error = true, error_code = "fs_guard" }
				end
				local old = cur.data
				local ok, werr = commit(path, a.content, cur, "write")
				if not ok then
					return { content = werr, is_error = true, error_code = "fs_guard" }
				end
				local lines = #mare.text.split_lines(a.content)
				-- a new file: its content is the call's own `content` argument, stored once; old
				-- bytes that aren't UTF-8 have no text diff (`replaced`: the view shows the result text)
				local details = { version = 2, path = path, replaced = old and true or nil }
				if old and mare.text.is_utf8(old) then
					details = stored_change(path, old, a.content)
				end
				return {
					content = (old and "Updated " or "Created ")
						.. rel(path)
						.. (" (%d line%s)"):format(lines, lines == 1 and "" or "s"),
					details = details,
					display = details.diff and stored_display(rel(path), details) or nil,
				}
			end)
		end,
		-- the approval dialog's diff: the file as it is against the new content
		preview = function(a, call_cwd)
			local path = resolve_preview_path(a.path, call_cwd)
			if not path then
				return nil
			end
			local old = preview_read(path)
			local is_new = false
			if old == nil then
				if mare.fs.exists(path) then
					return nil
				end
				old = ""
				is_new = true
			end
			return edit_result_display(rel(path), old, a.content or "", { full_path = path, is_new = is_new })
		end,
		render = function(a, res, expanded)
			if res.is_error then
				return nil
			end
			local d = res.details or {}
			if d.old or d.diff then
				return stored_display(rel(d.path), d)
			elseif d.replaced then
				return nil
			end
			-- a created file: version 2 keeps no copy of the content (it is the call's argument)
			local new = d.version == 2 and a.content or d.new or ""
			local n = #mare.text.split_lines(new)
			if expanded then
				return ui.Code(new, string.match(d.path or "", "%.(%w+)$") or "")
			end
			return ui.Line({ ui.Span(("Wrote %d line%s"):format(n, n == 1 and "" or "s")):hl("MareMuted") })
		end,
	})

	local replace_params = {
		type = "object",
		properties = {
			path = { type = "string", description = "Path to the file to edit" },
			old_string = {
				type = "string",
				description = "Exact text to replace (must be unique unless replace_all)",
			},
			new_string = { type = "string", description = "Replacement text" },
			replace_all = {
				type = "boolean",
				description = "Replace every occurrence instead of requiring a single match",
			},
		},
		required = { "path", "old_string", "new_string" },
	}

	tools.register(tools, ctx, "edit", {
		access = function(a)
			return {
				{ kind = "file", op = "read", path = a.path, recursive = false },
				{ kind = "file", op = "write", path = a.path, recursive = false },
			}
		end,
		sensitive_inputs = { "old_string", "new_string" },
		side_effect = "write",
		idempotent = false,
		enabled = function(_, mode)
			return mode == "replace"
		end,
		snippet = "Make precise file edits",
		guidelines = { "Read a file before editing it; prefer edit over write for changes to existing files." },
		description = "Replace exact text in a file you have read. old_string must match exactly once unless replace_all.",
		parameters = replace_params,
		summary = function(a)
			return ("Edit(%s)"):format(short(a.path))
		end,
		execute = function(a)
			local path = abs(a.path)
			if not mare.fs.exists(path) then
				return { content = "File not found: " .. a.path, is_error = true }
			end
			return tools.with_file_lock(tools, ctx, { path }, function()
				local cur, gerr = guard(path, "edit", "edit")
				if not cur then
					return { content = gerr, is_error = true, error_code = "fs_guard" }
				end
				local old = cur.data
				local new, e = mare.text.replace(old, a.old_string or "", a.new_string or "", a.replace_all)
				if not new then
					local msg = tostring(e)
					return {
						content = msg,
						is_error = true,
						-- distinguish "several matches" from "not found"
						error_code = string.find(msg, "^ambiguous_edit") and "ambiguous_edit" or "edit_not_found",
					}
				end
				-- an edit that leaves the bytes unchanged is a mistake, not a success
				if new == old then
					return {
						content = ("edit %s: the replacement leaves the file unchanged"):format(rel(path)),
						is_error = true,
					}
				end
				local ok, werr = commit(path, new, cur, "edit")
				if not ok then
					return { content = werr, is_error = true, error_code = "fs_guard" }
				end
				local details, add, del = stored_change(path, old, new)
				local out = ("Edited %s (+%d -%d)"):format(rel(path), add, del)
				return { content = out, details = details }
			end)
		end,
		-- the approval dialog's diff: the replacement applied in memory (nothing when it
		-- wouldn't apply — the call then fails with its own error)
		preview = function(a, call_cwd)
			local path = resolve_preview_path(a.path, call_cwd)
			if not path then
				return nil
			end
			local old = preview_read(path)
			local new = old and mare.text.replace(old, a.old_string or "", a.new_string or "", a.replace_all)
			return new and edit_result_display(rel(path), old, new, { full_path = path }) or nil
		end,
		render = function(a, res)
			if res.is_error then
				return nil
			end
			local d = res.details or {}
			return stored_display(rel(d.path or a.path), d)
		end,
	})

	local function register_ls()
		tools.register(tools, ctx, "ls", {
			access = function(a)
				return { { kind = "file", op = "read", path = a.path or ".", recursive = false } }
			end,
			side_effect = "read",
			snippet = "List directory contents",
			description = "List a directory (dirs end with /). Includes dotfiles; up to "
				.. cfg.ls_limit
				.. " entries by default.",
			parameters = {
				type = "object",
				properties = {
					path = { type = "string", description = "Directory (default: cwd)" },
					limit = { type = "integer", description = "Maximum entries (default " .. cfg.ls_limit .. ")" },
				},
			},
			summary = function(a)
				return ("List(%s)"):format(short(a.path or "."))
			end,
			execute = function(a)
				local dir = abs(a.path or ".")
				local st = mare.fs.stat(dir)
				if not st then
					return { content = "Not found: " .. (a.path or "."), is_error = true }
				end
				if st.kind ~= "dir" then
					return { content = "Not a directory: " .. (a.path or "."), is_error = true }
				end
				local limit = a.limit or cfg.ls_limit
				local out, n = {}, 0
				for _, e in ipairs(mare.fs.list(dir) or {}) do
					n = n + 1
					if n <= limit then
						out[#out + 1] = e.name .. (e.kind == "dir" and "/" or "")
					end
				end
				if n > limit then
					out[#out + 1] = ("[… %d more entr%s]"):format(n - limit, n - limit == 1 and "y" or "ies")
				end
				if n == 0 then
					out[1] = "(empty directory)"
				end
				return { content = table.concat(out, "\n"), details = { count = n } }
			end,
			render = function(_, res)
				local count = res.details and res.details.count or 0
				return ui.Line({
					ui.Span(("Listed %d entr%s"):format(count, count == 1 and "y" or "ies")):hl("MareMuted"),
				})
			end,
		})
	end

	return { register_ls = register_ls }
end
