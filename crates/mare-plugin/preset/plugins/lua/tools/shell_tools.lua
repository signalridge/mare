-- tools.shell_tools: the model's default shell (tools.shell), PowerShell discovery and the
-- powershell / bash tools; part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local MAX_LINES = cfg.max_lines
	local tools = D.registry.tools
	local function shell_config(...)
		return D.shell_env.shell_config(...)
	end
	local function bash_direct(...)
		return D.bash_run.bash_direct(...)
	end
	local function bash_job(...)
		return D.bash_run.bash_job(...)
	end
	local function bash_with_cwd(...)
		return D.bash_run.bash_with_cwd(...)
	end
	local function with_workdir(...)
		return D.bash_run.with_workdir(...)
	end
	local function resolve_workdir(...)
		return D.bash_run.resolve_workdir(...)
	end
	local function visible_control(s)
		return (s:gsub("[%z\1-\8\10-\31\127]", function(ch)
			return ("\\x%02x"):format(string.byte(ch))
		end))
	end
	local function shell_preview(a, call_cwd)
		local cmd = type(a) == "table" and type(a.command) == "string" and a.command or ""
		local cwd = (type(call_cwd) == "string" and call_cwd ~= "") and call_cwd or resolve_workdir(a)
		local items = {}
		if type(cwd) == "string" and cwd ~= "" then
			items[#items + 1] = ui.Paragraph(
				ui.Line({
					ui.Span("cwd: "):hl("MareMuted"),
					ui.Span(visible_control(cwd)):hl("MareTitle"),
				}),
				{ hard_wrap = true }
			)
		end
		for _, line in ipairs(mare.text.split_lines(cmd)) do
			items[#items + 1] =
				ui.Paragraph(ui.Line({ ui.Span(visible_control(line)):hl("MareText") }), { hard_wrap = true })
		end
		if #items == 0 then
			return nil
		end
		local node = ui.Lines(items)
		if type(cwd) == "string" and cwd ~= "" then
			node.includes_cwd = true
		end
		return node
	end
	local function shell_parameters(desc_hint, cmd_desc, has_jobs)
		local props = {
			description = { type = "string", description = desc_hint },
			command = { type = "string", description = cmd_desc },
			workdir = {
				type = "string",
				description = "Run in this directory instead of the current one (does not change it)",
			},
			timeout_ms = {
				type = "integer",
				description = ("Timeout in milliseconds (default %d, max %d)"):format(
					cfg.bash_timeout_ms,
					cfg.bash_max_timeout_ms
				),
			},
		}
		if has_jobs then
			props.run_in_background = {
				type = "boolean",
				description = "Start as a background job and return its id immediately",
			}
			props.tty = {
				type = "boolean",
				description = "Allocate a pseudo-terminal (PTY) for this command instead of plain pipes (default false; implies writable stdin)",
			}
			props.stdin = {
				type = "boolean",
				description = "Keep stdin open for job_write instead of connecting it to /dev/null (default false; always true when tty = true)",
			}
		end
		return {
			type = "object",
			properties = props,
			required = { "command" },
		}
	end

	-- the powershell tool's configuration (Windows; resolved below, where the tool registers)
	local ps_cfg, ps_err

	--- shell() -> {path, args, transport, kind} | nil, err — the shell model commands default
	--- to: PowerShell when it replaces bash, or inside the Windows default-tier sandbox where
	--- Git Bash can't start;
	--- otherwise the bash tool's shell
	function tools.shell(_, _)
		if mare.os == "windows" and ps_cfg then
			if cfg.powershell == "replace" then
				return ps_cfg
			end
			local sh = not cfg.shell and mare.proc.shell({ model = true })
			if sh and sh.kind == "powershell" then
				local c = { msys_blocked = true }
				for k, v in pairs(ps_cfg) do
					c[k] = v
				end
				return c
			end
		end
		return shell_config()
	end
	-- PowerShell: discovery and arguments; jobs, background and timeout
	-- handling are the bash ones
	local want_ps = cfg.powershell == true
		or cfg.powershell == "replace"
		or (cfg.powershell == "auto" and mare.os == "windows")
	if want_ps then
		ps_cfg, ps_err = mare.proc.powershell()
		if not ps_cfg and cfg.powershell ~= "auto" then
			mare.log("warn", "powershell tool unavailable: " .. tostring(ps_err))
		end
	end
	local function ps_command(cmd)
		local args = {}
		for k, v in ipairs(ps_cfg.args) do
			args[k] = v
		end
		args[#args + 1] = cmd
		return ps_cfg.path, args
	end
	if ps_cfg then
		local function ps_def(has_jobs)
			return {
				-- the short description comes before the command
				param_order = { "description", "command", "workdir", "timeout_ms", "run_in_background", "tty", "stdin" },
				side_effect = "external",
				idempotent = false,
				command_tool = true,
				command = { shell = "powershell", arg = "command" },
				snippet = "Execute PowerShell commands (Get-ChildItem, Select-String, etc.)",
				guidelines = function()
					local g = { "Use PowerShell syntax and cmdlets in the powershell tool." }
					if D.svc.jobs ~= nil then
						g[#g + 1] =
							"Use run_in_background for long-running commands; you are notified when they finish."
					end
					return g
				end,
				description = function()
					local base = "Execute a PowerShell command in the working directory (" .. ps_cfg.path .. "). "
					if D.svc.jobs ~= nil then
						return base .. "Same output limits, timeout_ms and run_in_background behaviour as bash."
					end
					return base .. "Same output limits and timeout_ms behaviour as bash."
				end,
				parameters = shell_parameters(
					"What the command does, 5-10 words, verb first",
					"PowerShell command to execute",
					has_jobs
				),
				timeout_ms = 24 * 3600 * 1000,
				resolve_cwd = resolve_workdir,
				preview = shell_preview,
				summary = function(a)
					return ("PowerShell(%s)"):format((string.gsub(a.command or "", "\n.*", " …")))
				end,
				execute = function(a, t)
					local a2 = with_workdir(a, t and t.cwd)
					local J = D.svc.jobs
					if J then
						return bash_job(J, a2, t, ps_command)
					end
					return bash_direct(a2, t, ps_command)
				end,
			}
		end
		local remove_ps = tools.register(tools, ctx, "powershell", ps_def(true))
		ctx:on("app/ready", function()
			if D.svc.jobs == nil then
				if require("mare_lib").callable(remove_ps) then
					pcall(remove_ps)
				end
				remove_ps = tools.register(tools, ctx, "powershell", ps_def(false))
			end
		end)
	end
	if cfg.powershell ~= "replace" or not ps_cfg then
		local function bash_def(has_jobs)
			return {
				-- the short description comes before the command
				param_order = { "description", "command", "workdir", "timeout_ms", "run_in_background", "tty", "stdin" },
				side_effect = "external",
				idempotent = false,
				command_tool = true,
				command = { shell = "bash", arg = "command" },
				snippet = "Execute shell commands (rg, fd, builds, tests, git, etc.)",
				guidelines = function()
					local g = {
						"Use bash for searching (rg, fd) and running builds, tests, or git; use read and ls instead"
							.. " of cat, sed, or raw ls. If you already know a file path, call read directly.",
					}
					if D.svc.jobs ~= nil then
						g[#g + 1] = "Use run_in_background for long-running commands (servers, watchers, long builds);"
							.. " you are notified when they finish. Do not busy-wait with shell sleep to poll background"
							.. " jobs; use job_output with wait = true or until when blocked."
					end
					return g
				end,
				description = function()
					local head = "Execute a bash command in the working directory. Foreground commands carry over"
						.. " working-directory changes (cd) on exit 0; environment variables reset per call and"
						.. " workdir leaves the carried-over directory unchanged. Before destructive commands (rm, mv,"
						.. " overwrite), verify the concrete target path and guard variables (e.g. ${VAR:?})."
						.. " Returns stdout and stderr, truncated to "
						.. MAX_LINES
						.. " lines or 50KB (head and tail kept). Optional timeout_ms (default "
						.. cfg.bash_timeout_ms
						.. ", max "
						.. cfg.bash_max_timeout_ms
						.. "); "
					if D.svc.jobs ~= nil then
						return head
							.. "a command still running then keeps running as a background job. "
							.. "run_in_background = true returns a job id at once (read output with job_output, write input with job_write). "
							.. "tty = true allocates a per-call PTY; stdin = true keeps stdin open for job_write."
					end
					return head .. "a command still running then is terminated."
				end,
				parameters = shell_parameters(
					"What the command does, 5-10 words, verb first (e.g. 'Run the unit tests')",
					"Shell command to execute",
					has_jobs
				),
				timeout_ms = 24 * 3600 * 1000, -- bash enforces its own timeout
				uses_shell_cwd = true, -- the snapshot / permission checks use the carried-over directory
				resolve_cwd = resolve_workdir,
				preview = shell_preview,
				summary = function(a)
					local c = string.gsub(a.command or "", "\n.*", " …")
					return ("Bash(%s)"):format(c)
				end,
				execute = function(a, t)
					return bash_with_cwd(a, t, function(a2, t2)
						local J = D.svc.jobs
						if J then
							return bash_job(J, a2, t2)
						end
						return bash_direct(a2, t2)
					end)
				end,
				render = function(a, res, expanded)
					local out = (res.details and res.details.output)
						or (type(res.content) == "string" and res.content)
						or ""
					out = string.gsub(out, "%s+$", "")
					if out == "" then
						return ui.Line({ ui.Span("(no output)"):hl("MareDim") })
					end
					local body = ui.Paragraph(ui.Line({ ui.Span(out):hl("MareMuted") }))
					if expanded then
						return body
					end
					return ui.Clip(body, 5, false)
				end,
			}
		end
		local remove_bash = tools.register(tools, ctx, "bash", bash_def(true))
		ctx:on("app/ready", function()
			if D.svc.jobs == nil then
				if require("mare_lib").callable(remove_bash) then
					pcall(remove_bash)
				end
				remove_bash = tools.register(tools, ctx, "bash", bash_def(false))
			end
		end)
	end
end
