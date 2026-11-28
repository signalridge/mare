-- commands.builtins: built-in slash command registrations;
-- part of the `commands` plugin (plugins/commands.lua).
local M = {}

local function aliases_of(stacks, name)
	local out = {}
	for n, st in pairs(stacks) do
		local d = st[#st]
		if d.alias_of == name then
			out[#out + 1] = "/" .. n
		end
	end
	table.sort(out)
	return out
end

-- local helper keeps the `alias(commands, "x", "y")` / `alias("x", "y")` convention cleanly shared
local function bind_alias(commands, n, target)
	commands:register(n, { alias_of = target })
end

local function browse(commands, run_local, title, filter)
	local list = {}
	for _, d in ipairs(commands:list().data) do
		if filter(d.name) then
			list[#list + 1] = d
		end
	end
	if #list == 0 then
		ui.notify(title .. ": nothing to show", "info")
		return
	end
	local items = {}
	for i, d in ipairs(list) do
		items[i] = { label = "/" .. d.name, detail = d.description }
	end
	local i = ui.select({ title = title .. " (type to filter, Enter runs)", items = items })
	if i then
		local line = "/" .. list[i].name
		if mare.mode == "tui" and run_local then
			run_local(line)
		else
			local ok, err = pcall(commands.run, commands, line)
			if not ok and err ~= nil and not (type(err) == "table" and err.code == "commands.command_unavailable") then
				ui.notify(tostring(err), "warning")
			end
		end
	end
end

local function register_core(ctx, commands, stacks, registry)
	local function reg(n, d)
		commands:register(n, d)
	end
	local function alias(n, target)
		bind_alias(commands, n, target)
	end

	reg("help", {
		description = "Show commands and keyboard shortcuts",
		run = function()
			local connected = ui.connected and ui.connected()
			local list = connected and commands:list().data or commands:list({ available = true }).data
			---@type mare.Node[]
			local lines = { ui.Line({ ui.Span("Commands"):hl("MareHeading") }) }
			local plain = { "Commands:" }
			for _, d in ipairs(list) do
				local al = aliases_of(stacks, d.name)
				local alias_text = #al > 0 and ("  (" .. table.concat(al, ", ") .. ")") or ""
				lines[#lines + 1] = ui.Line({
					ui.Span(("/%-14s"):format(d.name)):hl("MareAccent"),
					ui.Span(d.description or ""):hl("MareMuted"),
					ui.Span(alias_text):hl("MareDim"),
				})
				plain[#plain + 1] = ("/%-14s %s%s"):format(d.name, d.description or "", alias_text)
			end
			lines[#lines + 1] = ui.Gap()
			for _, l in ipairs(require("commands.keys").lines(16)) do
				lines[#lines + 1] = l
			end
			ui.overlay("Help", ui.Lines(lines))
			if not connected then
				return table.concat(plain, "\n")
			end
		end,
	})
	alias("h", "help")
	alias("?", "help")

	reg("exit", {
		description = "Exit mare",
		run = function()
			ui.exit()
		end,
	})
	alias("quit", "exit")
	alias("q", "exit")

	-- /permissions = /permission. Registered now, not on app/ready (a reload
	-- of commands gets no replay of it): hidden from listings like every alias, it reaches
	-- /permission when the permissions plugin has one, and a plugin's own /permissions shadows it
	alias("permissions", "permission")

	reg("status", {
		description = "Show version, model, session and working directory",
		run = function()
			return require("commands.status")(ctx)
		end,
	})

	reg("init", {
		description = "Analyze the codebase and create or update AGENTS.md",
		availability = "idle",
		run = function()
			local prompt = "Analyze this codebase and create (or update, if it exists) an AGENTS.md file at the "
				.. "repository root for future coding agents. Include: build, lint and test commands (and how to run "
				.. "a single test); a concise architecture overview (the big picture that needs several files to "
				.. "understand); code style and conventions actually used in the repo. Be concise; don't list every "
				.. "file or repeat obvious instructions. If AGENTS.md or CLAUDE.md exists, improve it instead."
			ctx:spawn(function()
				ctx.bus:emit("input/submit", { text = prompt })
			end)
		end,
	})

	reg("export", {
		description = "Export the conversation as Markdown (/export [path])",
		run = function(args)
			return require("commands.export")(ctx, args)
		end,
	})
	alias("export-md", "export")

	reg("config", {
		description = "Show options, their values and where each was set",
		run = function(args)
			return require("commands.config")(ctx, args)
		end,
	})
	alias("settings", "config")

	reg("model", {
		description = "Select model",
		args = "[provider/id]",
		auto_execute = true,
		complete = function(prefix)
			local out = {}
			for _, m in ipairs(ctx:get("llm"):models()) do
				if string.find(m.ref, prefix, 1, true) then
					out[#out + 1] = { label = m.ref, insert = m.ref }
				end
			end
			return out
		end,
		run = function(args)
			return require("commands.model")(ctx, args)
		end,
	})
end

local function register_browse_and_reasoning(ctx, commands, run_local)
	local function reg(n, d)
		commands:register(n, d)
	end

	reg("commands", {
		description = "Browse slash commands; Enter runs one",
		availability = "tui",
		run = function()
			browse(commands, run_local, "Commands", function(n)
				return string.sub(n, 1, 6) ~= "skill:"
			end)
		end,
	})

	reg("skills", {
		description = "Browse skills; Enter loads one",
		availability = "tui",
		run = function()
			browse(commands, run_local, "Skills", function(n)
				return string.sub(n, 1, 6) == "skill:"
			end)
		end,
	})

	local show_reasoning = false
	reg("show-reasoning", {
		description = "Toggle showing full reasoning instead of “Thought for N s”",
		availability = "tui",
		run = function(args)
			if args == "on" then
				show_reasoning = true
			elseif args == "off" then
				show_reasoning = false
			else
				show_reasoning = not show_reasoning
			end
			ui.flag("show_reasoning", show_reasoning)
			ui.notify("Reasoning: " .. (show_reasoning and "shown" or "folded"))
		end,
	})

	reg("effort", {
		description = "Set reasoning effort (/effort [level] [--session]; lists the model's levels)",
		args = "[level]",
		auto_execute = true,
		complete = function(prefix)
			local out = {}
			local m = ctx:get("llm") and ctx:get("llm"):current()
			for _, e in ipairs(require("commands.effort").levels(m, ctx:get("llm")) or {}) do
				if string.find(e, prefix, 1, true) == 1 then
					out[#out + 1] = { label = e, insert = e }
				end
			end
			return out
		end,
		run = function(args)
			return require("commands.reasoning")(ctx, args)
		end,
	})
end

local function register_ops(ctx, commands)
	local function reg(n, d)
		commands:register(n, d)
	end

	reg("doctor", {
		description = "Check the installation, credentials, plugins and MCP servers",
		run = function()
			ctx:spawn(function()
				require("commands.doctor")(ctx)
			end)
		end,
	})

	reg("theme", {
		description = "Pick the theme flavor (auto / dark / light / installed flavors)",
		args = "[flavor]",
		auto_execute = true,
		run = function(args)
			ctx:spawn(function()
				require("commands.theme")(ctx, args)
			end)
		end,
	})

	reg("terminal-setup", {
		description = "Check this terminal's keys (Shift+Enter, Alt+Enter) and show how to fix them",
		availability = "tui",
		run = function()
			require("commands.terminal_setup")(ctx)
		end,
	})

	reg("memory", {
		description = "Show the instruction files (AGENTS.md) in effect; /memory add [--user] <text>",
		args = "[add [--user] <text>]",
		auto_execute = true,
		run = function(args)
			ctx:spawn(function()
				require("commands.memory")(ctx, args)
			end)
		end,
	})

	reg("reload", {
		description = "Reload plugins (/reload [plugin]) or the configuration (/reload config)",
		run = function(args)
			return require("commands.reload")(ctx, args)
		end,
	})

	ctx:on("reload/completed", function(r)
		local parts = { ("Reloaded %d plugin(s)"):format(#(r.reloaded or {})) }
		if #(r.added or {}) > 0 then
			parts[#parts + 1] = "started " .. table.concat(r.added, ", ")
		end
		if #(r.removed or {}) > 0 then
			parts[#parts + 1] = "unloaded " .. table.concat(r.removed, ", ")
		end
		ui.notify(table.concat(parts, " · "), "success")
		for _, n in ipairs(r.notices or {}) do
			ui.notify(n, "warning")
		end
	end)

	reg("hotkeys", {
		description = "Show key bindings",
		availability = "tui",
		run = function()
			return require("commands.hotkeys")(ctx)
		end,
	})

	reg("login", {
		description = "Log in to a provider (/login [provider])",
		run = function(args)
			return require("commands.login")(ctx, args)
		end,
	})

	reg("add-dir", {
		description = "Add a directory to the workspace (/add-dir <path>; no arg lists them)",
		run = function(args)
			return require("commands.add_dir")(ctx, args)
		end,
	})

	reg("debug", {
		description = "Show recent log lines and the latest crash report",
		run = function()
			return require("commands.debug")(ctx)
		end,
	})

	reg("logout", {
		description = "Remove a provider's stored credentials (/logout [provider])",
		complete = function(prefix)
			local out = {}
			for _, n in ipairs(ctx:get("llm"):logged_in()) do
				if n:find(prefix, 1, true) == 1 then
					out[#out + 1] = { label = n, insert = n }
				end
			end
			return out
		end,
		run = function(args)
			return require("commands.logout")(ctx, args)
		end,
	})
end

function M.register(ctx, commands, stacks, registry, run_local)
	register_core(ctx, commands, stacks, registry)
	register_browse_and_reasoning(ctx, commands, run_local)
	register_ops(ctx, commands)
end

return M
