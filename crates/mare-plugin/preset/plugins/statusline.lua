-- statusline: mode capsule · workflow · jobs | model · effort · context gauge · cwd · git branch ·
-- pull request · changed files · cost
--
-- Two-stage startup & background probes:
--   `apply` is synchronous and only registers listeners, commands and status slots without loading
--   cold submodules (`statusline.*`) or spawning subprocesses. `branch` is read synchronously from
--   `.git/HEAD` during `apply` (a single small file read via `mare.fs.git_branch`), so all
--   synchronous segments (mode, hints, flags, model, effort, context, cwd, branch, cost, quota,
--   peer_allow, tips, jobs, workflow, mcp, flash) render identically on the first frame and once
--   ready.
--   Background-probed segments — `changes`, `pr` (and post-turn `branch` refreshes via
--   `statusline.git`) and external `command` output (via `statusline.external`) — start on
--   `ui/ready` (and refresh after turns) and update their content in place when results arrive;
--   startup never spawns subprocesses.
--
-- External command: mare.plugin("statusline", { command = "~/bin/sl" })
-- runs the command with a JSON object on stdin — session_id, transcript_path, cwd, width, model
-- {id, name, ref}, context {used, window, percent}, cost — and shows its first output line.
-- It reruns after each turn / model change, with a timeout; failures keep the last output.
return {
	name = "statusline",
	-- in the minimal plugin set
	tags = { "minimal" },
	-- `task` gives ctx:spawn
	inject = { "ui", "llm", "task" },
	config_schema = {
		command = {
			type = { "string", "table" },
			desc = "Command (string, run via the shell, or argv table) whose output joins the bar.",
		},
		command_timeout_ms = { type = "integer", default = 1000, desc = "Timeout of `command`." },
		command_replaces = {
			type = "boolean",
			default = false,
			desc = "Replace the built-in segments instead of adding the command's output to them.",
		},
		gauge_width = { type = "integer", default = 6, desc = "Cells of the context usage bar." },
		cwd_width = {
			type = "integer",
			default = 24,
			desc = "Cells of the working directory (longer paths keep their last components).",
		},
		cwd_min_terminal_width = {
			type = "integer",
			default = 100,
			desc = "The directory segment shows from this terminal width on.",
		},
		changed_files = {
			type = "boolean",
			default = true,
			desc = "Show `± N`: files `git status` reports changed (refreshed once per turn).",
		},
		pr = {
			type = "boolean",
			default = true,
			desc = "Show the current branch's pull request via `gh pr view` (when gh is installed).",
		},
		git_timeout_ms = { type = "integer", default = 10000, desc = "Timeout of git queries." },
		pr_timeout_ms = { type = "integer", default = 5000, desc = "Timeout of the pull request query." },
		-- segments not shown (`/statusline` toggles them and saves the choice in state.json):
		-- mode, model, effort, context, cwd, branch, pr, changes, cost, quota, tips
		hidden = { type = "list", items = { type = "string" }, default = {}, desc = "Segments not shown." },
		-- the bar's layout (statusline.looks; `"combined"` is the default):
		-- nil/"combined" | "classic" | "codex" | "kimi" | a name in `looks` | a look table.
		-- `layout_opts` overrides single elements of it (`{ sep = "space", flash = 0 }`) and
		-- `looks` adds or replaces named looks (segments, order and drop priority are data).
		layout = {
			type = { "string", "table" },
			desc = "The bar's layout: a built-in look, a name in `looks`, or a look table.",
		},
		layout_opts = { type = "map", desc = "Overrides single elements of the layout." },
		looks = { type = "map", desc = "Adds or replaces named looks." },
		layout_margin = {
			type = "integer",
			default = 2,
			desc = "Cells kept free at the right edge when fitting a layout.",
		},
	},
	apply = function(ctx, cfg)
		local hidden = {}
		for _, n in ipairs(type(cfg.hidden) == "table" and cfg.hidden or {}) do
			hidden[n] = true
		end
		local ok_st, st = pcall(mare.state.read)
		local saved = ok_st and type(st) == "table" and st.statusline_hidden
		if type(saved) == "table" then
			hidden = {}
			for _, n in ipairs(saved) do
				hidden[n] = true
			end
		end
		local function shown(n)
			return not hidden[n]
		end

		local ok_br, initial_branch = pcall(mare.fs.git_branch)
		local S = {
			shown = shown,
			-- permissions and session are optional: their state is read when each comes up
			-- (ctx:inject below), since on the cordis tree they may start after this plugin
			mode = "full-access",
			partial = false,
			workflow = false,
			ephemeral = true,
			readonly_view = false,
			jobs_running = {},
			limits_by_provider = {},
			branch = ok_br and initial_branch or nil,
			changed = nil,
			pr = nil,
			pr_branch = nil,
			external_text = nil,
		}
		S.external_width = function()
			return S.external_text and S.external_text ~= "" and (mare.text.width(S.external_text) + 3) or 0
		end

		local pair_box = { active = false, right = nil }
		local function redraw_both()
			pair_box.active = true
			pair_box.right = nil
			local ok, err = pcall(function()
				ctx.ui:invalidate("left")
				ctx.ui:invalidate("right")
			end)
			pair_box.active = false
			pair_box.right = nil
			if not ok then
				error(err, 0)
			end
		end

		-- `/statusline` picker loads `statusline.visibility` only when invoked.
		ctx:inject({ "commands" }, function(sub)
			sub.commands:register("statusline", {
				description = "Choose the status bar's segments (saved)",
				run = function()
					require("statusline.visibility").open_picker(sub, hidden, shown)
				end,
			})
		end)

		ctx:inject({ "permissions" }, function(c)
			local permissions = c.permissions
			S.mode = permissions:preset() or "full-access"
			if permissions.is_partial then
				local ok, current = pcall(function()
					return permissions:is_partial()
				end)
				S.partial = ok and current == true
			end
			ctx.ui:invalidate("left")
		end)
		ctx:inject({ "session" }, function()
			S.ephemeral = (mare.args and mare.args.no_session) and true or false
			ctx.ui:invalidate("left")
		end)

		-- Activity & transient flash state (lightweight in-memory trackers; no submodule requires).
		local act = { running = false, queued = 0, agents = 0, editor_empty = true }
		local children = {}
		local flash, flash_gen = nil, 0
		local look_cache
		local function get_look()
			if not look_cache then
				look_cache = require("statusline.looks").resolve(cfg)
			end
			return look_cache
		end
		local function show_flash(text)
			local L = get_look()
			if (L.flash or 0) <= 0 then
				return
			end
			flash_gen = flash_gen + 1
			local gen = flash_gen
			flash = text
			redraw_both()
			ctx:spawn(function()
				mare.task.sleep(L.flash * 1000)
				if gen == flash_gen then
					flash = nil
					redraw_both()
				end
			end)
		end
		local function now_model()
			local m = ctx.llm:current()
			return m and (m.name or m.id) or nil,
				m and m.reasoning and ctx.llm:effort() or nil,
				m and m.ref or nil,
				m and m.context_window or nil
		end
		local last_model, last_effort, last_ref, last_window = now_model()

		-- Lazy background probe runners (`statusline.git` and `statusline.external`).
		-- Both are gated on `ui/ready` (stored in `mare.g` across `/reload`) so early startup
		-- events like `llm/providers-changed` never spawn subprocesses before the first frame.
		local UI_READY_KEY = "statusline.ui_ready"
		local git_runner
		local function run_git_probe()
			if not mare.ui.connected() or not mare.g[UI_READY_KEY] then
				return
			end
			if not git_runner then
				git_runner = require("statusline.git").create(ctx, cfg, S)
			end
			git_runner()
		end
		local external_runner
		local function run_external()
			if not cfg.command or not mare.g[UI_READY_KEY] then
				return
			end
			if not external_runner then
				external_runner = require("statusline.external").create(ctx, cfg, S)
			end
			external_runner()
		end

		local label_cache_box = { map = {} }
		local function clear_label_cache()
			label_cache_box.map = {}
		end
		ctx:on("ui/keymap-changed", clear_label_cache)

		ctx:on("session/changed", function()
			local session_service = ctx:get("session")
			local info = session_service and session_service:info() or {}
			S.readonly_view = info.readonly == true
			if info.ephemeral ~= nil then
				S.ephemeral = info.ephemeral == true
			end
			redraw_both()
		end)
		for _, ev in ipairs({ "mcp/changed", "mcp/error", "tools/loaded" }) do
			ctx:on(ev, redraw_both)
		end
		local function run_background_probes()
			run_git_probe()
			run_external()
		end
		ctx:on("reload/completed", function()
			clear_label_cache()
			redraw_both()
			if mare.ui.connected() then
				mare.g[UI_READY_KEY] = true
				run_background_probes()
			end
		end)
		ctx:on("mcp/ready", function(ev)
			redraw_both()
			if not (cfg.command and cfg.command_replaces) then
				local raw = ev --[[@as table<string, any>]]
				local name = type(raw) == "table" and (raw.name or raw.server) or nil
				show_flash("MCP " .. (name and (name .. " ") or "") .. "ready")
			end
		end)
		ctx:on("permission/changed", function(ev)
			local raw = ev --[[@as table<string, any>]]
			S.mode = type(raw) == "table" and (raw.preset or raw.mode) or raw
			S.partial = type(raw) == "table" and raw.partial == true
			local permissions = ctx:get("permissions")
			if permissions and permissions.is_partial then
				local ok, current = pcall(function()
					return permissions:is_partial()
				end)
				if ok then
					S.partial = current == true
				end
			end
			redraw_both()
		end)
		ctx:on("app/ready", function()
			if not ctx:get("permissions") then
				require("statusline.bars").warn_no_permissions()
			end
		end)
		ctx:on("ui/ready", function()
			mare.g[UI_READY_KEY] = true
			run_background_probes()
		end)
		if mare.g[UI_READY_KEY] then
			run_background_probes()
		end
		ctx:on("workflow/changed", function(ev)
			S.workflow = type(ev) == "table" and ev.enabled == true
			redraw_both()
		end)
		ctx:on("jobs/changed", function(ev)
			local j = ev and ev.job
			if j and j.job_id then
				S.jobs_running[j.job_id] = j.status == "running" and true or nil
				redraw_both()
			end
		end)
		ctx:on("option/changed", function(ev)
			if type(ev) == "table" and ev.name == "peer" then
				redraw_both()
			end
		end)
		ctx:on("llm/rate-limits", function(ev)
			if type(ev) == "table" and type(ev.provider) == "string" and ev.provider ~= "" then
				S.limits_by_provider[ev.provider] = ev.limits
				ctx.ui:invalidate("right")
			end
		end)
		ctx:on("llm/model-changed", function()
			redraw_both()
			run_external()
			local m, e, ref, win = now_model()
			if not (cfg.command and cfg.command_replaces) then
				if m ~= last_model and m then
					show_flash("model → " .. m)
				elseif e ~= last_effort and e then
					show_flash("effort → " .. e)
				end
			end
			last_model, last_effort, last_ref, last_window = m, e, ref, win
		end)
		ctx:on("llm/providers-changed", function()
			local m, e, ref, win = now_model()
			if m ~= last_model or e ~= last_effort or ref ~= last_ref or win ~= last_window then
				last_model, last_effort, last_ref, last_window = m, e, ref, win
				redraw_both()
			end
			run_external()
		end)
		ctx:on("usage/changed", function()
			redraw_both()
			run_external()
		end)
		ctx:on("turn/started", function()
			act.running = true
			redraw_both()
		end)
		ctx:on("turn/completed", function()
			act.running = false
			redraw_both()
			run_external()
			run_git_probe()
		end)
		ctx:on("editor/changed", function(ev)
			local empty = type(ev) ~= "table" or (ev.text or "") == ""
			if act.editor_empty ~= empty then
				act.editor_empty = empty
				redraw_both()
			end
		end)
		ctx:on("queue/changed", function(ev)
			act.queued = type(ev) == "table" and #(ev.items or {}) or 0
			redraw_both()
		end)
		for ev, d in pairs({ ["subagent/started"] = true, ["subagent/completed"] = false }) do
			ctx:on(ev, function(e)
				if type(e) == "table" and e.subagent_id then
					children[e.subagent_id] = d or nil
					local n = 0
					for _ in pairs(children) do
						n = n + 1
					end
					act.agents = n
					redraw_both()
				end
			end)
		end
		if cfg.command then
			ctx:on("status/changed", run_external)
			ctx.ui:status("external", {
				align = "left",
				order = 100,
				render = function()
					if not S.external_text or S.external_text == "" then
						return nil
					end
					return ui.Line({ ui.Span(S.external_text):hl("MareStatus") })
				end,
			})
			if cfg.command_replaces then
				for _, id in ipairs({ "left", "right" }) do
					ctx.ui:status(id, { render = function() end })
				end
			end
		end

		if not (cfg.command and cfg.command_replaces) then
			local renderer
			local flush_pair = { gen = nil, right = nil }
			local function render_side(which, flush_gen)
				if not mare.ui.connected() then
					return nil
				end
				if not renderer then
					local helpers = require("statusline.bars").helpers(ctx, S)
					local P = {
						shown = S.shown,
						external_width = S.external_width,
						fmt_k = helpers.fmt_k,
						MODE_TEXT = helpers.MODE_TEXT,
						MODE_STYLE = helpers.MODE_STYLE,
						cwd_width = cfg.cwd_width,
						cwd_min_terminal_width = cfg.cwd_min_terminal_width,
						gauge_width = cfg.gauge_width,
						mode = function()
							return ctx:get("permissions") and S.mode or "full-access", S.partial
						end,
						ephemeral = function()
							return S.ephemeral
						end,
						readonly = function()
							return S.readonly_view
						end,
						workflow = function()
							return S.workflow
						end,
						peer_allow = helpers.peer_allow,
						mcp_connecting = helpers.mcp_connecting,
						jobs = helpers.count_jobs,
						branch = function()
							return S.branch
						end,
						changed = function()
							return S.changed
						end,
						pr = function()
							return S.pr_branch == S.branch and S.pr or nil
						end,
						quota = helpers.quota_text,
						activity = function()
							return act
						end,
						flash = function()
							return flash
						end,
					}
					renderer = require("statusline.layout").create_renderer(ctx, cfg, P, get_look(), label_cache_box)
				end
				if flush_gen ~= nil and not pair_box.active then
					if which == "left" then
						local pair = { active = true, right = nil }
						local line = renderer("left", pair)
						flush_pair.gen = flush_gen
						flush_pair.right = pair.right
						return line
					elseif which == "right" and flush_pair.gen == flush_gen and flush_pair.right ~= nil then
						local pair = { active = true, right = flush_pair.right }
						flush_pair.right = nil
						return renderer("right", pair)
					end
				end
				return renderer(which, pair_box)
			end
			pair_box.active = true
			pair_box.right = nil
			local ok_reg, err_reg = pcall(function()
				ctx.ui:status("left", {
					align = "left",
					order = 0,
					keep = true,
					render = function(_, flush_gen)
						return render_side("left", flush_gen)
					end,
				})
				ctx.ui:status("right", {
					align = "right",
					order = 0,
					render = function(_, flush_gen)
						return render_side("right", flush_gen)
					end,
				})
			end)
			pair_box.active = false
			pair_box.right = nil
			if not ok_reg then
				error(err_reg, 0)
			end
		end
	end,
}
