-- copy: /copy and copying single messages. The clipboard path itself is the core
-- primitive mare.clipboard.copy (tmux / OSC 52 over SSH, else native, WSL PowerShell, terminal);
-- this plugin decides *what* is copied:
--   * the last response's Markdown source (not the rendered text); `/copy N` = the Nth-last one
--   * with code blocks, a picker: each block (language + first line) and "Full response";
--     `Ctrl+A` there also remembers "always copy the full response" (mare.o.copy.always_full)
--   * `Ctrl+W` in the picker writes the choice to a file in the working directory instead
--   * the full response gets its tables re-aligned as GFM pipe tables (paste-friendly)
--   * `copy/request {text}` (emitted by the Ctrl+T overlay's `y` and /tree's Ctrl+Y) copies that text
-- No default key: Ctrl+O already toggles compact/expanded; bind `copy.last` with mare.keymap.set.
-- Events: copy/completed { method, bytes, target, verified }. The `copy` service can be replaced.

return {
	name = "copy",
	provide = { "copy" },
	-- `task` gives ctx:spawn, `bus` the mare events; agent and
	-- commands are optional (read / followed when used)
	inject = { "llm", "task", "bus" },
	-- /copy: the last response (or a code block) to the clipboard
	config_schema = {
		-- also settable as mare.o.copy = { always_full = true }; `Ctrl+A` in the picker saves it
		always_full = { type = "boolean", default = false, desc = "Always copy the full text." },
	},
	apply = function(ctx, cfg)
		local ui = mare.ui
		local function always_full()
			local o = (mare.o or {}).copy
			if type(o) == "table" and o.always_full ~= nil then
				return o.always_full == true
			end
			return cfg.always_full == true or (mare.state.read() or {}).copy_always_full == true
		end

		local M = {}

		--- blocks(markdown) -> { {lang, code, first}, ... } fenced code blocks, in order
		function M.blocks(_, _, md)
			local out = {}
			local lines = {}
			for l in ((md or "") .. "\n"):gmatch("(.-)\r?\n") do
				lines[#lines + 1] = l
			end
			local i = 1
			while i <= #lines do
				local indent, fence, info = lines[i]:match("^( ? ? ?)(```+)%s*(.-)%s*$")
				if not fence then
					indent, fence, info = lines[i]:match("^( ? ? ?)(~~~+)%s*(.-)%s*$")
				end
				if fence then
					local body = {}
					local j = i + 1
					local close = "^ ? ? ?" .. fence:sub(1, 1):rep(#fence) .. "%s*$"
					while j <= #lines and not lines[j]:match(close) do
						body[#body + 1] = lines[j]
						j = j + 1
					end
					local code = table.concat(body, "\n")
					local first = ""
					for _, b in ipairs(body) do
						if b:match("%S") then
							first = b:gsub("^%s+", "")
							break
						end
					end
					out[#out + 1] = { lang = (info or ""):match("^(%S*)") or "", code = code, first = first }
					i = j + 1
				else
					i = i + 1
				end
			end
			return out
		end

		--- align_tables(markdown) -> markdown with every GFM table re-padded so columns line up
		function M.align_tables(_, _, md)
			local lines = {}
			for l in ((md or "") .. "\n"):gmatch("(.-)\r?\n") do
				lines[#lines + 1] = l
			end
			if lines[#lines] == "" then
				lines[#lines] = nil
			end
			local function cells(l)
				local s = l:gsub("^%s*|", ""):gsub("|%s*$", "")
				local out = {}
				for c in (s .. "|"):gmatch("(.-)|") do
					out[#out + 1] = c:gsub("^%s+", ""):gsub("%s+$", "")
				end
				return out
			end
			local function is_sep(l)
				return l:match("^%s*|?%s*:?%-+:?%s*|") ~= nil or l:match("^%s*:?%-+:?%s*|%s*:?%-+") ~= nil
			end
			local out, i, in_fence = {}, 1, false
			while i <= #lines do
				local l = lines[i]
				if l:match("^%s*```") or l:match("^%s*~~~") then
					in_fence = not in_fence
				end
				if not in_fence and l:find("|", 1, true) and lines[i + 1] and is_sep(lines[i + 1]) then
					local rows, j = { cells(l) }, i + 2
					local seps = cells(lines[i + 1])
					while j <= #lines and lines[j]:find("|", 1, true) and lines[j]:match("%S") do
						rows[#rows + 1] = cells(lines[j])
						j = j + 1
					end
					local ncol = #seps
					for _, r in ipairs(rows) do
						ncol = math.max(ncol, #r)
					end
					local width = {}
					for c = 1, ncol do
						width[c] = 3
						for _, r in ipairs(rows) do
							width[c] = math.max(width[c], mare.text.width(r[c] or ""))
						end
					end
					local function align(c)
						local sp = seps[c] or ""
						local l2, r2 = sp:sub(1, 1) == ":", sp:sub(-1) == ":"
						return l2 and r2 and "center" or r2 and "right" or "left"
					end
					local function pad(text, c)
						local gap = width[c] - mare.text.width(text)
						local a = align(c)
						if a == "right" then
							return string.rep(" ", gap) .. text
						elseif a == "center" then
							local left = math.floor(gap / 2)
							return string.rep(" ", left) .. text .. string.rep(" ", gap - left)
						end
						return text .. string.rep(" ", gap)
					end
					local function row(r)
						local parts = {}
						for c = 1, ncol do
							parts[c] = pad(r[c] or "", c)
						end
						return "| " .. table.concat(parts, " | ") .. " |"
					end
					out[#out + 1] = row(rows[1])
					local sep = {}
					for c = 1, ncol do
						local a = align(c)
						local dash = string.rep("-", width[c])
						if a == "center" then
							dash = ":" .. string.rep("-", width[c] - 2) .. ":"
						elseif a == "right" then
							dash = string.rep("-", width[c] - 1) .. ":"
						end
						sep[c] = dash
					end
					out[#out + 1] = "| " .. table.concat(sep, " | ") .. " |"
					for k = 2, #rows do
						out[#out + 1] = row(rows[k])
					end
					i = j
				else
					out[#out + 1] = l
					i = i + 1
				end
			end
			return table.concat(out, "\n")
		end

		--- response(n) -> Markdown source of the Nth-last assistant response (1 = last), or nil
		function M.response(_, _, n)
			local agent = ctx:get("agent")
			if not agent then
				return nil
			end
			n = math.max(1, math.floor(tonumber(n) or 1))
			local msgs = agent:list_messages().data
			local seen = 0
			for i = #msgs, 1, -1 do
				if msgs[i].role == "assistant" then
					local t = ctx.llm:text_of(msgs[i])
					if t ~= "" then
						seen = seen + 1
						if seen == n then
							return t
						end
					end
				end
			end
			return nil
		end

		local function human(bytes)
			return bytes >= 1024 and ("%.1f KB"):format(bytes / 1024) or (bytes .. " bytes")
		end

		--- text(text, target?) -> result of mare.clipboard.copy; tells the user which path was used
		--- and never claims "copied" for OSC 52 (unverifiable). `target` names what was copied.
		function M.text(_, _, text, target)
			target = target or "text"
			local r = mare.clipboard.copy(text)
			if r.ok then
				local how = ({
					native = "system clipboard",
					tmux = "tmux clipboard",
					wsl = "Windows clipboard (via WSL)",
				})[r.method]
				if r.method == "osc52" then
					ui.notify(
						("Sent %s (%s) to the terminal clipboard — it depends on the terminal allowing OSC 52"):format(
							target,
							human(r.bytes)
						),
						"info"
					)
				else
					ui.notify(("Copied %s (%s) to the %s"):format(target, human(r.bytes), how), "success")
				end
			else
				local hint = r.bytes > mare.clipboard.max_osc52_bytes
						and " — use `w` in the /copy picker to write a file"
					or ""
				ui.notify(("Could not copy %s: %s%s"):format(target, tostring(r.error), hint), "error")
			end
			ctx.bus:emit(
				"copy/completed",
				{ method = r.method, bytes = r.bytes, target = target, ok = r.ok, verified = r.verified }
			)
			return r
		end

		--- write(text, lang?) -> path: mare-copy-<time>.<ext> in the working directory
		local EXT = {
			lua = "lua",
			rust = "rs",
			rs = "rs",
			python = "py",
			py = "py",
			javascript = "js",
			js = "js",
			typescript = "ts",
			ts = "ts",
			tsx = "tsx",
			jsx = "jsx",
			go = "go",
			sh = "sh",
			bash = "sh",
			zsh = "sh",
			shell = "sh",
			json = "json",
			yaml = "yaml",
			yml = "yaml",
			toml = "toml",
			html = "html",
			css = "css",
			sql = "sql",
			c = "c",
			cpp = "cpp",
			java = "java",
			ruby = "rb",
			diff = "diff",
			markdown = "md",
			md = "md",
		}
		function M.write(_, _, text, lang)
			local ext = lang and EXT[lang:lower()] or (lang and lang:match("^%w+$") and lang:lower()) or "md"
			local path = ("%s/mare-copy-%s.%s"):format(mare.cwd, mare.date("%Y%m%d-%H%M%S"), ext)
			local ok, err = pcall(mare.fs.write, path, text)
			if ok then
				ui.notify("Wrote " .. path, "success")
				ctx.bus:emit(
					"copy/completed",
					{ method = "file", bytes = #text, target = path, ok = true, verified = true }
				)
				return path
			end
			ui.notify("Could not write " .. path .. ": " .. tostring(err), "error")
			return nil
		end

		--- last(n?) -> copy the Nth-last response; with code blocks, the picker first
		function M.last(_, _, n)
			local md = M.response(M, nil, n)
			if not md then
				ui.notify("Nothing to copy yet", "warning")
				return nil
			end
			local full = M.align_tables(M, nil, md)
			local blocks = M.blocks(M, nil, md)
			-- only one code block and nothing else: copy it directly
			if #blocks == 1 then
				local rest = md:gsub("```.-```", ""):gsub("~~~.-~~~", "")
				if not rest:match("%S") then
					return M.text(M, nil, blocks[1].code, "the code block")
				end
			end
			if #blocks == 0 or always_full() or not ui.connected() then
				return M.text(M, nil, full, "the response")
			end
			local items = {}
			for i, b in ipairs(blocks) do
				local n = select(2, b.code:gsub("\n", "")) + 1
				items[i] = {
					label = ("%s  %s"):format(b.lang ~= "" and b.lang or "code", mare.text.trunc_width(b.first, 60)),
					detail = ("block %d · %d line%s"):format(i, n, n == 1 and "" or "s"),
				}
			end
			items[#items + 1] = { label = "Full response", detail = "Markdown, tables aligned" }
			local i, _, key = ui.select({
				title = "Copy which part?",
				items = items,
				selected = #items,
				keys = {
					{ key = "ctrl+w", label = "write to file" },
					{ key = "ctrl+a", label = "always full response" },
				},
			})
			if not i then
				return nil
			end
			if key == "ctrl+a" then
				pcall(mare.state.set, "copy_always_full", true) -- locked, atomic; never over a corrupt file
				ui.notify(
					"From now on /copy copies the full response (mare.o.copy = { always_full = false } undoes it)"
				)
				return M.text(M, nil, full, "the response")
			end
			local b = blocks[i]
			local text = b and b.code or full
			if key == "ctrl+w" then
				return M.write(M, nil, text, b and b.lang ~= "" and b.lang or nil)
			end
			return M.text(M, nil, text, b and ("code block " .. i) or "the response")
		end

		-- a mare.Service
		require("mare_lib").service(ctx, "copy", M)

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("copy", {
				description = "Copy the last response (/copy N: the Nth-last) — a code block or the whole Markdown",
				run = function(args)
					local n = tonumber((args or ""):match("^%s*(%d+)"))
					ctx:spawn(function()
						M.last(M, nil, n)
					end)
				end,
			})
		end)
		-- a keymap action users can bind, no default key: mare.keymap.set("editor", "<M-c>", "copy.last")
		local action = mare.keymap.action("copy.last", function()
			ctx:spawn(function()
				M.last(M, nil, 1)
			end)
		end, { desc = "copy the last response" })
		ctx:effect(function()
			return action.remove
		end, "copy.last action")
		-- other views ask for a copy: the Ctrl+T overlay (`y`) and /tree (`y`)
		ctx:on("copy/request", function(ev)
			if type(ev) == "table" and type(ev.text) == "string" and ev.text ~= "" then
				local what = ev.kind == "user" and "the message"
					or (ev.kind and ("the " .. ev.kind .. " message") or "the message")
				ctx:spawn(function()
					M.text(M, nil, ev.text, what)
				end)
			end
		end)
	end,
}
