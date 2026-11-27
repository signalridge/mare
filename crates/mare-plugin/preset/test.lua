-- mare.test: public spec organization and assertion helpers.
-- Available on `mare.test` and via `require("mare.test")`.
-- Returns (public_api, run_spec); the host stores `run_spec` in the Lua registry
-- so normal runtime callers only see the pure public functions.

local snapshot_io, _, create_test_host = ...
local active_suite = nil
local active_spec = nil
local active_test = nil

local function format_value(v, depth, seen)
	depth = depth or 0
	local t = type(v)
	if t == "string" then
		return string.format("%q", v)
	elseif t ~= "table" then
		return tostring(v)
	end
	if seen and seen[v] then
		return "<cycle>"
	end
	if depth >= 4 then
		return "{...}"
	end
	seen = seen or {}
	seen[v] = true
	local parts = {}
	local n = #v
	local used = {}
	for i = 1, n do
		used[i] = true
		parts[#parts + 1] = format_value(v[i], depth + 1, seen)
	end
	local keys = {}
	for k in pairs(v) do
		if not used[k] then
			keys[#keys + 1] = k
		end
	end
	table.sort(keys, function(a, b)
		return tostring(a) < tostring(b)
	end)
	for _, k in ipairs(keys) do
		local ks = (type(k) == "string" and k:match("^[%a_][%w_]*$")) and k
			or ("[" .. format_value(k, depth + 1, seen) .. "]")
		parts[#parts + 1] = ks .. " = " .. format_value(v[k], depth + 1, seen)
	end
	seen[v] = nil
	return "{ " .. table.concat(parts, ", ") .. " }"
end

local function is_json_array(t)
	return mare ~= nil
		and type(mare.json) == "table"
		and type(mare.json.is_array) == "function"
		and mare.json.is_array(t)
end

local function deep_eq(a, b, seen)
	if type(a) ~= "table" or type(b) ~= "table" then
		return a == b
	end
	if rawequal(a, b) then
		return true
	end
	if next(a) == nil and next(b) == nil then
		return is_json_array(a) == is_json_array(b)
	end
	seen = seen or {}
	local pair = seen[a]
	if pair and pair[b] then
		return true
	end
	seen[a] = seen[a] or {}
	seen[a][b] = true
	for k, va in next, a do
		local vb = rawget(b, k)
		if vb == nil or not deep_eq(va, vb, seen) then
			return false
		end
	end
	for k in next, b do
		if rawget(a, k) == nil then
			return false
		end
	end
	return true
end

local function fail_prefix(msg)
	if msg ~= nil and msg ~= "" then
		return tostring(msg) .. ": "
	end
	return ""
end

local expect = {}

function expect.eq(actual, expected, msg)
	if not deep_eq(actual, expected) then
		error(fail_prefix(msg) .. "expected " .. format_value(expected) .. ", got " .. format_value(actual), 2)
	end
end

function expect.ne(actual, unexpected, msg)
	if deep_eq(actual, unexpected) then
		error(fail_prefix(msg) .. "expected value not equal to " .. format_value(unexpected), 2)
	end
end

function expect.truthy(value, msg)
	if not value then
		error(fail_prefix(msg) .. "expected truthy value, got " .. format_value(value), 2)
	end
end

function expect.falsy(value, msg)
	if value then
		error(fail_prefix(msg) .. "expected falsy value, got " .. format_value(value), 2)
	end
end

function expect.error(fn, pattern, msg)
	if type(fn) ~= "function" then
		error("expect.error(fn, pattern?, msg?): fn must be a function", 2)
	end
	local ok, err = pcall(fn)
	if ok then
		error(fail_prefix(msg) .. "expected function to error, but it succeeded", 2)
	end
	if pattern ~= nil then
		if type(pattern) ~= "string" then
			error("expect.error(fn, pattern?, msg?): pattern must be a string", 2)
		end
		local s = tostring(err)
		if not string.find(s, pattern) then
			error(
				fail_prefix(msg) .. "expected error matching " .. format_value(pattern) .. ", got " .. format_value(s),
				2
			)
		end
	end
end

function expect.match(str, pattern, msg)
	if type(str) ~= "string" then
		error(fail_prefix(msg) .. "expected string, got " .. type(str), 2)
	end
	if type(pattern) ~= "string" then
		error("expect.match(str, pattern, msg?): pattern must be a string", 2)
	end
	if not string.find(str, pattern) then
		error(fail_prefix(msg) .. "expected " .. format_value(str) .. " to match " .. format_value(pattern), 2)
	end
end

local function serialize_snapshot_value(v, indent, seen)
	local t = type(v)
	if t == "string" then
		return string.format("%q", v)
	elseif t ~= "table" then
		return tostring(v)
	end
	if seen[v] then
		return "<cycle>"
	end
	if next(v) == nil then
		return is_json_array(v) and "[]" or "{}"
	end
	seen[v] = true
	local pad = string.rep("  ", indent)
	local child_pad = string.rep("  ", indent + 1)
	local lines = { "{" }
	local n = #v
	local used = {}
	for i = 1, n do
		used[i] = true
		lines[#lines + 1] = child_pad .. serialize_snapshot_value(v[i], indent + 1, seen) .. ","
	end
	local keys = {}
	for k in pairs(v) do
		if not used[k] then
			keys[#keys + 1] = k
		end
	end
	table.sort(keys, function(a, b)
		local ta, tb = type(a), type(b)
		if ta ~= tb then
			return ta < tb
		end
		return tostring(a) < tostring(b)
	end)
	for _, k in ipairs(keys) do
		local ks = (type(k) == "string" and k:match("^[%a_][%w_]*$")) and k
			or ("[" .. serialize_snapshot_value(k, indent + 1, seen) .. "]")
		lines[#lines + 1] = child_pad .. ks .. " = " .. serialize_snapshot_value(v[k], indent + 1, seen) .. ","
	end
	lines[#lines + 1] = pad .. "}"
	seen[v] = nil
	return table.concat(lines, "\n")
end

local function serialize_snapshot(v)
	if type(v) == "string" then
		return v
	end
	return serialize_snapshot_value(v, 0, {})
end

local function split_lines(s)
	local lines = {}
	local start = 1
	while true do
		local nl = string.find(s, "\n", start, true)
		if not nl then
			lines[#lines + 1] = string.sub(s, start)
			break
		end
		lines[#lines + 1] = string.sub(s, start, nl - 1)
		start = nl + 1
	end
	return lines
end

local function line_diff(expected, actual)
	local a = split_lines(expected)
	local b = split_lines(actual)
	local na, nb = #a, #b
	if na * nb > 250000 then
		local out = {}
		local max_n = na > nb and na or nb
		for i = 1, max_n do
			if a[i] ~= b[i] then
				if a[i] ~= nil then
					out[#out + 1] = "- " .. a[i]
				end
				if b[i] ~= nil then
					out[#out + 1] = "+ " .. b[i]
				end
			end
		end
		return table.concat(out, "\n")
	end

	local dp = {}
	for i = 0, na do
		dp[i] = {}
		dp[i][0] = 0
	end
	for j = 0, nb do
		dp[0][j] = 0
	end
	for i = 1, na do
		for j = 1, nb do
			if a[i] == b[j] then
				dp[i][j] = dp[i - 1][j - 1] + 1
			else
				local up = dp[i - 1][j]
				local left = dp[i][j - 1]
				dp[i][j] = up >= left and up or left
			end
		end
	end

	local ops = {}
	local i, j = na, nb
	while i > 0 or j > 0 do
		if i > 0 and j > 0 and a[i] == b[j] then
			ops[#ops + 1] = "  " .. a[i]
			i = i - 1
			j = j - 1
		elseif j > 0 and (i == 0 or dp[i][j - 1] >= dp[i - 1][j]) then
			ops[#ops + 1] = "+ " .. b[j]
			j = j - 1
		else
			ops[#ops + 1] = "- " .. a[i]
			i = i - 1
		end
	end

	local out = {}
	for k = #ops, 1, -1 do
		out[#out + 1] = ops[k]
	end
	return table.concat(out, "\n")
end

local function fnv1a_hex(s)
	local h = 2166136261
	for i = 1, #s do
		h = ((h ~ string.byte(s, i)) * 16777619) & 0xffffffff
	end
	return string.format("%08x", h)
end

local function sanitize_snapshot_name(name)
	local safe = name:gsub("[^%w%-_]+", "_"):gsub("^_+", ""):gsub("_+$", "")
	if safe == "" then
		safe = "snapshot"
	end
	if safe ~= name then
		return safe .. "_" .. fnv1a_hex(name)
	end
	return safe
end

local function split_spec_path(path)
	local normalized = path:gsub("\\", "/")
	local dir, file = normalized:match("^(.*)/([^/]+)$")
	if not dir then
		return "", normalized
	end
	return dir, file
end

local function parse_snapshot_body(raw)
	if raw == nil then
		return nil
	end
	local body = raw:match("^%-%-%-\n.-\n%-%-%-\n(.*)$")
	if body ~= nil then
		return body
	end
	return raw
end

function expect.snapshot(value, name)
	if not active_test then
		error("expect.snapshot(value, name?) can only be called inside it()", 2)
	end
	if not active_spec or type(active_spec.spec_path) ~= "string" or active_spec.spec_path == "" then
		error("expect.snapshot(value, name?) requires a file-backed spec", 2)
	end
	if not snapshot_io then
		error("expect.snapshot(value, name?) is unavailable in this runtime", 2)
	end

	active_test.snapshot_index = active_test.snapshot_index + 1
	local snap_name = name
	if snap_name ~= nil then
		if type(snap_name) ~= "string" or snap_name == "" then
			error("expect.snapshot(value, name?): name must be a non-empty string", 2)
		end
	else
		snap_name = active_test.full_name
		if active_test.snapshot_index > 1 then
			snap_name = snap_name .. " #" .. tostring(active_test.snapshot_index)
		end
	end

	local spec_dir, spec_file = split_spec_path(active_spec.spec_path)
	local spec_stem = spec_file:gsub("%.lua$", "")
	local safe_name = sanitize_snapshot_name(snap_name)
	local snap_dir = spec_dir ~= "" and (spec_dir .. "/snapshots") or "snapshots"
	local snap_path = snap_dir .. "/" .. spec_stem .. "__" .. safe_name .. ".snap"
	local pending_path = snap_path .. ".new"

	active_spec.used_snapshots = active_spec.used_snapshots or {}
	if active_spec.used_snapshots[snap_path] ~= nil then
		error(
			"snapshot name collision: "
				.. snap_path
				.. " already used by "
				.. tostring(active_spec.used_snapshots[snap_path]),
			2
		)
	end
	active_spec.used_snapshots[snap_path] = snap_name

	local actual_body = serialize_snapshot(value)
	local existing_raw = snapshot_io.read_file(snap_path)
	local expected_body = parse_snapshot_body(existing_raw)

	if existing_raw ~= nil and expected_body == actual_body then
		snapshot_io.remove_file(pending_path)
		return
	end

	local mode = snapshot_io.update_mode(active_spec.update_snapshots == true)
	local full_content = "---\nsource: " .. spec_file .. "\nname: " .. snap_name .. "\n---\n" .. actual_body

	if mode == "always" or (mode == "unseen" and existing_raw == nil) then
		local ok, err = snapshot_io.write_file(snap_path, full_content)
		if not ok then
			error("failed to write snapshot " .. snap_path .. ": " .. tostring(err), 2)
		end
		snapshot_io.remove_file(pending_path)
		return
	end

	local pending_hint = ""
	if mode == "new" or mode == "unseen" then
		local ok, err = snapshot_io.write_file(pending_path, full_content)
		if ok then
			pending_hint = "\nwrote pending snapshot: "
				.. pending_path
				.. "\nrun with --update-snapshots or MARE_UPDATE_SNAPSHOTS=always to accept"
		else
			pending_hint = "\n(failed to write pending snapshot " .. pending_path .. ": " .. tostring(err) .. ")"
		end
	end

	if existing_raw == nil then
		error("snapshot missing: " .. snap_path .. pending_hint, 2)
	end
	error("snapshot mismatch (" .. snap_path .. "):\n" .. line_diff(expected_body, actual_body) .. pending_hint, 2)
end

function expect.settled(host, msg)
	if type(host) ~= "table" or type(host._settled_problems) ~= "function" then
		error(fail_prefix(msg) .. "expect.settled(host): expected a mare.TestHost from mare.test.host()", 2)
	end
	local problems = host:_settled_problems()
	if #problems > 0 then
		error(fail_prefix(msg) .. "test host is not settled:\n  - " .. table.concat(problems, "\n  - "), 2)
	end
end

local function host(opts)
	if not active_spec then
		error("mare.test.host() can only be called while running a spec", 2)
	end
	if type(create_test_host) ~= "function" then
		error("mare.test.host() is not available in this runtime", 2)
	end
	local instance = create_test_host(opts)
	local bucket = active_test or active_spec
	bucket.hosts = bucket.hosts or {}
	bucket.hosts[#bucket.hosts + 1] = instance
	return instance
end

local function dispose_hosts(bucket)
	if not bucket or not bucket.hosts then
		return
	end
	local list = bucket.hosts
	bucket.hosts = nil
	for i = #list, 1, -1 do
		pcall(function()
			list[i]:dispose()
		end)
	end
end

local function require_active(api_name)
	if not active_suite then
		error("mare.test." .. api_name .. "() can only be called while running a spec", 3)
	end
	return active_suite
end

local function describe(name, fn)
	if type(name) ~= "string" then
		error("mare.test.describe(name, fn): name must be a string", 2)
	end
	if type(fn) ~= "function" then
		error("mare.test.describe(name, fn): fn must be a function", 2)
	end
	local parent = require_active("describe")
	local child = {
		name = name,
		tests = {},
		children = {},
		before_each = {},
		after_each = {},
	}
	parent.children[#parent.children + 1] = child
	active_suite = child
	local ok, err = pcall(fn)
	active_suite = parent
	if not ok then
		error(err, 0)
	end
end

local function it(name, fn)
	if type(name) ~= "string" then
		error("mare.test.it(name, fn): name must be a string", 2)
	end
	if type(fn) ~= "function" then
		error("mare.test.it(name, fn): fn must be a function", 2)
	end
	local suite = require_active("it")
	suite.tests[#suite.tests + 1] = { name = name, fn = fn }
end

local function before_each(fn)
	if type(fn) ~= "function" then
		error("mare.test.before_each(fn): fn must be a function", 2)
	end
	local suite = require_active("before_each")
	suite.before_each[#suite.before_each + 1] = fn
end

local function after_each(fn)
	if type(fn) ~= "function" then
		error("mare.test.after_each(fn): fn must be a function", 2)
	end
	local suite = require_active("after_each")
	suite.after_each[#suite.after_each + 1] = fn
end

local function traceback_handler(err)
	if debug and type(debug.traceback) == "function" then
		return debug.traceback(tostring(err), 2)
	end
	return tostring(err)
end

local function run_suite(suite, prefix, inherited_before, inherited_after, results)
	local befores = {}
	for _, fn in ipairs(inherited_before) do
		befores[#befores + 1] = fn
	end
	for _, fn in ipairs(suite.before_each) do
		befores[#befores + 1] = fn
	end

	local afters = {}
	for _, fn in ipairs(suite.after_each) do
		afters[#afters + 1] = fn
	end
	for _, fn in ipairs(inherited_after) do
		afters[#afters + 1] = fn
	end

	local suite_prefix = prefix == "" and suite.name or (suite.name == "" and prefix or (prefix .. " > " .. suite.name))

	for _, test_case in ipairs(suite.tests) do
		local full_name = suite_prefix == "" and test_case.name or (suite_prefix .. " > " .. test_case.name)
		local filter = active_spec and active_spec.filter
		if filter ~= nil and filter ~= "" and not string.find(full_name, filter, 1, true) then
			results.filtered = results.filtered + 1
		else
			results.total = results.total + 1
			local prev_test = active_test
			active_test = { full_name = full_name, snapshot_index = 0 }
			local ok = true
			local err_msg = nil

			for _, hook in ipairs(befores) do
				local h_ok, h_err = xpcall(hook, traceback_handler)
				if not h_ok then
					ok = false
					err_msg = h_err
					break
				end
			end

			if ok then
				local t_ok, t_err = xpcall(test_case.fn, traceback_handler)
				if not t_ok then
					ok = false
					err_msg = t_err
				end
			end

			for _, hook in ipairs(afters) do
				local a_ok, a_err = xpcall(hook, traceback_handler)
				if not a_ok then
					if ok then
						ok = false
						err_msg = a_err
					else
						err_msg = tostring(err_msg) .. "\n(after_each error: " .. tostring(a_err) .. ")"
					end
				end
			end

			dispose_hosts(active_test)
			active_test = prev_test

			if ok then
				results.passed = results.passed + 1
			else
				results.failures[#results.failures + 1] = {
					name = full_name,
					error = tostring(err_msg),
				}
			end
		end
	end

	for _, child in ipairs(suite.children) do
		run_suite(child, suite_prefix, befores, afters, results)
	end
end

local function run_spec(chunk_fn, opts)
	local root = {
		name = "",
		tests = {},
		children = {},
		before_each = {},
		after_each = {},
	}
	local prev = active_suite
	local prev_spec = active_spec
	active_suite = root
	active_spec = opts or {}
	local load_ok, load_err = xpcall(chunk_fn, traceback_handler)
	active_suite = prev

	if not load_ok then
		active_spec = prev_spec
		return {
			total = 0,
			passed = 0,
			filtered = 0,
			load_error = true,
			failures = {
				{
					name = "<spec>",
					error = tostring(load_err),
				},
			},
		}
	end

	local results = {
		total = 0,
		passed = 0,
		filtered = 0,
		load_error = false,
		failures = {},
	}
	run_suite(root, "", {}, {}, results)
	dispose_hosts(active_spec)
	active_spec = prev_spec
	return results
end

local M = {
	describe = describe,
	it = it,
	before_each = before_each,
	after_each = after_each,
	expect = expect,
	host = host,
}

return M, run_spec
