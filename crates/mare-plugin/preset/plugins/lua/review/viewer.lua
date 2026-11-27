-- review.viewer: Ctrl+R change viewer (per-turn journal and git scopes);
-- part of the `review` plugin (plugins/review.lua).
local git = require("review.git")

local M = {}

local LOCKFILES = {
	["Cargo.lock"] = true,
	["package-lock.json"] = true,
	["yarn.lock"] = true,
	["pnpm-lock.yaml"] = true,
	["bun.lockb"] = true,
	["go.sum"] = true,
	["uv.lock"] = true,
	["poetry.lock"] = true,
	["Pipfile.lock"] = true,
	["flake.lock"] = true,
	["Gemfile.lock"] = true,
	["composer.lock"] = true,
}

local DETAIL = { unstaged = "git diff", staged = "git diff --cached", committed = "last commit (HEAD)" }

local function collapsed(cfg, f, only)
	if only then
		return false
	end
	local base = string.match(f.path, "([^/]+)$") or f.path
	return LOCKFILES[base] or (f.adds + f.dels) > cfg.collapse_lines
end

local function file_node(cfg, f, only)
	local head = ui.Line({
		ui.Span(f.path):hl("MareHeading"),
		ui.Span(("  +%d"):format(f.adds)):hl("MareDiffAddText"),
		ui.Span((" −%d"):format(f.dels)):hl("MareDiffDeleteText"),
	})
	if f.missing then
		return ui.Lines({
			head,
			ui.Line({ ui.Span("  (no backup: too large, a link, or trimmed)"):hl("MareDim") }),
		})
	end
	if collapsed(cfg, f, only) then
		return ui.Lines({
			head,
			ui.Line({ ui.Span("  (collapsed — /review changes " .. f.path .. " to show)"):hl("MareDim") }),
		})
	end
	local body = f.unified and ui.Diff({ unified = f.text })
		or ui.Diff({ old = f.old or "", new = f.new or "", path = f.path })
	return ui.Lines({ head, body, ui.Gap and ui.Gap(1) or ui.Line({}) })
end

local function journal_turns(svc)
	local ch = svc.changes()
	if not ch then
		return nil, "the changes plugin is off"
	end
	local list = ch:diffs()
	local relpath = mare.fs.relative
	for _, t in ipairs(list) do
		for _, f in ipairs(t.files) do
			f.path = relpath(f.path)
			f.adds, f.dels = f.adds or 0, f.dels or 0
		end
	end
	return list
end

local function gather(scope)
	local args = ({
		unstaged = { "diff", "--no-color" },
		staged = { "diff", "--no-color", "--cached" },
		committed = { "show", "--no-color", "--format=", "HEAD" },
	})[scope]
	local d, err = git.run(args)
	if not d then
		return nil, err
	end
	local files = git.split_files(d)
	for _, f in ipairs(files) do
		f.unified = true
	end
	return files
end

local function show_files(cfg, title_scope, files, only, notes)
	if only then
		local keep = {}
		for _, f in ipairs(files) do
			if f.path == only or string.find(f.path, only, 1, true) then
				keep[#keep + 1] = f
			end
		end
		files = keep
	end
	if #files == 0 then
		ui.notify(("No changes (%s)"):format(title_scope))
		return
	end
	local nodes, ta, td = {}, 0, 0
	for _, n in ipairs(notes or {}) do
		nodes[#nodes + 1] = n
	end
	for _, f in ipairs(files) do
		ta, td = ta + f.adds, td + f.dels
		nodes[#nodes + 1] = file_node(cfg, f, only)
	end
	local title = ("Changes · %s · %d file%s +%d −%d"):format(
		title_scope,
		#files,
		#files == 1 and "" or "s",
		ta,
		td
	)
	mare.ui.overlay(title, ui.Lines(nodes))
end

local function show_turn(cfg, t, only)
	local notes = {}
	if t.shell then
		notes[1] = ui.Line({
			ui.Span("Shell commands ran in this turn: other changes may not be recorded"):hl("MareWarning"),
		})
	end
	show_files(cfg, "turn “" .. t.label .. "”", t.files, only, notes)
end

local function show_all_turns(cfg, list, only)
	local files, notes = {}, {}
	for i = #list, 1, -1 do
		for _, f in ipairs(list[i].files) do
			local f2 = table.copy(f)
			f2.path = f.path .. "  (" .. list[i].label .. ")"
			files[#files + 1] = f2
		end
	end
	show_files(cfg, "this session, per turn", files, only, notes)
end

function M.create(ctx, cfg, svc)
	local last = nil
	return function(only)
		local items, acts = {}, {}
		local list = {}
		if table.find(cfg.scopes, "turns") or table.find(cfg.scopes, "session") then
			list = journal_turns(svc) or {}
			if #list > 0 then
				items[#items + 1] = { label = "mare's edits, per turn", header = true }
				for i = #list, 1, -1 do
					local t = list[i]
					items[#items + 1] = {
						label = "Turn: " .. t.label,
						detail = ("%d file%s%s"):format(
							#t.files,
							#t.files == 1 and "" or "s",
							t.shell and " · ran shell commands" or ""
						),
					}
					acts[#items] = { turn = t, key = "turn:" .. t.turn }
				end
				if #list > 1 then
					items[#items + 1] = { label = "All turns", detail = #list .. " turns, newest first" }
					acts[#items] = { all = true, key = "all" }
				end
			end
		end
		local git_header = false
		for _, s in ipairs(cfg.scopes) do
			if DETAIL[s] then
				if not git_header then
					items[#items + 1] = { label = "git", header = true }
					git_header = true
				end
				items[#items + 1] = { label = s, detail = DETAIL[s] }
				acts[#items] = { scope = s, key = s }
			end
		end
		local sel
		for i, a in pairs(acts) do
			if a.key == last then
				sel = i
			end
		end
		if not sel then
			for i = 1, #items do
				if acts[i] then
					sel = i
					break
				end
			end
		end
		local i = ui.select({ title = "Review changes", items = items, selected = sel })
		local a = i and acts[i]
		if not a then
			return
		end
		last = a.key
		if a.turn then
			show_turn(cfg, a.turn, only)
		elseif a.all then
			show_all_turns(cfg, list, only)
		else
			local files, err = gather(a.scope)
			if not files then
				ui.notify("review: " .. tostring(err), "warning")
				return
			end
			show_files(cfg, a.scope, files, only)
		end
	end
end

return M
