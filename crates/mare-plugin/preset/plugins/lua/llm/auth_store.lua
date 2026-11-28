-- llm.auth_store: auth.json reads and writes (corrupt-file guard, lock); part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local ctx, llm = D.ctx, D.llm
	local lib = require("mare_lib")
	local AUTH_VERSION = 1

	--- auth.json helpers shared by providers.
	--- read_auth -> table (empty when the file is missing) | nil, err when it is corrupt: the
	--- file is copied to auth.json.bak and nothing is written over it (a parse error keeps the file
	--- and refuses writes, instead of replacing every credential with an empty object)
	local warned_corrupt = false
	local function read_auth(lossless)
		local ok_read, s, read_err, code = pcall(mare.fs.read, mare.paths.auth)
		if not ok_read then
			return nil, ("%s cannot be read: %s"):format(mare.paths.auth, tostring(s)), "io"
		end
		if not s then
			if code == "not_found" then
				return {}
			end
			return nil, ("%s cannot be read: %s"):format(mare.paths.auth, tostring(read_err)), "io"
		end
		local body = lib.strip_utf8_bom(s)
		local ok, t = pcall(mare.json.decode, body)
		if not ok then
			return nil, tostring(t), "corrupt"
		end
		if
			type(t) == "table"
			and not mare.json.is_array(t)
			and math.type(t.version) == "integer"
			and t.version == AUTH_VERSION
		then
			if lossless then
				local converted, result, err = pcall(mare.json.decode_lossless, body)
				if not converted then
					return nil, tostring(result), "corrupt"
				end
				if not result then
					return nil, tostring(err), "corrupt"
				end
				t = result
			end
			t.version = nil
			return t
		end
		local bak = mare.paths.auth .. ".bak"
		pcall(function()
			if mare.fs.read(bak) ~= s then
				mare.fs.write(bak, s, { mode = 384, atomic = true })
			end
		end)
		local err = (
			"%s is corrupt (invalid JSON or missing, invalid, or unsupported version): saved credentials can't be read or changed "
			.. "until it's fixed — mare tries to keep a copy in %s; fix the file or delete it and log in again"
		):format(mare.paths.auth, bak)
		if not warned_corrupt then
			warned_corrupt = true
			mare.log("warn", err)
			if mare.ui.connected() then
				mare.ui.notify(err, "error")
			end
		end
		return nil, err, "corrupt"
	end
	local function check_auth_mode()
		local ok_stat, st, stat_err, stat_code = pcall(mare.fs.stat, mare.paths.auth, { strict = true })
		if not ok_stat then
			return false, ("%s cannot be read: %s"):format(mare.paths.auth, tostring(st))
		end
		if not st then
			if stat_code == "not_found" then
				return true
			end
			return false, ("%s cannot be read: %s"):format(mare.paths.auth, tostring(stat_err))
		end
		if st.mode and st.mode & 63 ~= 0 then -- 63 = 0o077
			return false,
				("%s is readable by other users (mode %o): refusing to use it — run `mare doctor --fix` (or chmod 600)"):format(
					mare.paths.auth,
					st.mode
				)
		end
		return true
	end
	function llm.auth_get(_, _, key)
		if key == "version" then
			error("auth.json: 'version' is reserved for the file format", 0)
		end
		-- a credential file other users can read is refused, not used
		local ok_mode, mode_err = check_auth_mode()
		if not ok_mode then
			error(mode_err, 0)
		end
		local t, err = read_auth()
		if not t then
			error(err, 0)
		end
		return t[key]
	end
	--- set (value = nil removes) one entry. The read-modify-write holds the cross-process
	--- auth lock unless the caller already holds it (`opts.locked`,
	--- e.g. an OAuth refresh); a corrupt file is never overwritten.
	-- Compare the persisted credential record, including its current token generation.
	-- Never compare only account_id: a new login to the same account must also win.
	local function same(a, b)
		if type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return a == b
		end
		for k, v in pairs(a) do
			if not same(v, b[k]) then
				return false
			end
		end
		for k in pairs(b) do
			if a[k] == nil then
				return false
			end
		end
		return true
	end
	function llm.auth_set(_, _, key, value, opts)
		if key == "version" then
			error("auth.json: 'version' is reserved for the file format", 0)
		end
		local cancel = opts and opts.cancel or nil
		if cancel and cancel:is_cancelled() then
			error({ code = "cancelled", message = "login cancelled" }, 0)
		end
		local guard = not (opts and opts.locked)
			and mare.lock.file(mare.paths.auth .. ".lock", { timeout_ms = 30000, cancel = cancel })
		local ok, err = pcall(function()
			local t, rerr = read_auth(true)
			if not t then
				error(rerr, 0)
			end
			if cancel and cancel:is_cancelled() then
				error({ code = "cancelled", message = "login cancelled" }, 0)
			end
			if opts and opts.expected ~= nil then
				-- Match the value exposed by auth_get, where JSON nulls decode as nil; keep the
				-- lossless table for the write so unrelated fields still roundtrip exactly.
				local current = t[key]
				if current ~= nil then
					current = mare.json.decode(mare.json.encode_strict(current))
				end
				if not same(current, opts.expected) then
					return false
				end
			end
			t[key] = value
			t.version = AUTH_VERSION
			assert(mare.fs.write(mare.paths.auth, mare.json.encode_strict(t), { mode = 384, atomic = true }))
			return true
		end)
		if guard then
			guard:release()
		end
		if not ok then
			error(err, 0)
		end
		return err
	end

	return { read_auth = read_auth, check_mode = check_auth_mode, same = same }
end
