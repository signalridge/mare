-- Storage mechanisms shared by result policies, renderers and other producers.
-- Backends own their opaque spill_ids and must enforce the supplied session owner.
local PREFIX = "spill:v1:"
local MAX_SPILL_BYTES = 8 * 1024 * 1024
local MAX_SPILL_BASE64 = 11184812
local MAX_TEXT_LIMIT = 2000

return {
	name = "spill",
	provide = { "spill" },
	inject = { "bus" },
	config_schema = {
		root = { type = "string", desc = "Directory of the local spill store." },
		backend = { type = "string", default = "local", desc = "Name of the spill store backend." },
		cleanup_period_days = { type = "integer", default = 30, desc = "Days stored spills are kept." },
	},
	apply = function(ctx, cfg)
		local lib = require("mare_lib")
		local backends, service = {}, {}
		local local_store = mare.spill.new_local({ root = cfg.root, cleanup_period_days = cfg.cleanup_period_days })
		backends["local"] = local_store
		ctx:effect(function()
			return function()
				local_store.dispose()
			end
		end, "spill local store")
		local function changed(name, kind)
			ctx.bus:emit("spill/backends-changed", { name = name, kind = kind })
		end
		local function backend(name)
			if type(name) ~= "string" or not backends[name] then
				lib.fail("spill.unknown_backend", "unknown spill backend: " .. tostring(name))
			end
			return backends[name]
		end
		local function unpack_spill_id(spill_id)
			if type(spill_id) ~= "string" or #spill_id > 65536 or spill_id:sub(1, #PREFIX) ~= PREFIX then
				lib.fail("invalid_arguments", "invalid spill locator")
			end
			local name, namespace, encoded =
				spill_id:sub(#PREFIX + 1):match("^([a-z][a-z0-9_-]*):([A-Za-z0-9_-]+):([A-Za-z0-9_-]+)$")
			if not name then
				lib.fail("invalid_arguments", "invalid spill locator")
			end
			local identity = mare.crypto.base64url_decode(namespace)
			if identity == nil or mare.crypto.base64url_encode(identity) ~= namespace then
				lib.fail("invalid_arguments", "noncanonical spill namespace")
			end
			if backend(name).identity ~= identity then
				lib.fail("spill.unknown_spill", "spill backend namespace changed: " .. name)
			end
			local raw = mare.crypto.base64url_decode(encoded)
			if raw == nil or mare.crypto.base64url_encode(raw) ~= encoded then
				lib.fail("invalid_arguments", "noncanonical spill locator")
			end
			return name, raw
		end
		local function active_session_id(missing_code)
			local session = ctx:get("session")
			local info = session and session:info()
			if type(info) ~= "table" or type(info.session_id) ~= "string" or info.session_id == "" then
				lib.fail(missing_code or "spill.unknown_spill", "spill requires an active session")
			end
			return info.session_id
		end
		local function strip_backend(opts)
			if opts == nil then
				return nil
			end
			local copy, has = {}, false
			for key, value in pairs(opts) do
				if key ~= "backend" then
					copy[key] = value
					has = true
				end
			end
			return has and copy or nil
		end
		local function save_for(method, owner, content, opts)
			assert(type(owner) == "string" and owner ~= "", "spill requires a session owner")
			local name = opts and opts.backend or cfg.backend
			local implementation = backend(name)
			local ref = implementation[method](owner, content, strip_backend(opts))
			local ok, failure = pcall(function()
				assert(
					type(ref) == "table"
						and type(ref.spill_id) == "string"
						and ref.spill_id ~= ""
						and #ref.spill_id <= 32768,
					"spill backend returned an invalid reference"
				)
				assert(
					type(ref.size) == "number" and ref.size >= 0 and ref.size <= 9007199254740991 and ref.size % 1 == 0,
					"spill backend returned an invalid size"
				)
			end)
			if not ok then
				if type(ref) == "table" and type(ref.spill_id) == "string" then
					pcall(implementation.delete, ref.spill_id, { session_id = owner })
				end
				error(failure, 0)
			end
			local result = {}
			for key, value in pairs(ref) do
				if key ~= "spill_id" then
					result[key] = value
				end
			end
			result.spill_id = PREFIX
				.. name
				.. ":"
				.. mare.crypto.base64url_encode(implementation.identity)
				.. ":"
				.. mare.crypto.base64url_encode(ref.spill_id)
			result.backend = name
			return result
		end
		local function is_raw_spill_id(spill_id)
			return type(spill_id) == "string"
				and #spill_id > 0
				and #spill_id <= 65536
				and not spill_id:match("^spill:")
				and spill_id:match("^[^:]+:.+$") ~= nil
		end
		local function classify_read_error(err)
			if type(err) == "table" and type(err.code) == "string" then
				error(err, 0)
			end
			local msg = tostring(err)
			local lower = msg:lower()
			if
				lower:find("cursor exceeds", 1, true)
				or lower:find("limit must be", 1, true)
				or lower:find("offset >= 1", 1, true)
				or lower:find("utf-8", 1, true)
				or lower:find("utf8", 1, true)
			then
				lib.fail("invalid_arguments", msg)
			end
			if
				lower:find("sandbox:", 1, true)
				or lower:find("deny_read", 1, true)
				or lower:find("access denied", 1, true)
				or lower:find("permission denied", 1, true)
			then
				lib.fail("denied", msg)
			end
			lib.fail("spill.unknown_spill", msg)
		end
		local function read_for(method, owner, spill_id, opts)
			local backend_opts = { session_id = owner }
			if opts and opts.offset ~= nil then
				backend_opts.cursor = opts.offset
			end
			if opts and opts.limit ~= nil then
				backend_opts.limit = opts.limit
			end
			local ok, res
			if type(spill_id) == "string" and spill_id:sub(1, #PREFIX) ~= PREFIX then
				if not is_raw_spill_id(spill_id) then
					lib.fail("invalid_arguments", "invalid spill locator")
				end
				ok, res = pcall(mare.spill[method], spill_id, backend_opts)
			else
				local name, raw = unpack_spill_id(spill_id)
				ok, res = pcall(backend(name)[method], raw, backend_opts)
			end
			if not ok then
				classify_read_error(res)
			end
			local next_offset = res.more and res.next_byte or nil
			if method == "read_bytes" then
				return { bytes = res.bytes, next_offset = next_offset, size = res.size }
			end
			return { content = res.content, next_offset = next_offset }
		end
		local function delete_for(owner, spill_id)
			local name, raw = unpack_spill_id(spill_id)
			local ok, removed = pcall(backend(name).delete, raw, { session_id = owner })
			if not ok then
				if type(removed) == "table" and type(removed.code) == "string" then
					error(removed, 0)
				end
				lib.fail("spill.unknown_spill", tostring(removed))
			end
			return removed ~= false
		end
		local function bounded_int(v, name, min, max)
			if v == nil then
				return
			end
			if type(v) ~= "number" or v % 1 ~= 0 or v < min or (max and v > max) then
				local msg = max and string.format("%s must be between %d and %d", name, min, max)
					or string.format("%s must be >= %d", name, min)
				lib.fail("invalid_arguments", msg)
			end
		end

		function service.register(_, caller, name, implementation)
			assert(
				type(name) == "string" and #name <= 64 and name:match("^[a-z][a-z0-9_-]*$"),
				"invalid spill backend name"
			)
			assert(not backends[name], "spill backend already registered: " .. name)
			assert(type(implementation) == "table", "spill backend must be a table")
			assert(
				type(implementation.identity) == "string"
					and #implementation.identity > 0
					and #implementation.identity <= 128,
				"spill backend requires a stable opaque identity"
			)
			local captured = { identity = implementation.identity }
			for _, method in ipairs({ "save", "read", "delete", "save_bytes", "read_bytes", "dispose" }) do
				assert(type(implementation[method]) == "function", "spill backend missing " .. method)
				captured[method] = implementation[method]
			end
			backends[name] = captured
			local removed = false
			local function remove()
				if removed then
					return
				end
				removed = true
				if backends[name] == captured then
					backends[name] = nil
					changed(name, "removed")
				end
				captured.dispose()
			end
			-- the backend is withdrawn with the registering plugin's fiber
			local release = lib.owned(caller, remove, "spill backend " .. name)
			changed(name, "registered")
			return release
		end

		function service.list_backends()
			local result = mare.json.array({})
			for name in pairs(backends) do
				result[#result + 1] = { name = name, selected = name == cfg.backend }
			end
			table.sort(result, function(a, b)
				return a.name < b.name
			end)
			return { data = result }
		end

		function service.save(_, _, opts)
			local owner = active_session_id("unavailable")
			if type(opts) ~= "table" then
				lib.fail("invalid_arguments", "spill.save requires an arguments table")
			end
			local has_content = opts.content ~= nil
			local has_data = opts.data_base64 ~= nil
			if has_content == has_data then
				lib.fail("invalid_arguments", "spill.save requires exactly one of `content` or `data_base64`")
			end
			if has_content then
				if type(opts.content) ~= "string" then
					lib.fail("invalid_arguments", "spill.save `content` must be a string")
				end
				local saved = save_for("save", owner, opts.content, { backend = opts.backend })
				return { spill_id = saved.spill_id, size = saved.size }
			end
			if type(opts.data_base64) ~= "string" then
				lib.fail("invalid_arguments", "spill.save `data_base64` must be a string")
			end
			if #opts.data_base64 > MAX_SPILL_BASE64 then
				lib.fail("invalid_arguments", "base64 payload exceeds 8 MiB limit")
			end
			local bytes = mare.crypto.base64_decode(opts.data_base64)
			if bytes == nil or mare.crypto.base64_encode(bytes) ~= opts.data_base64 then
				lib.fail("invalid_arguments", "invalid canonical padded base64 payload")
			end
			if #bytes > MAX_SPILL_BYTES then
				lib.fail("invalid_arguments", "spill payload exceeds 8 MiB limit")
			end
			local saved = save_for("save_bytes", owner, bytes, { backend = opts.backend })
			return { spill_id = saved.spill_id, size = saved.size }
		end

		function service.read(_, _, spill_id, opts)
			local owner = active_session_id()
			opts = opts or {}
			local encoding = opts.encoding or "text"
			bounded_int(opts.offset, "offset", 0)
			if encoding == "base64" then
				bounded_int(opts.limit, "base64 read limit", 1, MAX_SPILL_BYTES)
				local page = read_for("read_bytes", owner, spill_id, { offset = opts.offset, limit = opts.limit })
				return {
					data_base64 = mare.crypto.base64_encode(page.bytes),
					next_offset = page.next_offset,
					size = page.size,
				}
			elseif encoding == "text" then
				bounded_int(opts.limit, "text read limit", 1, MAX_TEXT_LIMIT)
				local page = read_for("read", owner, spill_id, { offset = opts.offset, limit = opts.limit })
				return {
					content = page.content,
					next_offset = page.next_offset,
				}
			else
				lib.fail("invalid_arguments", "unknown read encoding: " .. tostring(encoding))
			end
		end

		function service.delete(_, _, spill_id)
			local owner = active_session_id()
			if not delete_for(owner, spill_id) then
				lib.fail("spill.unknown_spill", "spill object not found")
			end
			return {}
		end

		function service.save_owned(_, _, owner, content, opts)
			return save_for("save", owner, content, opts)
		end
		function service.save_bytes(_, _, owner, content, opts)
			return save_for("save_bytes", owner, content, opts)
		end
		function service.read_owned(_, _, owner, spill_id, opts)
			return read_for("read", owner, spill_id, opts)
		end
		function service.read_bytes(_, _, owner, spill_id, opts)
			return read_for("read_bytes", owner, spill_id, opts)
		end
		function service.delete_owned(_, _, owner, spill_id)
			return delete_for(owner, spill_id)
		end

		-- a mare.Service: `caller` in a method is the calling plugin's ctx
		lib.service(ctx, "spill", service)
	end,
}
