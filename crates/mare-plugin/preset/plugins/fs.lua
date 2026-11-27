-- fs: policy-constrained filesystem read/write/list/stat service.
---@type mare.PluginDef
return {
	name = "fs",
	tags = { "minimal" },
	inject = { "bus" },
	provide = { "fs" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local fs_read = mare.fs.read
		local fs_write = mare.fs.write
		local fs_list = mare.fs.list
		local fs_stat = mare.fs.stat
		local base64_encode = mare.crypto.base64_encode
		local base64_decode = mare.crypto.base64_decode
		local json_array = mare.json.array
		local watches_by_owner = {}
		local next_lua_watch = 0

		local Fs = require("cordis").class("FsService", mare.Service)
		function Fs:constructor(owner)
			Fs.super.constructor(self, owner, "fs")
		end

		local function fail_op(op, path, err, kind)
			local detail = tostring(err)
			local msg = ("%s `%s`: %s"):format(op, path, detail)
			if kind == "denied" or detail:find("sandbox:", 1, true) or detail:find("blocked by policy", 1, true) then
				lib.fail("denied", msg)
			elseif kind == "not_found" then
				lib.fail("fs.not_found", msg)
			elseif kind == "already_exists" then
				lib.fail("fs.already_exists", msg)
			elseif kind == "too_large" then
				lib.fail("fs.too_large", msg)
			end
			lib.fail("fs.io_failed", msg)
		end

		function Fs:read(path, opts)
			opts = opts or {}
			if opts.max_bytes ~= nil and opts.max_bytes < 0 then
				lib.fail("invalid_arguments", "max_bytes must be non-negative")
			end
			local ok, bytes, err, kind = pcall(fs_read, path, {
				max_bytes = opts.max_bytes,
			})
			if not ok or bytes == nil then
				fail_op("fs.read", path, ok and err or bytes, ok and kind or nil)
			end
			---@cast bytes string
			return { data_base64 = base64_encode(bytes) }
		end

		function Fs:write(path, data_base64, opts)
			opts = opts or {}
			if type(data_base64) ~= "string" or data_base64:find("%s") then
				lib.fail("invalid_arguments", "invalid data_base64")
			end
			local bytes = base64_decode(data_base64)
			if bytes == nil then
				lib.fail("invalid_arguments", "invalid data_base64")
			end
			local ok, written, err, kind = pcall(fs_write, path, bytes, {
				atomic = opts.atomic == true,
				create_new = opts.create_new == true,
				capture_snapshot = true,
			})
			if not ok or not written then
				fail_op("fs.write", path, ok and err or written, ok and kind or nil)
			end
			return {}
		end

		function Fs:list(path)
			local ok, entries, err, kind = pcall(fs_list, path, {
				strict = true,
				filter_denied = true,
			})
			if not ok or entries == nil then
				fail_op("fs.list", path, ok and err or entries, ok and kind or nil)
			end
			---@cast entries mare.FsListEntries
			local data = {}
			for _, entry in ipairs(entries) do
				data[#data + 1] = {
					file_name = entry.name,
					is_file = entry.kind == "file",
					is_directory = entry.kind == "dir",
				}
			end
			return { data = json_array(data) }
		end

		function Fs:stat(path)
			local ok, st, err, kind = pcall(fs_stat, path, {
				strict = true,
			})
			if not ok or st == nil then
				fail_op("fs.stat", path, ok and err or st, ok and kind or nil)
			end
			---@cast st table
			return {
				is_file = st.kind == "file",
				is_directory = st.kind == "dir",
				size = st.size,
				created_at_ms = st.ctime or 0,
				modified_at_ms = math.floor(st.mtime or 0),
				mode = st.mode,
				nlink = st.nlink,
			}
		end

		function Fs:watch(path)
			local caller_id = lib.caller_id(self.ctx) or lib.caller_id(ctx) or 0
			local count = watches_by_owner[caller_id] or 0
			if count >= 32 then
				lib.fail("busy", "too many active filesystem watches (32 max per connection); release one first")
			end
			watches_by_owner[caller_id] = count + 1
			next_lua_watch = next_lua_watch + 1
			local watch_id = "lua-watch-" .. tostring(next_lua_watch)
			local disposed = false
			local function dec_count()
				local cur = watches_by_owner[caller_id] or 0
				if cur <= 1 then
					watches_by_owner[caller_id] = nil
				else
					watches_by_owner[caller_id] = cur - 1
				end
			end
			local ok, handle = pcall(mare.fs.watch, path, function(ev)
				if disposed then
					return
				end
				ctx.bus:emit("fs/watch-event", {
					owner = caller_id,
					watch_id = watch_id,
					changed_paths = json_array(ev.changed_paths or {}),
					truncated = ev.truncated == true,
				})
			end)
			if not ok or not handle then
				dec_count()
				lib.fail("invalid_arguments", tostring(handle))
			end
			local function dispose()
				if disposed then
					return
				end
				disposed = true
				dec_count()
				handle.stop()
			end
			return setmetatable({
				_bind_id = function(reg_id)
					watch_id = reg_id
				end,
			}, {
				__call = dispose,
			})
		end

		ctx:plugin(Fs)
	end,
}
