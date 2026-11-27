-- review.runner: /review target parsing, prompt resolution, and read-only reviewer session;
-- part of the `review` plugin (plugins/review.lua).
local git = require("review.git")

local M = {}

local function key_for(ctx, action)
	local key = (mare.keymap.hint(action, "editor") or {})[1]
	if not key then
		return nil
	end
	for _, binding in ipairs(ctx.ui:get_keymap("editor").bindings or {}) do
		if binding.key == key then
			return binding.key_label or key
		end
	end
	return key
end

-- Condensed review rubric (Markdown output)
local RUBRIC = [[# Review guidelines

You are acting as a reviewer for a proposed code change made by another engineer. You are in a
read-only session: inspect files and run read-only commands (git diff, git show, grep, tests that
don't write) but never modify anything.

Flag an issue only when:
1. It meaningfully impacts the accuracy, performance, security, or maintainability of the code.
2. It is discrete and actionable (not a general issue with the codebase).
3. Fixing it doesn't demand more rigor than the rest of the codebase has.
4. It was introduced by the change (pre-existing bugs are not flagged).
5. The author would likely fix it if made aware of it.
6. It doesn't rely on unstated assumptions about the codebase or the author's intent.
7. You can point at the parts of the code that are provably affected (no speculation).
8. It is clearly not an intentional change by the author.

For each finding write a short comment: why it is a bug, how severe it is and in which scenarios
or inputs it shows; one paragraph, matter-of-fact, no code chunks longer than 3 lines, no
flattery. Ignore trivial style unless it obscures meaning or violates documented standards.
Project instruction files (AGENTS.md and the like) that apply to the changed files take
precedence over these general guidelines; cite the rule when a finding relies on one.

Output all findings the author would fix; if there are none, say so — don't invent findings.

## Output format (Markdown)

For each finding, most severe first:

### [P0|P1|P2|P3] <imperative title, at most 80 characters>
`<file>:<line or range>` — <one-paragraph explanation>

P0: drop everything (universal, blocking). P1: urgent. P2: normal. P3: nice to have.

End with one line: **Overall: correct** or **Overall: incorrect** (whether existing code and tests
keep working and the change is free of blocking bugs; ignore non-blocking nits).
]]

-- Review target prompts
local UNCOMMITTED_PROMPT =
	"Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings."
local BASE_PROMPT = "Review the code changes against the base branch '%s'. The merge base commit for this "
	.. "comparison is %s. Run `git diff %s` to inspect the changes relative to %s. Provide prioritized, "
	.. "actionable findings."
local COMMIT_PROMPT = "Review the code changes introduced by commit %s%s. Provide prioritized, actionable findings."

local function cap(cfg, text)
	if #text <= cfg.prompt_diff_bytes then
		return text, false
	end
	return mare.text.cut(text, cfg.prompt_diff_bytes), true
end

--- staged + unstaged (against HEAD) plus every untracked file as an addition
local function uncommitted_diff(cfg)
	local d = git.run({ "diff", "--no-color", "HEAD" })
	if not d then
		-- no commit yet: the index and the work tree
		d = (git.run({ "diff", "--no-color", "--cached" }) or "") .. (git.run({ "diff", "--no-color" }) or "")
	end
	local list = git.run({ "ls-files", "--others", "--exclude-standard", "-z" }) or ""
	local parts, n, skipped = { d }, 0, {}
	for p in string.gmatch(list, "([^\0]+)") do
		n = n + 1
		local st = mare.fs.stat(p)
		if n > cfg.untracked_files or (st and st.size and st.size > cfg.untracked_file_bytes) then
			skipped[#skipped + 1] = p
		else
			-- exit code 1 = "there are differences" for --no-index
			local u = git.run({ "diff", "--no-color", "--no-index", "--", "/dev/null", p }, { [1] = true })
			if u and u ~= "" then
				parts[#parts + 1] = u
			end
		end
	end
	local text = table.concat(parts, "")
	if #skipped > 0 then
		text = text
			.. ("\n# untracked files not included (too many or too large): %s\n"):format(table.concat(skipped, ", "))
	end
	return text
end

--- target → {prompt, hint} | nil, err
local function resolve(cfg, target)
	if target.kind == "uncommitted" then
		local d = uncommitted_diff(cfg)
		if not d:find("%S") then
			return nil, "no uncommitted changes to review"
		end
		local body, cut = cap(cfg, d)
		return {
			hint = "uncommitted changes",
			prompt = UNCOMMITTED_PROMPT
				.. "\n\nThe diff (staged, unstaged and untracked files)"
				.. (cut and " — cut at the size limit; run git yourself for the rest" or "")
				.. ":\n\n```diff\n"
				.. body
				.. "\n```",
		}
	elseif target.kind == "commit" then
		local sha = git.run({ "rev-parse", "--verify", "--quiet", target.sha .. "^{commit}" })
		sha = sha and sha:match("%x+")
		if not sha then
			return nil, "no such commit: " .. target.sha
		end
		local title = (git.run({ "log", "-1", "--format=%s", sha }) or ""):gsub("%s+$", "")
		local d = git.run({ "show", "--no-color", "--format=", sha }) or ""
		local body, cut = cap(cfg, d)
		return {
			hint = "commit " .. git.short(sha) .. (title ~= "" and (": " .. title) or ""),
			prompt = COMMIT_PROMPT:format(sha, title ~= "" and (' ("' .. title .. '")') or "")
				.. "\n\nThe commit's diff"
				.. (cut and " (cut at the size limit; use `git show` for the rest)" or "")
				.. ":\n\n```diff\n"
				.. body
				.. "\n```",
		}
	elseif target.kind == "base" then
		local mb = git.run({ "merge-base", "HEAD", target.branch })
		mb = mb and mb:match("%x+")
		if not mb then
			return nil, "no merge base with " .. target.branch
		end
		local d = git.run({ "diff", "--no-color", mb }) or ""
		if not d:find("%S") then
			return nil, "no changes against " .. target.branch
		end
		local body, cut = cap(cfg, d)
		return {
			hint = "changes against '" .. target.branch .. "'",
			prompt = BASE_PROMPT:format(target.branch, mb, mb, target.branch)
				.. "\n\nThe diff"
				.. (cut and " (cut at the size limit)" or "")
				.. ":\n\n```diff\n"
				.. body
				.. "\n```",
		}
	elseif target.kind == "custom" then
		local text = string.match(target.text or "", "^%s*(.-)%s*$")
		if text == "" then
			return nil, "review instructions can't be empty"
		end
		return { hint = mare.text.trunc_width(text, 60), prompt = text }
	end
	return nil, "unknown review target"
end

--- `/review` with no arguments: the target picker
function M.pick_target(ctx)
	local viewer_key = key_for(ctx, "review.viewer")
	local items = {
		{ label = "Uncommitted changes", detail = "staged, unstaged and untracked files" },
		{ label = "A commit", detail = "pick from the recent commits" },
		{ label = "Against a base branch", detail = "what merging this branch would bring" },
		{ label = "Custom instructions", detail = "describe what to review" },
		{
			label = "Browse changes",
			detail = "the change viewer" .. (viewer_key and (" (" .. viewer_key .. ")") or ""),
		},
	}
	local i = ui.select({ title = "Review", items = items })
	if i == 1 then
		return { kind = "uncommitted" }
	elseif i == 2 then
		local log = git.run({ "log", "-30", "--format=%H%x09%s" }) or ""
		local shas, its = {}, {}
		for sha, subj in string.gmatch(log, "(%x+)\t([^\n]*)") do
			shas[#shas + 1] = sha
			its[#its + 1] = { label = git.short(sha), detail = subj }
		end
		if #its == 0 then
			ui.notify("No commits", "info")
			return nil
		end
		local j = ui.select({ title = "Review a commit", items = its })
		return j and { kind = "commit", sha = shas[j] } or nil
	elseif i == 3 then
		local bs = git.branches()
		if #bs == 0 then
			ui.notify("No branches", "info")
			return nil
		end
		local j = ui.select({ title = "Base branch", items = bs })
		return j and { kind = "base", branch = bs[j] } or nil
	elseif i == 4 then
		local text = ui.input({ title = "Review instructions", placeholder = "e.g. check error handling in src/net" })
		return text and text ~= "" and { kind = "custom", text = text } or nil
	elseif i == 5 then
		return { kind = "viewer" }
	end
	return nil
end

--- `/review` arguments → target
function M.parse(args)
	if args == "" then
		return nil
	end
	local word, rest = string.match(args, "^(%S+)%s*(.-)$")
	if word == "uncommitted" and rest == "" then
		return { kind = "uncommitted" }
	elseif word == "commit" then
		return { kind = "commit", sha = rest ~= "" and rest or "HEAD" }
	elseif word == "base" then
		local b = rest
		if b == "" then
			b = (git.run({ "rev-parse", "--abbrev-ref", "origin/HEAD" }) or ""):match("^origin/(%S+)") or "main"
		end
		return { kind = "base", branch = b }
	elseif word == "changes" then
		return { kind = "viewer", only = rest ~= "" and rest or nil }
	elseif rest == "" and string.match(word, "^%x+$") and #word >= 7 and #word <= 40 then
		return { kind = "commit", sha = word }
	end
	return { kind = "custom", text = args }
end

function M.create(ctx, cfg, svc)
	local running = nil -- the reviewer's handle while it runs (one at a time)

	-- Esc / Ctrl+C stop a running review: a step of ui's ordered interrupt chain, ahead of ui's
	-- own handling
	ctx.ui:handler("interrupt", "review", { order = 10 }, function()
		if running then
			pcall(running.cancel, running)
			return true
		end
		return false
	end)

	--- run the reviewer → its findings (Markdown) | nil, err. Shown in the transcript; emits
	--- review/done {target, hint, text, error}
	return function(target)
		if running then
			return nil, "a review is already running"
		end
		local req, err = resolve(cfg, target)
		if not req then
			return nil, err
		end
		local llm = svc.llm()
		local m = llm and llm:current()
		if not m then
			return nil, "no model — /login, then /model"
		end
		local handle
		local ok, serr = pcall(function()
			handle = mare.subagent.start({
				prompt = req.prompt,
				system = RUBRIC,
				model = m.ref,
				effort = llm:effort(),
				tools = cfg.tools,
				label = "review",
				permission = "read-only",
				permission_ceiling = "read-only",
				-- read-only preset, with the permissions plugin on even when it's off here
				config = { plugins = { permissions = { enabled = true, preset = "read-only" } } },
			})
		end)
		if not ok then
			return nil, "couldn't start the reviewer: " .. tostring(serr)
		end
		running = handle
		if ui.connected() then
			-- The working renderer adds the current interrupt binding on every frame.
			mare.ui.working("Reviewing " .. req.hint, "read-only session")
		end
		local res = handle:wait()
		running = nil
		if ui.connected() then
			mare.ui.working(nil)
		end
		local text = res and res.text or ""
		local rerr = res and require("mare_lib").error_message(res.error) or (res == nil and "no result" or nil)
		ctx.bus:emit("review/completed", { target = target, hint = req.hint, text = text, error = rerr })
		if rerr and not text:find("%S") then
			return nil, rerr
		end
		if ui.connected() then
			local id = mare.ui.push("info", {
				node = ui.Lines({
					ui.Line({
						ui.Span("◆ "):hl("MareMark"),
						ui.Span("Review · " .. req.hint):hl("MareHeading"),
						ui.Span("  (read-only session; not added to the conversation)"):hl("MareDim"),
					}),
					ui.Markdown(text ~= "" and text or "_The reviewer returned nothing._"),
				}),
			})
			mare.ui.finish(id)
		else
			print(text)
		end
		return text
	end
end

return M
