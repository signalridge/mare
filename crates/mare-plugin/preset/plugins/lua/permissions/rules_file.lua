-- The shared "always allow" rule table for the
-- `permissions` plugin.
--
-- Several writers share one file: two sessions, or a parent writing a rule its sub-agent's
-- "always allow" produced while the sub-agent reads it. So the file is never written from a
-- copy read at startup: every write takes the cross-process lock `<file>.lock`
-- (mare.lock.file, the primitive the credential store uses), re-reads the file, merges the new
-- rule into what is there now and writes atomically (temp file + rename, 0600) — a rule another
-- process added in between is kept (locked read-modify-write, then refresh the policy).
-- A file that isn't a JSON object is never overwritten. Readers pick up other writers' rules on
-- the next `refresh()`; compare contents so same-size, same-mtime replacements are not missed.
--
-- Format: { version = 1, rules = {...}, ... } — unknown top-level fields and unknown rule fields
-- are kept verbatim.
local lib = require("mare_lib")
local VERSION = 1

--- structural equality of two decoded JSON values (a rule already in the file isn't added twice)
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

return function(path, bump_policy_revision)
	local S = { rules = {}, stamp = nil, error = nil }

	--- the file's document: {} when missing or empty; nil, err when corrupt or unsupported version
	local function load()
		local st, serr, kind = mare.fs.stat(path, { strict = true })
		if not st then
			if kind == "not_found" then
				return {}, nil, "missing"
			end
			return nil, path .. ": " .. tostring(serr or "stat failed"), "stat-error:" .. tostring(serr)
		end
		local text, rerr = mare.fs.read(path)
		if text == nil then
			return nil, path .. ": " .. tostring(rerr or "read failed"), "read-error:" .. tostring(rerr)
		end
		if not text:match("%S") then
			return {}, nil, "bytes:" .. text
		end
		local ok, doc = pcall(mare.json.decode_lossless, text)
		if not ok or type(doc) ~= "table" or #doc > 0 then
			return nil, path .. " is corrupt (not a JSON object): not overwritten", "bytes:" .. text
		end
		if doc.version ~= VERSION then
			return nil,
				("%s has unsupported version %s (expected %d): not overwritten"):format(
					path,
					tostring(doc.version),
					VERSION
				),
				"bytes:" .. text
		end
		if doc.rules ~= nil then
			if not mare.json.is_array(doc.rules) or not lib.is_dense_list(doc.rules) then
				return nil, path .. ": rules must be a dense list", "bytes:" .. text
			end
		end
		return doc, nil, "bytes:" .. text
	end

	local function adopt(doc, st)
		S.rules = type(doc.rules) == "table" and doc.rules or {}
		S.stamp = st
		S.error = nil
	end

	--- Re-read when another writer changed it. Invalid replacements close the rule source;
	--- retaining its old rules could hide a newly added denial.
	--- (reading needs a coroutine: from plain code the cached rules stay)
	function S.refresh()
		local ok, doc, err, st = pcall(load)
		if not ok then
			st = "exception:" .. tostring(doc)
		end
		if st == S.stamp then
			return
		end
		-- A newly observed shared rule file invalidates in-flight permission approvals.
		if S.stamp ~= nil then
			bump_policy_revision()
		end
		if ok and doc then
			adopt(doc, st)
		else
			S.rules, S.stamp = {}, st
			S.error = tostring(ok and err or doc)
		end
	end

	--- the current rules (as of the last refresh)
	function S.list()
		return S.rules
	end

	--- add `rule` under the lock: re-read, merge, write atomically. -> true | nil, err
	function S.add(rule)
		local okg, guard = pcall(mare.lock.file, path .. ".lock", { timeout_ms = 5000 })
		if not okg then
			return nil, tostring(guard)
		end
		local ok, res, err = pcall(function()
			local doc, e = load()
			if not doc then
				return nil, e
			end
			local rules = type(doc.rules) == "table" and doc.rules or {}
			local dup = false
			for _, r in ipairs(rules) do
				dup = dup or same(r, rule)
			end
			if not dup then
				rules[#rules + 1] = rule
			end
			doc.rules = mare.json.array(rules)
			doc.version = VERSION
			local text = mare.json.encode(doc, { pretty = true })
			local w, we = mare.fs.write(path, text, { atomic = true, mode = 384 })
			if not w then
				return nil, we
			end
			adopt(doc, "bytes:" .. text)
			return true
		end)
		guard:release()
		if not ok then
			return nil, tostring(res)
		end
		return res, err
	end

	S.refresh()
	return S
end
