-- terminal: owner-bound PTY sessions (`terminal.create/send/read/signal/resize/close/list`).
local MAX_SESSIONS_PER_OWNER = 16
local DEFAULT_COLS = 80
local DEFAULT_ROWS = 24
local DEFAULT_BUFFER_BYTES = 4 * 1024 * 1024
local DEFAULT_MAX_BYTES = 256 * 1024
local MAX_MAX_BYTES = DEFAULT_BUFFER_BYTES
local MAX_WAIT_MS = 60000

local function resolve_max_bytes(raw)
	if raw == nil then
		return DEFAULT_MAX_BYTES
	end
	return math.max(1, math.min(MAX_MAX_BYTES, math.floor(raw)))
end

return {
	name = "terminal",
	provide = { "terminal" },
	-- `task` gives ctx:spawn
	inject = { "task" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local next_id = 0
		local touch_seq = 0

		local function touch(session)
			touch_seq = touch_seq + 1
			session.last_used = touch_seq
		end

		local function refresh_exit(session)
			if session.exit_code ~= nil or session.exit_signal ~= nil then
				return
			end
			local ok, st = pcall(session.pty.exit_status, session.pty)
			if ok and type(st) == "table" then
				session.exit_code = st.code
				session.exit_signal = st.signal
			end
		end

		local function notify_waiters(session)
			local waiters = session.waiters
			if not next(waiters) then
				return
			end
			session.waiters = {}
			for tx in pairs(waiters) do
				pcall(tx.send, tx, true)
			end
		end

		local function append_stream(session, chunk)
			session.chunks[#session.chunks + 1] = chunk
			session.chunk_bytes = session.chunk_bytes + #chunk
			session.cursor = session.cursor + #chunk
			while session.chunk_bytes > session.log_limit and #session.chunks > 0 do
				local excess = session.chunk_bytes - session.log_limit
				local head = session.chunks[1]
				if excess >= #head then
					table.remove(session.chunks, 1)
					session.chunk_bytes = session.chunk_bytes - #head
					session.base_offset = session.base_offset + #head
				else
					local drop = excess
					while drop < #head do
						local b = string.byte(head, drop + 1) or 0
						if b < 0x80 or b >= 0xc0 then
							break
						end
						drop = drop + 1
					end
					if drop >= #head then
						table.remove(session.chunks, 1)
						session.chunk_bytes = session.chunk_bytes - #head
						session.base_offset = session.base_offset + #head
					else
						session.chunks[1] = head:sub(drop + 1)
						session.chunk_bytes = session.chunk_bytes - drop
						session.base_offset = session.base_offset + drop
					end
				end
				session.truncated = true
			end
		end

		local function slice_stream(session, from_cursor, max_bytes)
			local wanted = math.max(0, math.floor(from_cursor or 0))
			local dropped = wanted < session.base_offset
			local start = math.max(wanted, session.base_offset)
			if start >= session.cursor then
				return "", session.cursor, dropped
			end
			local parts, pos = {}, session.base_offset
			for _, chunk in ipairs(session.chunks) do
				local chunk_end = pos + #chunk
				if chunk_end > start then
					local s = start > pos and (start - pos + 1) or 1
					parts[#parts + 1] = chunk:sub(s)
				end
				pos = chunk_end
			end
			local text = table.concat(parts)
			local capped, cut = lib.truncate_utf8(text, { max_bytes = max_bytes, floor_one_char = true })
			return capped, start + #capped, dropped or cut
		end

		local function finished_status(session)
			if session.cancelled then
				return "cancelled"
			end
			if session.exit_code == 0 and session.exit_signal == nil then
				return "completed"
			end
			return "failed"
		end

		local function snapshot(session, opts)
			local out = {
				terminal_id = session.id,
				name = session.name,
				pid = session.pid,
				cols = session.cols,
				rows = session.rows,
				status = session.status,
				exit_code = session.exit_code,
				exit_signal = session.exit_signal,
				output_offset = session.cursor,
				truncated = session.truncated,
			}
			if session.emulate then
				local include_screen = opts and opts.screen == true or false
				local ok, view = pcall(session.pty.view, session.pty, {
					screen = include_screen,
					max_bytes = resolve_max_bytes(opts and opts.max_bytes),
				})
				if ok and type(view) == "table" then
					out.screen = view.screen
					out.view_truncated = view.view_truncated
					out.alternate_screen = view.alternate_screen
					out.prompt_seq = view.prompt_generation
					out.command_exit_code = view.command_exit_code
				end
			end
			return out
		end

		local function wait_for(session, wait_ms, done_fn)
			local ms = math.max(0, math.min(MAX_WAIT_MS, math.floor(wait_ms or 0)))
			if ms == 0 or done_fn() then
				return false
			end
			local tx, rx = mare.task.channel()
			session.waiters[tx] = true
			local finished = mare.task.timeout(ms, function()
				while not done_fn() do
					session.waiters[tx] = true
					rx:recv()
				end
			end)
			session.waiters[tx] = nil
			return not finished
		end

		local function close_session(bucket, session, grace_ms)
			local was_running = session.status == "running"
			if was_running then
				session.cancelled = true
				local res = session.pty:stop({ grace_ms = grace_ms })
				if res.stop_unconfirmed then
					session.cancelled = false -- close-guard-ok: rolls back cancelled only when stop_unconfirmed
					lib.fail("terminal.stop_unconfirmed", "terminal process group did not confirm termination")
				end
			else
				pcall(session.pty.stop, session.pty, { grace_ms = grace_ms })
			end
			bucket.sessions[session.id] = nil
			if session.name and bucket.by_name[session.name] == session.id then
				bucket.by_name[session.name] = nil
			end
			refresh_exit(session)
			if was_running then
				session.status = "cancelled"
			elseif session.status == "running" then
				session.status = finished_status(session)
			end
			notify_waiters(session)
			if session.reader and not session.reader:done() then
				session.reader:abort()
			end
			local closed_snap = snapshot(session, { screen = false })
			bucket.closed[session.id] = closed_snap
			return closed_snap
		end

		local registry = lib.owner_registry(ctx, function(bucket)
			local list = {}
			for _, session in pairs(bucket.sessions) do
				list[#list + 1] = session
			end
			for _, session in ipairs(list) do
				pcall(close_session, bucket, session, 0)
			end
		end, "terminal")
		ctx:inject({ "session" }, function(sub)
			if sub.session.on_drain then
				sub.session:on_drain(registry.close_all)
			end
		end)

		local function new_bucket()
			return { sessions = {}, by_name = {}, closed = {}, count = 0 }
		end

		local function owner_bucket(caller, create)
			return registry.get(caller, create and new_bucket or nil)
		end

		local function expect_owned(caller, terminal_id)
			assert(type(terminal_id) == "string" and terminal_id ~= "", "terminal_id is required")
			local bucket = owner_bucket(caller, false)
			local session = bucket and bucket.sessions[terminal_id]
			if session == nil then
				lib.fail("terminal.unknown_terminal", "unknown terminal: " .. terminal_id)
			end
			return bucket, session
		end

		local function prune_exited_if_needed(bucket)
			local count = 0
			for _ in pairs(bucket.sessions) do
				count = count + 1
			end
			while count >= MAX_SESSIONS_PER_OWNER do
				local victim
				for _, existing in pairs(bucket.sessions) do
					if existing.status ~= "running" then
						if victim == nil or (existing.last_used or 0) < (victim.last_used or 0) then
							victim = existing
						end
					end
				end
				if victim == nil then
					return false
				end
				close_session(bucket, victim, 0)
				count = count - 1
			end
			return true
		end

		local service = {}

		function service.create(_, caller, opts)
			mare.exec.check_mutation("terminal.create")
			opts = opts or {}
			assert(type(opts) == "table", "terminal.create expects an options table")
			local bucket = owner_bucket(caller, true)
			if not prune_exited_if_needed(bucket) then
				lib.fail("terminal.terminal_limit_reached", "terminal session limit reached")
			end
			local name = opts.name
			if name ~= nil then
				assert(type(name) == "string" and #name > 0 and #name <= 128, "invalid terminal session name")
				if bucket.by_name[name] ~= nil then
					lib.fail("terminal.duplicate_name", "terminal session name already exists: " .. name)
				end
			end
			local cmd = opts.cmd or mare.env.get("SHELL") or (mare.env.os == "windows" and "cmd.exe" or "sh")
			local cols = opts.cols or DEFAULT_COLS
			local rows = opts.rows or DEFAULT_ROWS
			local emulate = opts.emulate ~= false
			local log_limit =
				math.max(1024, math.min(DEFAULT_BUFFER_BYTES, math.floor(opts.buffer_bytes or DEFAULT_BUFFER_BYTES)))
			local pty = mare.pty.spawn({
				cmd = cmd,
				args = opts.args,
				cwd = opts.cwd or mare.cwd,
				env = opts.env,
				cols = cols,
				rows = rows,
				buffer_bytes = log_limit,
				scrollback_lines = opts.scrollback_lines,
				emulate = emulate,
			})
			next_id = next_id + 1
			local id = "term-" .. tostring(next_id)
			local session = {
				id = id,
				name = name,
				pty = pty,
				pid = pty:pid(),
				cols = cols,
				rows = rows,
				emulate = emulate,
				log_limit = log_limit,
				chunks = {},
				chunk_bytes = 0,
				base_offset = 0,
				cursor = 0,
				truncated = false,
				status = "running",
				waiters = {},
			}
			touch(session)
			session.reader = ctx:spawn(function()
				while true do
					local ok, chunk = pcall(pty.read_text, pty)
					if not ok or chunk == nil then
						break
					end
					if #chunk > 0 then
						append_stream(session, chunk)
					end
					notify_waiters(session)
				end
				refresh_exit(session)
				if session.exit_code == nil and session.exit_signal == nil then
					for _ = 1, 20 do
						mare.task.sleep(10)
						refresh_exit(session)
						if session.exit_code ~= nil or session.exit_signal ~= nil then
							break
						end
					end
				end
				session.status = finished_status(session)
				if session.name and bucket.by_name[session.name] == session.id then
					bucket.by_name[session.name] = nil
				end
				notify_waiters(session)
			end)
			bucket.sessions[id] = session
			if name then
				bucket.by_name[name] = id
			end
			return snapshot(session, { screen = false })
		end

		function service.send(_, caller, opts)
			mare.exec.check_mutation("terminal.send")
			assert(type(opts) == "table", "terminal.send expects an options table")
			local _, session = expect_owned(caller, opts.terminal_id)
			touch(session)
			if session.status ~= "running" then
				lib.fail("terminal.not_running", "terminal is not running: " .. session.id)
			end
			local text = opts.text or ""
			assert(type(text) == "string", "terminal.send text must be a string")
			if opts.submit == true then
				text = text .. "\n"
			end
			local start_cursor = session.cursor
			local start_prompt = snapshot(session, { screen = false }).prompt_seq or 0
			if #text > 0 then
				session.pty:write(text)
			end
			local max_bytes = resolve_max_bytes(opts.max_bytes)
			local timed_out = wait_for(session, opts.wait_ms or 0, function()
				if session.status ~= "running" then
					return true
				end
				if (session.cursor - start_cursor) >= max_bytes then
					return true
				end
				if session.emulate then
					local cur_prompt = snapshot(session, { screen = false }).prompt_seq or 0
					if cur_prompt > start_prompt then
						return true
					end
				end
				return false
			end)
			local output, next_offset, sliced_trunc = slice_stream(session, start_cursor, max_bytes)
			local res = snapshot(session, { screen = opts.screen == true, max_bytes = max_bytes })
			res.output = output
			res.next_output_offset = next_offset
			res.timed_out = timed_out
			res.truncated = res.truncated or sliced_trunc
			return res
		end

		function service.read(_, caller, opts)
			assert(type(opts) == "table", "terminal.read expects an options table")
			local _, session = expect_owned(caller, opts.terminal_id)
			touch(session)
			local max_bytes = resolve_max_bytes(opts.max_bytes)
			if (opts.wait_ms or 0) > 0 then
				local target_cursor = opts.output_offset or session.cursor
				wait_for(session, opts.wait_ms, function()
					return session.status ~= "running" or session.cursor > target_cursor
				end)
			end
			local res = snapshot(session, { screen = opts.screen == true, max_bytes = max_bytes })
			if opts.output_offset ~= nil then
				local text, next_offset, sliced_trunc = slice_stream(session, opts.output_offset, max_bytes)
				res.text = text
				res.total_lines = 0
				res.line_begin = 0
				res.line_end = 0
				res.next_output_offset = next_offset
				res.truncated = res.truncated or sliced_trunc
				return res
			end
			if session.emulate then
				local page = session.pty:page({
					offset = opts.offset,
					count = opts.count,
					max_bytes = max_bytes,
				})
				res.text = page.text
				res.total_lines = page.total_lines
				res.line_begin = page.line_begin
				res.line_end = page.line_end
				res.next_output_offset = session.cursor
				res.truncated = res.truncated or page.truncated
				return res
			end
			local text, next_offset, sliced_trunc = slice_stream(session, 0, max_bytes)
			res.text = text
			res.total_lines = 0
			res.line_begin = 0
			res.line_end = 0
			res.next_output_offset = next_offset
			res.truncated = res.truncated or sliced_trunc
			return res
		end

		function service.signal(_, caller, opts)
			mare.exec.check_mutation("terminal.signal")
			assert(type(opts) == "table", "terminal.signal expects an options table")
			local _, session = expect_owned(caller, opts.terminal_id)
			touch(session)
			if session.status ~= "running" then
				lib.fail("terminal.not_running", "terminal is not running: " .. session.id)
			end
			assert(type(opts.signal) == "string", "terminal.signal requires signal")
			session.pty:signal(opts.signal)
			return {}
		end

		function service.resize(_, caller, opts)
			mare.exec.check_mutation("terminal.resize")
			assert(type(opts) == "table", "terminal.resize expects an options table")
			local _, session = expect_owned(caller, opts.terminal_id)
			touch(session)
			if session.status ~= "running" then
				lib.fail("terminal.not_running", "terminal is not running: " .. session.id)
			end
			assert(type(opts.cols) == "number" and type(opts.rows) == "number", "cols and rows are required")
			session.pty:resize(opts.cols, opts.rows)
			session.cols = opts.cols
			session.rows = opts.rows
			return {}
		end

		function service.close(_, caller, opts)
			mare.exec.check_mutation("terminal.close")
			assert(type(opts) == "table", "terminal.close expects an options table")
			assert(type(opts.terminal_id) == "string" and opts.terminal_id ~= "", "terminal_id is required")
			local bucket = owner_bucket(caller, false)
			if bucket and bucket.closed[opts.terminal_id] then
				return bucket.closed[opts.terminal_id]
			end
			local b, session = expect_owned(caller, opts.terminal_id)
			return close_session(b, session, opts.grace_ms)
		end

		function service.list(_, caller)
			local bucket = owner_bucket(caller, false)
			local list = {}
			if not bucket then
				return { data = mare.json.array(list) }
			end
			for _, session in pairs(bucket.sessions) do
				list[#list + 1] = snapshot(session, { screen = false })
			end
			table.sort(list, function(a, b)
				return a.terminal_id < b.terminal_id
			end)
			return { data = mare.json.array(list) }
		end

		-- a mare.Service: `caller` in a method is the calling plugin's ctx
		lib.service(ctx, "terminal", service)
	end,
}
