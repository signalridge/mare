-- session.pickers: message text helpers and the lazy session browser / index refresher
-- behind /resume and the startup picker;
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, dir, session = D.ctx, D.S, D.dir, D.session
	local load_file = D.lifecycle.load_file

	--- first text block of a message
	local function msg_text(m)
		if type(m.content) == "string" then
			return m.content
		end
		for _, b in ipairs(m.content or {}) do
			if b.type == "text" then
				return b.text
			end
		end
		return ""
	end

	--- user entries that aren't the user's own prompts: `!` shell commands, notifications,
	--- interrupt markers, compaction summaries (hidden from /tree, /fork, /undo)
	local function own_prompt(m)
		return m.role == "user"
			and not m.compaction
			and not m.notification
			and not m.turn_aborted
			and not m.branch_summary
			and not string.find(msg_text(m), "^<bash command=")
	end

	local after_pick -- set below; the browser can only call it after startup completes
	local browser
	local refresher -- the index/file reconcile, shared by startup and the pickers
	local function index_refresh()
		refresher = refresher or require("session.refresh")(ctx, D.warn_unsupported)
		return refresher
	end
	local function session_browser()
		if not browser then
			browser = require("session.browser")({
				ctx = ctx,
				-- the plugin's own implementation (called as (impl, caller, ...)), not its service
				session = session,
				cfg = D.cfg,
				refresh = index_refresh(),
				state = S,
				dir = dir,
				msg_text = msg_text,
				own_prompt = own_prompt,
				after_pick = function(...)
					return after_pick(...)
				end,
				load_transcript = function(path, title)
					return session.load_transcript(session, ctx, path, title)
				end,
				switching = function()
					return D.store.switching()
				end,
				delete_files = function(...)
					return D.metadata.delete_files(...)
				end,
				annotate = function(...)
					return D.metadata.annotate(...)
				end,
			})
		end
		return browser
	end
	local function relative_time(...)
		return session_browser().relative_time(...)
	end
	local function index_list(...)
		return session_browser().index_list(...)
	end
	local function resolve_resume(...)
		return session_browser().resolve_resume(...)
	end
	local function refuse_busy(...)
		return session_browser().refuse_busy(...)
	end
	local function pick_session(...)
		return session_browser().pick_session(...)
	end
	local function search_sessions(...)
		return session_browser().search_sessions(...)
	end
	--- short_id(id) -> the shortest prefix (8+ characters) of a session id that no other session
	--- in the index shares (git's short hashes): what `-r <id>` / `/resume <id>` take back
	function session.short_id(_, _, id)
		return session_browser().short_id(id)
	end
	after_pick = function(s, opts)
		local ok, err
		if opts and opts.fork then
			ok, err = pcall(load_file, s.path, s)
		elseif S.started then
			local svc = assert(ctx:get("session"), "session service is unavailable")
			ok, err = pcall(svc.load, svc, { path = s.path })
		else
			ok, err = pcall(load_file, s.path, s)
		end
		if not ok then
			if opts and opts.propagate_error then
				error(err, 0)
			end
			ui.notify(type(err) == "table" and err.message or tostring(err), "warning")
			return
		end
		-- --fork-session: continue in a copy (a session that had to be forked anyway already is)
		if opts and opts.fork and S.path == s.path then
			D.fork.fork_at(nil, true, opts.fork)
		end
	end

	return {
		msg_text = msg_text,
		own_prompt = own_prompt,
		index_refresh = index_refresh,
		relative_time = relative_time,
		index_list = index_list,
		resolve_resume = resolve_resume,
		refuse_busy = refuse_busy,
		pick_session = pick_session,
		search_sessions = search_sessions,
	}
end
