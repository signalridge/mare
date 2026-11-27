-- Read-only view of a session: chosen when the file is locked by another process or
-- damaged. Nothing is written — the conversation goes on in memory only — and tools with side
-- effects are refused with a result that explains how to fork a completed snapshot. Loaded on first use.
return function()
	local M = {}

	--- the one notice when a view opens (the status bar keeps a `read-only` badge meanwhile)
	M.notice = "read-only view — nothing is saved; tools with side effects won't run; "
		.. "fork a completed snapshot; back up or repair damaged sources first"

	local refusal = "this session is open as a read-only view (nothing is saved), so tools with side effects "
		.. "don't run here; fork a completed snapshot; back up or repair damaged sources first"

	--- tool/before vote: deny a call that isn't side-effect free (read-only tools still run)
	function M.vote(info)
		if (info and info.side_effect or "external") ~= "read" then
			return { decision = "deny", reason = refusal }
		end
		return nil
	end

	return M
end
