-- /memory: implementation of the command registered by plugins/commands.lua; loaded on first use
-- . Runs in the commands plugin's env. The instruction files ("memory": AGENTS.md
-- and related instruction files) that the context plugin loads — the user file in
-- the config dir and the project chain — with sizes; Enter shows one. `/memory add [--user]
-- <text>` appends a bullet to the project's AGENTS.md (or the user one), creating it if needed.
-- The system prompt is built once per session: an added note
-- reaches the model from the next session (/new) on, and the command says so.

-- a path's parent directory: Windows paths (mare.cwd, the context plugin's file paths) use `\`
-- and may mix in `/`; elsewhere `\` is an ordinary file name character (as in
-- context/instructions.lua)
local PARENT = mare.os == "windows" and "^(.*)[/\\][^/\\]+$" or "^(.*)/[^/]+$"

local function project_root()
	local d = mare.cwd
	while d and d ~= "" do
		local gitdir, stop = mare.fs.git_dir(d)
		if gitdir then
			return d
		elseif stop then
			break -- an invalid `.git` file: not a repository, as Git's discovery sees it
		end
		d = string.match(d, PARENT)
	end
	return mare.cwd
end

local function add(ctx, args)
	local user = false
	if args == "--user" or string.find(args, "^%-%-user%s") then
		user = true
		args = string.match(args, "^%-%-user%s*(.*)$")
	end
	local text = string.match(args or "", "^%s*(.-)%s*$")
	if text == "" then
		ui.notify("Usage: /memory add [--user] <text>", "warning")
		return
	end
	text = string.gsub(text, "%s*\n%s*", " ")
	local path = (user and mare.paths.config or project_root()) .. "/AGENTS.md"
	local cur = mare.fs.read(path) or ""
	local sep = (cur == "" or string.sub(cur, -1) == "\n") and "" or "\n"
	local okw, err = pcall(mare.fs.write, path, cur .. sep .. "- " .. text .. "\n", { atomic = true })
	if not okw then
		ui.notify("Not saved: " .. tostring(err), "error")
		return
	end
	ui.notify(("Added to %s — applies from the next session (/new)"):format(ui.display_path(path, 60)), "success")
	return path
end

return function(ctx, args)
	local word, rest = string.match(args or "", "^(%S*)%s*(.-)$")
	if word == "add" then
		return add(ctx, rest)
	elseif word ~= "" then
		ui.notify("Usage: /memory, or /memory add [--user] <text>", "warning")
		return
	end
	local okc, C = pcall(ctx.get, ctx, "context")
	if not okc or not C then
		ui.notify("The context plugin is off: no instruction files are loaded", "info")
		return
	end
	local files = C:files() or {}
	local items, nodes = {}, {}
	local have_user, have_root = false, false
	local root = project_root()
	for _, f in ipairs(files) do
		have_user = have_user or f.scope == "user"
		have_root = have_root or (string.match(f.path, PARENT) == root)
		items[#items + 1] = {
			label = ui.display_path(f.path, 60),
			detail = ("%s · %d bytes%s"):format(
				f.scope or "project",
				#(f.content or ""),
				f.omitted and " · over budget" or ""
			),
		}
		nodes[#nodes + 1] = f
	end
	if not have_user then
		items[#items + 1] =
			{ label = ui.display_path(mare.paths.config .. "/AGENTS.md", 60), detail = "user · not created yet" }
		nodes[#nodes + 1] = { path = mare.paths.config .. "/AGENTS.md", missing = true }
	end
	if not have_root then
		items[#items + 1] = { label = ui.display_path(root .. "/AGENTS.md", 60), detail = "project · not created yet" }
		nodes[#nodes + 1] = { path = root .. "/AGENTS.md", missing = true }
	end
	local i = ui.select({ title = "Memory (instruction files) · /memory add <text> to append", items = items })
	local f = i and nodes[i]
	if not f then
		return
	end
	if f.missing then
		ui.notify(
			("%s doesn't exist yet: /memory add%s <text> creates it"):format(
				ui.display_path(f.path, 60),
				f.path == mare.paths.config .. "/AGENTS.md" and " --user" or ""
			)
		)
		return
	end
	ui.overlay(ui.display_path(f.path, 60), ui.Markdown(f.content or ""))
end
