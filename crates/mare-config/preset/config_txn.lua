-- One runtime config write = one transaction, run by the host's config writer one at a time.
-- A job is
-- `{ ops = { { path, kind = "set", patch, dotted } | { path, kind = "clear" } }, at }`.
--
--   0 read each owner's live value (`option/current`)                  may yield
--   1 stage: candidate committed and effective values, changed leaves  no shared write
--   2 owner apply per changed leaf (`option/apply`, checked)            may yield; shared state old
--   3 publish: committed value, sources, runtime base, kernel copy,    no yield
--     running `cfg[k]` = effective value
--   4 settle: an owner whose live value drifted during 2 gets the leaves again
--   5 announce the changed leaves (the notifier emits `option/changed`, then answers)
-- A refusal in 2 (or a stale config at 3) publishes nothing: owners that were offered a leaf are
-- brought back to what they had; a leaf that cannot be is listed as unconfirmed.
--
-- Rules: snapshots are `{ present, value }`; option values never go through `x and y or z`.
local mare, core = ...
local cfg = core.config

local function S()
	return cfg.state
end
local function I()
	return S().internal
end
local function copy(v)
	return S().deepcopy(v)
end
local function snap(v)
	return { present = v ~= nil, value = copy(v) }
end
local function is_list(t)
	return type(t) == "table" and #t > 0
end
local function equal(a, b)
	if type(a) ~= type(b) then
		return false
	end
	if type(a) ~= "table" then
		return a == b
	end
	for k, v in pairs(a) do
		if not equal(v, b[k]) then
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

local function split(path)
	return path:match("^plugins%.([^.]+)%.([^.]+)$")
end
local function entry(p, k)
	local schema = (S().plugin_schemas or {})[p]
	if schema == nil then
		return nil
	end
	return schema[k]
end
-- a union or a list is one value: replaced, compared and applied as one leaf
local function whole(p, k)
	local e = entry(p, k)
	return e ~= nil and (e.type == "union" or e.type == "list")
end
local function live(p, k)
	local e = entry(p, k)
	return e ~= nil and (e.apply == "live" or e.apply == "next_op")
end
local function effective(p, k, s)
	return S().plugin_effective(p, k, s.present, s.value)
end

-- `src` merged into `dst` by key (tables that are not lists recurse; anything else replaces)
local function merge(dst, src)
	for k, v in pairs(src) do
		if type(v) == "table" and not is_list(v) and type(dst[k]) == "table" and not is_list(dst[k]) then
			merge(dst[k], v)
		else
			dst[k] = copy(v)
		end
	end
	return dst
end

