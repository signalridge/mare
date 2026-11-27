-- mcp.approvals: the approval hash (persisted in mcp-approvals.json) and the
-- per-project enabled/disabled map;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local read_json, write_json = D.read_json, D.write_json
	local approvals_path, disabled_path = D.approvals_path, D.disabled_path

	-- ---- persistent state: approvals and per-project enabled/disabled ------------------

	local function with_lock(path, fn)
		local okg, guard = pcall(mare.lock.file, path .. ".lock", { timeout_ms = 5000 })
		if not okg then
			error(tostring(guard), 0)
		end
		local ok, err = pcall(fn)
		guard:release()
		if not ok then
			error(tostring(err), 0)
		end
	end
	local function approvals()
		local j, err = read_json(approvals_path)
		if err then
			return nil, err
		end
		if j == nil then
			return { approved = {} }
		end
		if type(j) ~= "table" or #j > 0 or (j.approved ~= nil and type(j.approved) ~= "table") then
			return nil, approvals_path .. ": corrupt (expected JSON object)"
		end
		j.approved = j.approved or {}
		return j
	end
	local function disabled_map()
		local j, err = read_json(disabled_path)
		if err then
			return nil, err
		end
		if j == nil then
			return {}
		end
		if type(j) ~= "table" or #j > 0 then
			return nil, disabled_path .. ": corrupt (expected JSON object)"
		end
		return j
	end
	local function approval_key(e)
		local r = e.raw
		local identity = {
			command = r.command,
			args = r.args,
			env = r.env,
			env_file = r.env_file,
			env_include = r.env_include,
			env_exclude = r.env_exclude,
			cwd = r.cwd,
			url = r.url,
			headers = r.headers,
			env_http_headers = r.env_http_headers,
			bearer_token = r.bearer_token,
			bearer_token_env_var = r.bearer_token_env_var,
			oauth = r.oauth,
			tls = r.tls,
			type = r.type,
			tier = r.tier,
			tools = r.tools,
			enabled_tools = r.enabled_tools,
			disabled_tools = r.disabled_tools,
			-- project overlays may change these (config.lua OVERLAY): they decide whether the
			-- server starts, whether it starts eagerly and what the model is told about it
			enabled = r.enabled,
			required = r.required,
			description = r.description,
			-- each partial overlay's layer and file (nil when none, so keys of plain definitions
			-- keep their persisted bytes)
			overlays = e.overlays,
		}
		return e.name .. "|" .. mare.crypto.sha256(mare.json.encode(identity)):sub(1, 16)
	end
	local function needs_approval(e)
		return e.source == "project" or e.source == "env"
	end
	local function approved(e)
		if not needs_approval(e) then
			return true
		end
		local a, err = approvals()
		if not a then
			return false, err
		end
		local rec = a.approved[approval_key(e)]
		return rec == true or (type(rec) == "number" and rec > 0)
	end
	local function approve(e)
		with_lock(approvals_path, function()
			local a, err = approvals()
			if not a then
				error(err, 0)
			end
			a.approved[approval_key(e)] = mare.task.now and mare.task.now() or true
			local ok, werr = write_json(approvals_path, a)
			if not ok then
				error(werr, 0)
			end
		end)
	end
	local function enabled(e)
		local m, err = disabled_map()
		if not m then
			return false, err
		end
		local proj = m[mare.cwd]
		if proj ~= nil and type(proj) ~= "table" then
			return false, disabled_path .. ": corrupt project entry"
		end
		local d = proj and proj[e.name]
		if d ~= nil then
			return d == false
		end
		return e.raw.enabled ~= false
	end
	local function set_enabled(e, on)
		with_lock(disabled_path, function()
			local m, err = disabled_map()
			if not m then
				error(err, 0)
			end
			if m[mare.cwd] ~= nil and type(m[mare.cwd]) ~= "table" then
				error(disabled_path .. ": corrupt project entry", 0)
			end
			m[mare.cwd] = m[mare.cwd] or {}
			m[mare.cwd][e.name] = not on
			local ok, werr = write_json(disabled_path, m)
			if not ok then
				error(werr, 0)
			end
		end)
	end

	return { approved = approved, approve = approve, enabled = enabled, set_enabled = set_enabled }
end
