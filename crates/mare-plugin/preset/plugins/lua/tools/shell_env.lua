-- tools.shell_env: the configured shell, command prelude and the spawned-process environment;
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local cfg = D.cfg
	local tools = D.registry.tools

	-- resolved lazily: a missing Git Bash on Windows only matters once a command runs
	local shell_cfg, shell_err
	local function shell_config()
		if not shell_cfg and not shell_err then
			shell_cfg, shell_err = mare.proc.shell({ shell_path = cfg.shell })
		end
		return shell_cfg, shell_err
	end

	--- shell_command(cmd) -> program, args, input? : applies the configured shell and prelude.
	--- `input` (stdin) is set for shells that take the script on stdin (legacy WSL launcher).
	function tools.shell_command(_, _, cmd)
		local pre = cfg.prelude
		if type(pre) == "table" then
			pre = table.concat(pre, "\n")
		end
		if pre and pre ~= "" then
			-- variables the prelude activates (mise, direnv) are scrubbed like the inherited ones
			-- explicitly kept / set names survive
			local keep = {}
			for _, n in ipairs((cfg.env or {}).include or {}) do
				keep[#keep + 1] = n
			end
			for n in pairs((cfg.env or {}).set or {}) do
				keep[#keep + 1] = n
			end
			local skip = #keep > 0 and ('case "$__mare_v" in %s) continue;; esac; '):format(table.concat(keep, "|"))
				or ""
			cmd = pre
				.. "\nfor __mare_v in $(env | sed -n 's/^\\([A-Za-z_][A-Za-z0-9_]*\\)=.*/\\1/p'); do "
				.. skip
				.. 'case "$(printf %s "$__mare_v" | tr a-z A-Z)" in *KEY*|*PASSWORD*|*SECRET*|*TOKEN*) unset "$__mare_v";; esac; '
				.. "done; unset __mare_v\n"
				.. cmd
		end
		local sh, err = shell_config()
		if not sh then
			error(err, 0)
		end
		local args = {}
		for i, a in ipairs(sh.args) do
			args[i] = a
		end
		if sh.transport == "stdin" then
			return sh.path, args, cmd
		end
		args[#args + 1] = cmd
		return sh.path, args
	end

	--- process_env(extra?) -> env for spawned commands (MARE_* + non-interactive defaults)
	function tools.process_env(_, _, extra)
		local env = {
			TERM = "dumb",
			PAGER = "cat",
			GIT_PAGER = "cat",
			GH_PAGER = "cat",
			NO_COLOR = "1",
			MARE_CWD = mare.cwd,
		}
		-- a UTF-8 locale when the user's environment sets none; never
		-- overrides LANG / LC_ALL / LC_CTYPE the user set. macOS has no C.UTF-8 locale.
		local function set_env(k)
			local v = mare.env(k)
			return v ~= nil and v ~= ""
		end
		if mare.os ~= "windows" and not (set_env("LANG") or set_env("LC_ALL") or set_env("LC_CTYPE")) then
			local loc = mare.os == "macos" and "en_US.UTF-8" or "C.UTF-8"
			env.LANG, env.LC_CTYPE = loc, loc
		end
		-- MARE_SESSION_*, never MARE_<OPTION>: MARE_MODEL / MARE_EFFORT are the config env layer,
		-- so a mare started by a tool would take this session's model over its own init.lua
		-- (session env vars are facts for children, never config a nested instance reads)
		local llm = D.svc.llm
		local m = llm and llm:current()
		if m then
			env.MARE_SESSION_MODEL = m.ref
			env.MARE_SESSION_EFFORT = llm:effort() or ""
		end
		local ok, st = pcall(function()
			local agent = D.svc.agent
			return agent and agent:activity()
		end)
		if ok and st and st.session_id then
			env.MARE_SESSION_ID = st.session_id
		else
			local ok_s, info = pcall(function()
				return D.svc.session and D.svc.session:info()
			end)
			if ok_s and info and info.session_id then
				env.MARE_SESSION_ID = info.session_id
			end
		end
		-- this session's peer socket, when it has one: `mare peer send` from a tool reaches
		-- it
		local me = mare.peers and mare.peers.self()
		if me and me.socket then
			env.MARE_PEER_SOCKET = me.socket
		end
		for k, v in pairs((cfg.env or {}).set or {}) do
			env[k] = v
		end
		for k, v in pairs(extra or {}) do
			env[k] = v
		end
		return env
	end
	--- env_policy() -> {env_include, env_exclude} for mare.proc.run / start / pty
	function tools.env_policy(_, _)
		local e = cfg.env or {}
		return { env_include = e.include or {}, env_exclude = e.exclude or {} }
	end

	return { shell_config = shell_config }
end
