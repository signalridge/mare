-- session.snapshots: per-turn file snapshots (the Ctrl+R "session" scope) and the
-- tool/before votes (read-only refusal, snapshot);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session

	-- in-memory record of each file's content before this session first edited it (the
	-- "session" scope of the Ctrl+R review page); /rewind uses core's snapshot journal instead
	-- S.turn_id: id of the latest user prompt (set by session.store's add)
	local snapshots = {} -- { {turn = id, path = abs, content = string|false} } in order
	local snapped = {} -- turn .. "\0" .. path -> true
	ctx:on("session/changed", function()
		snapshots = {}
		snapped = {}
	end)

	--- files edited by tools in this session: { {path, old (string|false), new (string|false)} }
	--- old = content before the first edit, new = content now (Ctrl+R "session" scope)
	function session.file_changes(_, _)
		local out, seen = {}, {}
		for _, snap in ipairs(snapshots) do
			if not seen[snap.path] then
				seen[snap.path] = true
				local now = mare.fs.read(snap.path) or false
				if now ~= snap.content then
					out[#out + 1] = { path = snap.path, old = snap.content, new = now }
				end
			end
		end
		return out
	end
	--- paths a tool call may modify (edit / write / apply_patch / anything with a path arg)
	local function touched_paths(call)
		local a = call.arguments or {}
		local out = {}
		for _, k in ipairs({ "path", "file_path" }) do
			if type(a[k]) == "string" then
				out[#out + 1] = a[k]
			end
		end
		local patch = a.input or a.patch
		if type(patch) == "string" then
			for _, p in string.gmatch(patch, "%*%*%* (%a+) File: ([^\n]+)") do
				out[#out + 1] = (string.gsub(p, "%s+$", ""))
			end
			for p in string.gmatch(patch, "%*%*%* Move to: ([^\n]+)") do
				out[#out + 1] = (string.gsub(p, "%s+$", ""))
			end
		end
		return out
	end
	local function on_tool_before(call)
		if not S.turn_id or not call or call.name == "read" or call.name == "grep" or call.name == "ls" then
			return nil
		end
		for _, p in ipairs(touched_paths(call)) do
			local abs = mare.fs.resolve(p)
			local key = S.turn_id .. "\0" .. abs
			if not snapped[key] then
				snapped[key] = true
				local content = mare.fs.read(abs)
				snapshots[#snapshots + 1] = { turn = S.turn_id, path = abs, content = content or false }
			end
		end
		return nil
	end
	if not D.lazy_snapshots then
		-- a read-only view refuses tools with side effects (they'd change files or the world while
		-- nothing records that they ran); read-only tools still run
		ctx.policy:voter("tool/before", function(_, info)
			if S.readonly then
				return require("session.readonly")().vote(info)
			end
			return nil
		end)
		-- the file's content before a turn changes it (for /undo). Not ordered: it never objects, and a
		-- tool an earlier voter refuses does not run, so a snapshot it skips is not needed (it was
		-- priority 1000)
		ctx.policy:voter("tool/before", on_tool_before)
	end

	return { touched_paths = touched_paths, on_tool_before = on_tool_before }
end