-- the leaves where `old` and `new` differ, as { path, old = snap, new = snap }, sorted by path
local function diff(old, new, path, one, out)
	-- an object on either side is compared leaf by leaf (a side that is absent counts as empty),
	-- so a new or removed entry is announced by its leaves
	local function object(v)
		return type(v) == "table" and not is_list(v) and next(v) ~= nil
	end
	local function object_or_absent(v)
		return v == nil or (type(v) == "table" and not is_list(v))
	end
	local tables = (object(old) or object(new)) and object_or_absent(old) and object_or_absent(new)
	if tables then
		old = old or {}
		new = new or {}
	end
	if one or not tables then
		if not equal(old, new) then
			out[#out + 1] = { path = path, old = snap(old), new = snap(new) }
		end
		return out
	end
	local keys, seen = {}, {}
	for k in pairs(old) do
		seen[k] = true
		keys[#keys + 1] = k
	end
	for k in pairs(new) do
		if not seen[k] then
			keys[#keys + 1] = k
		end
	end
	table.sort(keys, function(a, b)
		return tostring(a) < tostring(b)
	end)
	for _, k in ipairs(keys) do
		diff(old[k], new[k], path .. "." .. tostring(k), false, out)
	end
	return out
end

-- what the first runtime write of an option replaced: `mare.config_clear` puts it back
local state = S()
state.runtime_base = state.runtime_base or {}
function state.remember_base(path, existed, value, sources)
	local s = S()
	s.runtime_base = s.runtime_base or {}
	if s.runtime_base[path] == nil then
		s.runtime_base[path] = { existed = existed, value = copy(value), sources = sources }
	end
end

local function fail(msg, info, notices)
	return { ok = false, msg = tostring(msg), info = info, notices = notices }
end
local STALE = "configuration changed while this write was being applied; retry"

--- One transaction (called by the config writer, in its transaction scope).
function state.config_run(job)
	local s = S()
	local C = I().C
	local ops = {}
	-- 0: the owners' live values
	for i, op in ipairs(job.ops or {}) do
		local p, k = split(op.path)
		if not p then
			return fail(("not a plugin option: %s"):format(tostring(op.path)))
		end
		ops[i] = { op = op, p = p, k = k, path = op.path, live_pre = s.option_current(op.path) }
	end
	-- 1: stage (nothing shared is written)
	local candidate = {}
	for _, o in ipairs(ops) do
		if not candidate[o.p] then
			candidate[o.p] = copy((C.plugins or {})[o.p] or {})
		end
		local own = (C.plugins or {})[o.p] or {}
		o.old_c = snap(own[o.k])
		o.old_eff = snap(effective(o.p, o.k, o.old_c))
		o.one = whole(o.p, o.k)
		if o.op.kind == "clear" then
			o.base = s.runtime_base[o.path]
			if o.base == nil then
				o.new_c = o.old_c -- nothing was written at runtime
			elseif o.base.existed then
				o.new_c = { present = true, value = copy(o.base.value) }
			else
				o.new_c = { present = false }
			end
			if o.new_c.present then
				candidate[o.p][o.k] = copy(o.new_c.value)
			else
				candidate[o.p][o.k] = nil
			end
		else
			local patch = o.op.patch
			local value
			if o.one then
				if o.op.dotted then
					-- a path below a union or a list changes the owner's real value, then replaces
					local under = o.live_pre
					if under == nil then
						under = o.old_eff.value
					end
					if type(under) == "table" then
						value = merge(copy(under), patch)
					else
						value = copy(patch)
					end
				else
					value = copy(patch)
				end
			elseif type(patch) == "table" and not is_list(patch) and type(o.old_c.value) == "table" then
				-- an object or a map: by key into the committed value (a dotted write is a sparse
				-- patch, so untouched siblings are never copied into the runtime layer)
				value = merge(copy(o.old_c.value), patch)
			else
				value = copy(patch)
			end
			o.new_c = { present = true, value = value }
			candidate[o.p] = candidate[o.p] or {}
			candidate[o.p][o.k] = copy(value)
		end
		o.new_eff = snap(effective(o.p, o.k, o.new_c))
		-- what the owner saw before: its live value when it reports one
		local base = o.live_pre
		if base == nil then
			base = o.old_eff.value
		end
		o.changes = diff(base, o.new_eff.value, o.path, o.one, {})
		-- the conversion the publish point does for the kernel copy, done here so it cannot fail there
		local ok, err = s.config_convertible(o.new_c.value)
		if not ok then
			return fail(("%s: %s"):format(o.path, tostring(err)))
		end
	end
	if next(candidate) ~= nil then
		local problems = s.validate_options({ plugins = candidate })
		if #problems > 0 then
			return fail(table.concat(problems, "; "))
		end
	end
	-- 2: owners apply, each leaf marked attempted before it is offered
	local attempted, failure = {}, nil
	for _, o in ipairs(ops) do
		for _, leaf in ipairs(o.changes) do
			attempted[#attempted + 1] = { o = o, leaf = leaf }
			local ok, err = pcall(s.option_apply, leaf.path, copy(leaf.new.value))
			if not ok then
				failure = err
				break
			end
		end
		if failure then
			break
		end
	end
	-- 3: stale check, then publish without yielding
	if not failure then
		if I().C ~= C then
			failure = STALE
		else
			for _, o in ipairs(ops) do
				if not equal(((C.plugins or {})[o.p] or {})[o.k], o.old_c.value) then
					failure = STALE
					break
				end
			end
		end
	end
	if failure then
		-- abort: bring back every owner that was offered a leaf, in reverse; an owner that
		-- reports its live value and still has it is left alone
		local unconfirmed, checked, unchanged = {}, {}, {}
		for n = #attempted, 1, -1 do
			local a = attempted[n]
			local o = a.o
			if o.live_pre ~= nil and not checked[o] then
				checked[o] = true
				unchanged[o] = equal(s.option_current(o.path), o.live_pre)
			end
			if not unchanged[o] then
				if not pcall(s.option_apply, a.leaf.path, copy(a.leaf.old.value), true) then
					table.insert(unconfirmed, 1, a.leaf.path)
				end
			end
		end
		return fail(failure, { unconfirmed = unconfirmed })
	end
	C.plugins = C.plugins or {}
	for _, o in ipairs(ops) do
		C.plugins[o.p] = C.plugins[o.p] or {}
		local own = C.plugins[o.p]
		local old_sources = I().sources_at(o.path)
		if o.new_c.present then
			own[o.k] = copy(o.new_c.value)
		else
			own[o.k] = nil
		end
		I().clear_sources(o.path)
		if o.op.kind == "clear" then
			if o.base then
				for sk, src in pairs(o.base.sources or {}) do
					s.sources[sk] = src
				end
			else
				for sk, src in pairs(old_sources) do
					s.sources[sk] = src
				end
			end
			s.runtime_base[o.path] = nil
		else
			I().record_leaves(o.path, o.new_c.value, s.layer_runtime, job.at)
			s.remember_base(o.path, o.old_c.present, o.old_c.value, old_sources)
		end
		if o.new_c.present then
			s.plugin_kernel_sync(o.p, o.k, o.new_c.value)
		else
			s.plugin_kernel_sync(o.p, o.k, nil)
		end
		-- fetched now: a reload may have replaced the plugin instance since staging
		local running = s.plugin_running(o.p)
		if running then
			running[o.k] = copy(o.new_eff.value)
		end
	end
	-- 4: settle owners whose live value drifted while they applied (e.g. a session reset them)
	local unconfirmed = {}
	for _, o in ipairs(ops) do
		if o.live_pre ~= nil and #o.changes > 0 and not equal(s.option_current(o.path), o.new_eff.value) then
			for _, leaf in ipairs(o.changes) do
				if not pcall(s.option_apply, leaf.path, copy(leaf.new.value)) then
					unconfirmed[#unconfirmed + 1] = leaf.path
				end
			end
		end
	end
	-- 5: announce the changed leaves; reload a plugin with an option that is not applied live
	local notices, reload, seen = {}, {}, {}
	for _, o in ipairs(ops) do
		for _, leaf in ipairs(o.changes) do
			-- announced redacted (the one redactor); the kernel copy and running cfg keep real values
			local safe, redacted = mare.config_redact(leaf.path, copy(leaf.new.value))
			notices[#notices + 1] = { name = leaf.path, value = safe, redacted = redacted or nil }
		end
		if not live(o.p, o.k) and not seen[o.p] then
			seen[o.p] = true
			reload[#reload + 1] = o.p
		end
	end
	if #unconfirmed > 0 then
		return {
			ok = false,
			msg = "committed, but the owner did not confirm " .. table.concat(unconfirmed, ", "),
			info = { committed = true, unconfirmed = unconfirmed },
			notices = notices,
			reload = reload,
		}
	end
	return { ok = true, notices = notices, reload = reload }
end

-- the caller's error for a failed answer
local function raise(msg, info)
	local text = tostring(msg):gsub("\nstack traceback:.*", "")
	if type(info) == "table" and type(info.unconfirmed) == "table" and #info.unconfirmed > 0 then
		if not info.committed then
			text = text .. "; owner state unconfirmed: " .. table.concat(info.unconfirmed, ", ")
		end
	end
	error(text, 0)
end

--- A runtime write of `batch = { [plugin] = { [key] = value } }`; `dotted[plugin][key]` marks a
--- key written below its top (RPC `plugins.p.k.leaf`: the value is then the sparse patch).
function state.config_write(batch, dotted, at)
	local plugins = {}
	for p in pairs(batch) do
		plugins[#plugins + 1] = p
	end
	table.sort(plugins)
	local ops = {}
	for _, p in ipairs(plugins) do
		local keys = {}
		for k in pairs(batch[p]) do
			keys[#keys + 1] = k
		end
		table.sort(keys)
		for _, k in ipairs(keys) do
			ops[#ops + 1] = {
				path = "plugins." .. p .. "." .. k,
				kind = "set",
				patch = copy(batch[p][k]),
				dotted = dotted ~= nil and dotted[p] ~= nil and dotted[p][k] == true,
			}
		end
	end
	local ok, msg, info = S().config_submit({ ops = ops, at = at })
	if not ok then
		raise(msg, info)
	end
end

--- `mare.config_clear(path)` at runtime for a plugin option; false when `path` is not one.
function state.config_clear_runtime(path)
	if not split(path) then
		return false
	end
	local ok, msg, info = S().config_submit({ ops = { { path = path, kind = "clear" } } })
	if not ok then
		raise(msg, info)
	end
	return true
end
