-- statusline.git: branch detection and background git/PR stats refresh;
-- part of the `statusline` plugin (plugins/statusline.lua).
local M = {}

function M.branch()
	return mare.fs.git_branch(mare.cwd)
end

-- Background probe runner for git-derived segments (`changes`, `pr`, and post-turn `branch`
-- refreshes). Initial `branch` is read synchronously in `apply`; subprocess probes run on
-- `ui/ready` and `turn/completed`.
function M.create(ctx, cfg, S)
	local refreshing = false
	local rerun = false
	local git_env_exclude = {
		"GIT_DIR",
		"GIT_WORK_TREE",
		"GIT_INDEX_FILE",
		"GIT_COMMON_DIR",
		"GIT_OBJECT_DIRECTORY",
		"GIT_ALTERNATE_OBJECT_DIRECTORIES",
		"GIT_PREFIX",
	}
	local function run_git_status(timeout_ms)
		return pcall(mare.proc.run, {
			cmd = "git",
			args = { "--no-optional-locks", "status", "--porcelain=v1", "--untracked-files=normal" },
			cwd = mare.cwd,
			env_exclude = git_env_exclude,
			sandbox = "current",
			timeout_ms = timeout_ms,
			max_output = 1024 * 1024,
		})
	end
	return function()
		if not ui.connected() then
			return
		end
		if refreshing then
			rerun = true
			return
		end
		refreshing = true
		ctx:spawn(function()
			local _reset <close> = setmetatable({}, {
				__close = function()
					refreshing = false
					rerun = false
				end,
			})
			local pr_checked_branch = nil
			repeat
				rerun = false
				local ok_b, b = pcall(M.branch)
				if ok_b then
					if b ~= S.branch then
						S.branch = b
						S.pr = nil
						S.pr_branch = nil
						if not b then
							S.changed = nil
						end
						ctx.ui:invalidate("right")
					end
					if S.branch then
						local dirty = false
						if cfg.changed_files then
							local ok, r = run_git_status(cfg.git_timeout_ms)
							if (not ok or not r or r.timed_out) and ui.connected() then
								mare.task.sleep(50)
								ok, r = run_git_status(math.max(cfg.git_timeout_ms or 0, 10000))
							end
							if ok and r and r.code == 0 and not r.timed_out then
								local n = 0
								for _ in string.gmatch(r.stdout or "", "[^\n]+") do
									n = n + 1
								end
								dirty = dirty or n ~= S.changed
								S.changed = n
							end
						end
						-- Query PR when branch changed or no PR is cached yet (so gh failures/timeouts and
						-- newly created PRs on the same branch are discovered on subsequent probes), while
						-- skipping duplicate gh calls within the same trailing-rerun burst on the same branch.
						if
							cfg.pr
							and (S.pr_branch ~= S.branch or S.pr == nil)
							and pr_checked_branch ~= S.branch
							and mare.proc.which
							and mare.proc.which("gh")
						then
							local looked = S.branch
							pr_checked_branch = looked
							local ok, r = pcall(mare.proc.run, {
								cmd = "gh",
								args = { "pr", "view", "--json", "number,url" },
								cwd = mare.cwd,
								sandbox = "current",
								timeout_ms = cfg.pr_timeout_ms,
								max_output = 64 * 1024,
							})
							local found = nil
							if ok and r and r.code == 0 and not r.timed_out then
								local okj, j = pcall(mare.json.decode, r.stdout or "")
								if okj and type(j) == "table" and j.number then
									found = { number = j.number, url = j.url }
								end
							end
							if S.branch == looked then
								if found then
									S.pr, S.pr_branch = found, looked
									dirty = true
								elseif S.pr ~= nil then
									S.pr, S.pr_branch = nil, nil
									dirty = true
								end
							end
						end
						if dirty then
							ctx.ui:invalidate("right")
						end
					end
				end
			until not rerun or not ui.connected()
		end)
	end
end

return M
