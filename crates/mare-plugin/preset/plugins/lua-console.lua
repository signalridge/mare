-- lua-console: `/lua` runs Lua in the *running* state (Neovim `:lua`, not WezTerm's
-- separate debug VM), for writing plugins and debugging: every mare API, the services
-- (`svc.<name>`) and `mare.g` are reachable. An expression's results are pretty-printed into the
-- scrollback; an incomplete chunk asks for another line (multi-line, like the `lua` REPL);
-- history lives in $XDG_STATE_HOME/mare/lua-history (WezTerm `repl-history`). TUI only: the
-- command isn't registered in headless/RPC mode and is marked local-only, so RPC `commands.run`
-- and peer connections can't reach it. Errors are only reported; plugins are unaffected.

return {
	name = "lua-console",
	-- `task` gives ctx:spawn, commands is followed below
	inject = { "ui", "task" },
	config_schema = {
		history_max = { type = "integer", default = 500, desc = "Most history entries kept." },
	},
	apply = function(ctx, cfg)
		if mare.mode ~= "tui" then
			return
		end
		local hist_path = mare.paths.state .. "/lua-history"

		local function load_history()
			local s = mare.fs.read(hist_path)
			local out = {}
			for line in (s or ""):gmatch("[^\n]+") do
				out[#out + 1] = line
			end
			return out
		end
		local function save_history(entry)
			local h = load_history()
			h[#h + 1] = (entry:gsub("\n", "\\n"))
			while #h > cfg.history_max do
				table.remove(h, 1)
			end
			pcall(mare.fs.write, hist_path, table.concat(h, "\n") .. "\n", { mode = 384 })
		end

		--- pretty-print a value (bounded depth and width)
		local function show(v, depth, seen)
			depth, seen = depth or 0, seen or {}
			local t = type(v)
			if t == "string" then
				return ("%q"):format(v)
			elseif t ~= "table" then
				return tostring(v)
			end
			if seen[v] then
				return "<cycle>"
			end
			if depth >= 4 then
				return "{…}"
			end
			seen[v] = true
			local parts, n = {}, 0
			local keys = {}
			for k in pairs(v) do
				keys[#keys + 1] = k
			end
			table.sort(keys, function(a, b)
				return tostring(a) < tostring(b)
			end)
			for _, k in ipairs(keys) do
				n = n + 1
				if n > 50 then
					parts[#parts + 1] = "…"
					break
				end
				local key = type(k) == "string" and k:match("^[%a_][%w_]*$") and k
					or ("[" .. show(k, depth + 1, seen) .. "]")
				parts[#parts + 1] = key .. " = " .. show(v[k], depth + 1, seen)
			end
			seen[v] = nil
			return "{ " .. table.concat(parts, ", ") .. " }"
		end

		-- the console's globals: the plugin's own env (so `mare` and everything else resolve),
		-- plus `svc.<name>` for services
		local env = setmetatable({
			svc = setmetatable({}, {
				__index = function(_, name)
					return ctx:get(name)
				end,
			}),
		}, { __index = _ENV })

		local function print_lines(prefix, text, hl)
			local lines = {}
			for line in (text .. "\n"):gmatch("(.-)\n") do
				lines[#lines + 1] = ui.Line({ ui.Span(prefix):hl("MareDim"), ui.Span(line):hl(hl) })
				prefix = string.rep(" ", #prefix)
			end
			mare.ui.print(ui.Lines(lines))
		end

		--- compile: an expression first (`return …`), else statements. nil, "incomplete" when the
		--- chunk ends early (more lines needed)
		local function compile(src)
			local f = load("return " .. src, "=lua", "t", env)
			if f then
				return f
			end
			local g, err = load(src, "=lua", "t", env)
			if g then
				return g
			end
			if err and err:find("<eof>", 1, true) then
				return nil, "incomplete"
			end
			return nil, err
		end

		local function run(src)
			print_lines("lua> ", src, "MareText")
			local f, err = compile(src)
			if not f then
				print_lines("  ", tostring(err), "MareError")
				return
			end
			-- The shared Lua VM intentionally does not expose `debug`; pcall retains multiple
			-- return values and reports errors without requiring that library.
			local res = table.pack(pcall(f))
			if not res[1] then
				print_lines("  ", tostring(res[2]), "MareError")
				return
			end
			if res.n > 1 then
				local out = {}
				for i = 2, res.n do
					out[#out + 1] = show(res[i])
				end
				print_lines("  ", table.concat(out, ", "), "MareAccent")
			end
		end

		local function console(first)
			local history = load_history()
			local buf = first
			while true do
				if not buf or buf == "" then
					buf = ui.input({
						title = "lua (Esc to close; " .. #history .. " in history)",
						placeholder = "expression or statements",
					})
					if not buf or buf == "" then
						return
					end
				end
				local _, err = compile(buf)
				while err == "incomplete" do
					local more =
						ui.input({ title = "lua … (continue; Esc cancels)", placeholder = buf:match("[^\n]*$") })
					if not more then
						buf = nil
						break
					end
					buf = buf .. "\n" .. more
					_, err = compile(buf)
				end
				if buf then
					save_history(buf)
					history[#history + 1] = buf
					run(buf)
				end
				buf = nil
				if first then
					return -- `/lua <code>`: one shot
				end
			end
		end

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("lua", {
				description = "Run Lua in mare's running state (/lua [code])",
				args = "[code]",
				auto_execute = true,
				local_only = true, -- never via RPC commands.run or peer connections
				run = function(args)
					ctx:spawn(function()
						console(args ~= "" and args or nil)
					end)
				end,
			})
		end)
	end,
}
