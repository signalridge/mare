-- context.runtime: the runtime facts at the end of the system prompt (`cwd`, `environment`; the
-- sandbox is the permissions plugin's) and the built-in prompt variables (`{{model}}`,
-- `{{provider}}`, `{{cwd}}`, `{{os}}`, `{{shell}}`); part of the `context` plugin
-- (plugins/context.lua), loaded at the start of its apply.
-- Runtime facts go last (order >= 10000), so everything above stays a stable cacheable prefix.
local OS_NAMES = { macos = "macOS", linux = "Linux", windows = "Windows" }

return function(D)
	local ctx, context = D.ctx, D.context

	local function shell()
		local tools = D.tools()
		return tools and tools:shell() or nil
	end

	-- OS and shell, so commands follow the platform's conventions
	context.section(context, ctx, {
		name = "environment",
		runtime = true, -- a runtime fact: context:suppress_runtime hides it
		order = 10001,
		interpolate = false,
		text = function()
			local lines = { "OS: " .. (OS_NAMES[mare.os] or mare.os) }
			local sh = shell()
			if sh then
				local kind = sh.kind == "wsl" and "bash (legacy WSL launcher)" or sh.kind
				lines[#lines + 1] = ("Shell for commands: %s (%s)"):format(kind, sh.path)
				if sh.kind == "powershell" then
					lines[#lines + 1] = "Use the powershell tool for commands (PowerShell syntax)."
					if sh.msys_blocked then
						-- the Windows default-tier sandbox
						lines[#lines] = lines[#lines]
							.. " Git Bash can't start inside this sandbox: the bash tool fails with SANDBOX_MSYS_UNSUPPORTED."
					end
				elseif mare.os == "windows" and sh.kind ~= "wsl" then
					lines[#lines + 1] = "Commands run in Git Bash: use POSIX syntax and forward slashes;"
						.. " Windows paths like C:\\x are /c/x there."
				end
			end
			return table.concat(lines, "\n")
		end,
	})
	context.section(context, ctx, {
		name = "cwd",
		runtime = true, -- a runtime fact: context:suppress_runtime hides it
		order = 10000,
		interpolate = false,
		text = function()
			local extra = {}
			for _, d in ipairs(mare.workspace and mare.workspace.dirs() or {}) do
				if d ~= mare.cwd then
					extra[#extra + 1] = d
				end
			end
			if #extra == 0 then
				return mare.cwd
			end
			return mare.cwd .. "\n\nAdditional workspace directories:\n" .. table.concat(extra, "\n")
		end,
	})

	-- variables for interpolated sections (plugin templates; file and user text never is)
	context.variable(context, ctx, "model", function(info)
		return info.model and info.model.id or nil
	end)
	context.variable(context, ctx, "provider", function(info)
		return info.model and info.model.provider or nil
	end)
	context.variable(context, ctx, "cwd", function()
		return mare.cwd
	end)
	context.variable(context, ctx, "os", function()
		return OS_NAMES[mare.os] or mare.os
	end)
	context.variable(context, ctx, "shell", function()
		local sh = shell()
		return sh and sh.kind or nil
	end)
end
