-- session.startup: app/ready — --continue / --resume / --fork-session / --session-id / --name;
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, disabled, dir, session = D.ctx, D.S, D.disabled, D.dir, D.session
	local ensure_file = D.store.ensure_file
	local load_file = D.lifecycle.load_file
	local function index_refresh(...)
		return D.pickers.index_refresh(...)
	end
	local function index_list(...)
		return D.pickers.index_list(...)
	end
	local function resolve_resume(...)
		return D.pickers.resolve_resume(...)
	end
	local function pick_session(...)
		return D.pickers.pick_session(...)
	end

	--- start a new session with a given id (--session-id); refused when one exists
	local function use_id(id)
		if type(id) ~= "string" or not id:match("^[%w%-_]+$") or #id < 8 or #id > 64 then
			return nil, ("--session-id %q: use 8-64 letters, digits, - or _ (e.g. a UUID)"):format(tostring(id))
		end
		if #mare.store.index_read({ session_id = id, kind = "any", archived = "any" }) > 0 then
			return nil, ("a session with id %s already exists — resume it with --resume %s"):format(id, id)
		end
		S.id = id
		S.planned = require("mare_lib").session_file_path(dir, S.id)
		ctx.agent:set_session_id(S.id)
		if D.ctx and D.ctx.kernel and D.ctx.kernel.begin_session_switch then
			local sw <close> = D.ctx.kernel:begin_session_switch({ session_id = S.id })
			sw:commit(S.id)
		end
		return true
	end

	-- startup: --continue / --resume (-r <id|name|-N>, -r alone opens the picker), with
	-- --fork-session (continue in a copy), --session-id and --name. Without a terminal a
	-- session that can't be found is an error (headless exits 1), never a silent new one.
	-- A full /reload restarts this plugin with an empty session: the session the user was in is
	-- remembered across it and reopened once the reload is done, never the
	-- command line's --resume / --continue again (those name the session the process started in).
	-- The record belongs to the reload that restarted this instance: only that reload's
	-- kernel/reloaded reopens it (a `/reload config` restarts this plugin without one, and a later
	-- unrelated reload must not go back to the old session), and a session the user starts or
	-- switches to meanwhile (session/changed new / resume / fork) voids it.
	local CARRY = "session"
	local carried = mare.g[CARRY]
	mare.g[CARRY] = nil
	if
		not (
			type(carried) == "table"
			and (type(carried.path) == "string" or type(carried.id) == "string")
			and type(carried.generation) == "number"
		)
	then
		carried = nil
	end
	-- stamped when this instance is unloaded: the reload running then (its generation moved at
	-- its start, before any unload) is the one whose kernel/reloaded may reopen it — not whichever
	-- reload next starts this plugin
	local kernel = ctx.kernel
	ctx:effect(function()
		return function()
			if not disabled and (S.path or S.id) then
				mare.g[CARRY] = {
					path = S.path,
					id = S.id,
					generation = kernel:reload_generation(),
				}
			end
		end
	end, "session: carry the live session across a reload")
	ctx:on("session/changed", function(ev)
		local reason = type(ev) == "table" and ev.reason
		if reason == "new" or reason == "resume" or reason == "fork" then
			carried = nil
		end
	end)
	ctx:on("kernel/reloaded", function()
		local record = carried
		carried = nil
		if not record or record.generation ~= ctx.kernel:reload_generation() then
			return
		end
		if record.path then
			if record.path == S.path or not mare.fs.exists(record.path) then
				return
			end
			-- restoring where the user was, not a switch the user asked for: no before-switch vote
			local ok, err = pcall(load_file, record.path, nil, { skip_policy = true, _from_reload = true })
			if not ok then
				mare.log("warn", "the session could not be reopened after the reload: " .. tostring(err))
			end
		elseif record.id and not S.path and S.id ~= record.id then
			S.id = record.id
			S.planned = require("mare_lib").session_file_path(dir, S.id)
			ctx.agent:set_session_id(S.id)
			if D.ctx and D.ctx.kernel and D.ctx.kernel.begin_session_switch then
				local sw <close> = D.ctx.kernel:begin_session_switch({ session_id = S.id })
				sw:commit(S.id)
			end
			S.started = true
			ctx.bus:emit("session/started", { session_id = S.id, path = S.planned, resumed = false })
		end
	end)

	ctx:on("app/ready", function()
		if carried ~= nil then
			-- a replay to the restarted plugin during a reload: kernel/reloaded reopens the session
			return
		end
		local args = mare.args or {}
		-- the index follows the session files: catch up whatever changed while
		-- no mare was indexing it (a crash, an older mare), in the background — listings
		-- read the index meanwhile and are never blocked by it
		if not disabled then
			ctx:spawn(function()
				index_refresh().reconcile()
			end)
		end
		-- `mare session new`: an empty session (header only), indexed so --resume <id> finds it
		if args.session_new then
			if pcall(ensure_file) and S.path then
				if args.name then
					D.set_name(args.name)
				end
				pcall(mare.store.index_append, { session_id = S.id, path = S.path, cwd = mare.cwd })
			end
			return
		end
		local interactive = ui.connected()
		local fork = args.fork_session and { id = args.session_id, name = args.name } or nil
		local target, known, err, err_code
		local function capture_error(e, prefix)
			err_code = type(e) == "table" and type(e.code) == "string" and e.code or nil
			local text = type(e) == "table" and type(e.message) == "string" and e.message or tostring(e)
			err = prefix and (prefix .. tostring(e)) or text
		end
		if disabled and (args.continue or args.resume ~= nil) then
			-- nothing is opened
			err = "--no-session: sessions are neither resumed nor written (drop -c / --resume or --no-session)"
		elseif args.continue then
			-- the latest few: rows whose file is gone are skipped
			local list = index_list(args.all, nil, nil, 20)
			if not list[1] then
				-- not indexed yet (the index line is written in the background): the newest
				-- session file here that has more than its header
				for _, f in ipairs(mare.store.list(dir)) do
					if f.header and f.size > 0 and f.path ~= S.path then
						local ok, tail = pcall(mare.store.read_tail, f.path, 2)
						if ok and #tail > 1 then
							list[1] = { path = f.path }
							break
						end
					end
				end
			end
			if list[1] then
				target, known = list[1].path, list[1].session_id and list[1] or nil
			else
				err = args.all and "no session to continue"
					or "no session to continue in this directory (-c --all looks in every directory)"
			end
		elseif args.resume ~= nil and args.resume ~= "" then
			local search
			target, known, search = resolve_resume(args.resume, args.all, interactive)
			if search then
				-- not a full id (`-r <text>`): the picker opens with it as the
				-- search text — "(no matches)" when nothing fits, Esc shows the whole list
				local pok, perr =
					pcall(pick_session, args.all, { fork = fork, search = args.resume, propagate_error = true })
				if not pok then
					capture_error(perr)
				end
			elseif not target then
				err = known
				known = nil
			end
		elseif args.resume == "" then
			if interactive then
				-- app/ready is the startup barrier: finish the picker before queued input runs.
				-- A cancelled picker (or none to pick) starts a new session, as without -r:
				-- an error here would block the TUI's input behind a failed startup.
				local pok, perr = pcall(pick_session, args.all, { fork = fork, propagate_error = true })
				if not pok then
					capture_error(perr)
				end
			else
				err = "--resume without a session needs the interactive picker; give an id, a name or -N"
			end
		elseif args.fork_session then
			err = "--fork-session goes with -c or --resume"
		end
		local named_by_fork = false
		local restore_perm_ceiling = args.restore_permission_ceiling
		local restore_sb_ceiling = args.restore_sandbox_ceiling
		args.restore_permission_ceiling = nil
		args.restore_sandbox_ceiling = nil
		local load_opts = (restore_perm_ceiling ~= nil or restore_sb_ceiling ~= nil)
				and {
					restore_permission_ceiling = restore_perm_ceiling,
					restore_sandbox_ceiling = restore_sb_ceiling,
				}
			or nil
		if type(fork) == "table" and load_opts then
			fork.restore_permission_ceiling = restore_perm_ceiling
			fork.restore_sandbox_ceiling = restore_sb_ceiling
		end
		if target then
			local ok, lerr = pcall(load_file, target, type(known) == "table" and known or nil, load_opts)
			if not ok then
				err_code = type(lerr) == "table" and type(lerr.code) == "string" and lerr.code or nil
				err = tostring(lerr)
			elseif fork and S.path == target then
				local fok, ferr = pcall(D.fork.fork_at, args.fork_before_entry, args.fork_before_entry == nil, fork)
				if not fok then
					capture_error(ferr, "--fork-session: ")
				end
				named_by_fork = true
			end
		elseif mare.mode ~= "child" and args.session_id and not err and not fork then
			local ok, ierr = use_id(args.session_id)
			if not ok then
				err = ierr
			end
		end
		if args.name and not err and not named_by_fork then
			D.set_name(args.name)
		end
		if not err and type(args.add_dirs) == "table" and #args.add_dirs > 0 then
			local ok_add, aerr = pcall(session.add_workspace_dirs, session, ctx, args.add_dirs)
			if not ok_add then
				capture_error(aerr)
			end
		end
		if err then
			S.resume_error = err
			S.resume_error_code = err_code
			-- the TUI reports it itself, as the startup block (`resume_error`): one message,
			-- not a second warning toast with the same text
			if interactive and mare.mode ~= "tui" then
				ui.notify(err, "warning")
			else
				mare.log("warn", err)
			end
			return
		end
		S.started = true
		ctx.bus:emit("session/started", { session_id = S.id, path = S.path or S.planned, resumed = S.path ~= nil })
	end)
end
