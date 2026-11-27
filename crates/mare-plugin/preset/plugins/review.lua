-- review: two things under one name.
--
-- /review: a real code review: a read-only one-shot agent session
-- (mare.subagent.start with the `read-only` permission preset and read-only tools) reviews
-- uncommitted changes (staged, unstaged and untracked files), a commit, the changes against a
-- base branch, or follows custom instructions. Its findings are shown in the transcript; they
-- never enter this conversation's context (Apache-2.0).
--
--   /review pick a target
--   /review <sha> review that commit /review commit [sha]
--   /review base [branch] against a base branch /review uncommitted
--   /review changes [path] the change viewer (below) /review <anything else> = instructions
--
-- Ctrl+R: the change viewer: mare's own edits per turn (the
-- `changes` journal), or git's unstaged / staged / last commit. Word-level diffs come from the
-- core diff renderer; lockfiles and huge diffs start collapsed.

return {
	name = "review",
	provide = { "review" },
	inject = { "ui", "bus", "task" },
	config_schema = {
		collapse_lines = {
			type = "integer",
			default = 400,
			desc = "Diffs longer than this many lines start collapsed.",
		},
		scopes = {
			type = "list",
			items = { type = "string" },
			default = { "turns", "unstaged", "staged", "committed" },
			desc = "Diff scopes offered, in order.",
		},
		-- /review: the diff included in the reviewer's prompt is cut at this many bytes (the
		-- reviewer can still run git itself)
		prompt_diff_bytes = {
			type = "integer",
			default = 120 * 1024,
			desc = "/review: the diff in the reviewer's prompt is cut at this many bytes.",
		},
		-- untracked files: at most this many, each at most `untracked_file_bytes`
		untracked_files = { type = "integer", default = 50, desc = "Most untracked files included." },
		untracked_file_bytes = { type = "integer", default = 256 * 1024, desc = "Most bytes of one untracked file." },
		-- the reviewer's tools (read-only; bash runs under the read-only preset's sandbox)
		tools = {
			type = "list",
			items = { type = "string" },
			default = { "read", "ls", "bash" },
			desc = "The reviewer's tools (read-only).",
		},
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call; commands is a follower below
		local follow = require("mare_lib").follow
		local svc = { changes = follow(ctx, "changes"), llm = follow(ctx, "llm") }
		local viewer_fn, runner_mod, run_review_fn
		local function get_viewer()
			if not viewer_fn then
				viewer_fn = require("review.viewer").create(ctx, cfg, svc)
			end
			return viewer_fn
		end
		local function get_runner()
			if not run_review_fn then
				local mod = require("review.runner")
				local fn = mod.create(ctx, cfg, svc)
				runner_mod = mod
				run_review_fn = fn
			end
			return runner_mod, run_review_fn
		end

		local review = {}
		--- run(target) -> findings | nil, err. target: {kind = "uncommitted"} | {kind = "commit",
		--- sha} | {kind = "base", branch} | {kind = "custom", text}
		function review.run(_, _, target)
			local _, run_review = get_runner()
			return run_review(target)
		end
		function review.viewer(_, _, only)
			return get_viewer()(only)
		end
		-- a mare.Service
		require("mare_lib").service(ctx, "review", review)

		ctx.ui:action("review.viewer", function()
			ctx:spawn(function()
				get_viewer()()
			end)
		end, {
			desc = "review changes: mare's edits per turn, or git unstaged / staged / last commit",
			contexts = { "editor" },
			keys = { editor = { key = "<C-r>", priority = 10 } },
		})
		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("review", {
				description = "Review code in a read-only session (/review [sha|base <branch>|<instructions>])",
				args = "[sha|commit|base|changes|instructions]",
				auto_execute = true,
				availability = "idle",
				run = function(args)
					ctx:spawn(function()
						local runner, run_review = get_runner()
						local target = runner.parse(args) or runner.pick_target(ctx)
						if not target then
							return
						end
						if target.kind == "viewer" then
							get_viewer()(target.only)
							return
						end
						local _, err = run_review(target)
						if err then
							ui.notify("Review: " .. tostring(err), "warning")
						end
					end)
				end,
			})
		end)
	end,
}
