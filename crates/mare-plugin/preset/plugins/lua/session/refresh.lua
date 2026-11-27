-- Keeping the session index in step with the session files. One reconcile at a
-- time per process: callers that arrive while one runs wait for its result instead of starting
-- another, and every reconcile that changed rows bumps `changes`, so a picker that read the
-- index before the change knows it is stale however the reconcile was started (startup,
-- another picker, a lookup). Constructed once by the session entry.
return function(ctx, warn_unsupported)
	local M = { changes = 0 }
	local waiting -- list of channels while a reconcile runs
	function M.warn_unsupported(paths)
		warn_unsupported(paths)
	end

	--- one notice when the index had to be rebuilt in this process (damaged, a schema change,
	--- missing): the first reconcile after the open — at startup or when the session list is
	--- opened — hands it out, nobody after. doctor keeps reporting it.
	function M.announce()
		local ok, why = pcall(mare.store.index_notice)
		if ok and why then
			ui.notify(("session index rebuilt (%s)"):format(why), "info")
		end
	end

	--- reconcile the index with the files (off-thread, stat only for unchanged files) ->
	--- {files, updated, removed, failed, pending} | nil (failed: logged, the index stays as it is)
	local MAX_PENDING_WAIT_MS = 2000
	function M.reconcile()
		if waiting then
			local tx, rx = mare.task.channel()
			waiting[#waiting + 1] = tx
			return rx:recv() or nil
		end
		local joined = {}
		waiting = joined
		local settled_result
		local _release <close> = require("mare_lib").guard(function()
			if waiting == joined then
				waiting = nil
			end
			for _, tx in ipairs(joined) do
				tx:send(settled_result or false)
			end
		end)
		local ok, r, delay = true, nil, 10
		local started = mare.task.clock()
		repeat
			ok, r = pcall(mare.store.index_reconcile)
			if not ok then
				mare.log("warn", "session index not reconciled: " .. tostring(r))
				r = nil
				break
			end
			if (r.updated or 0) + (r.removed or 0) > 0 then
				M.changes = M.changes + 1
			end
			if (r.pending or 0) == 0 then
				break
			end
			if mare.task.clock() - started >= MAX_PENDING_WAIT_MS then
				mare.log("warn", ("session index reconcile timed out with %d pending file(s)"):format(r.pending))
				break
			end
			-- A session writer holds its lock only while rolling back the failed append. Keep this
			-- background reconcile alive until the writer completes (bounded by MAX_PENDING_WAIT_MS)
			-- so open pickers receive the resulting index change instead of staying stale or hanging.
			if not mare.task.sleep(delay) then
				break
			end
			delay = math.min(delay * 2, 250)
		until false
		if not ok then
			r = nil
		elseif r then
			settled_result = r
			M.warn_unsupported(r.unsupported)
		end
		M.announce()
		_release:release()
		return r
	end

	--- synchronous catch-up for callers that need the answer now (resolving an id or `-N`,
	--- `--continue`, content search): at most once per 2 s. Pickers don't use it — they open on
	--- the index as it is and `watch` for changes.
	local caught_up_at
	function M.catch_up()
		local now = mare.task.clock()
		if caught_up_at and now - caught_up_at < 2000 then
			return
		end
		caught_up_at = now
		M.reconcile()
	end

	--- run a reconcile in the background; when the index changed since `seen` (the value of
	--- `changes` read before the caller's own index read), call `on_changed()`. A reconcile
	--- already running when the watch starts may have scanned before a file the caller should
	--- see was written, so joining one is followed by a reconcile of the watch's own.
	function M.watch(seen, on_changed)
		return ctx:spawn(function()
			local joined = waiting ~= nil
			M.reconcile()
			if joined then
				M.reconcile()
			end
			if M.changes ~= seen then
				on_changed()
			end
		end)
	end

	return M
end
