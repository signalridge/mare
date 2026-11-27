local function resolve_token(tok)
	return type(tok) == "function" and tok() or tok
end
-- Asynchronous script cells. Results are consumed once, with an output cursor per cell.
return function(ctx, run)
	local lib = require("mare_lib")
	local cells = {}
	local function cell_key(owner, cell_id)
		return tostring(owner or "host") .. "/" .. tostring(cell_id)
	end
	local function integer(value, default, max)
		value = value == nil and default or value
		if math.type(value) ~= "integer" or value < 0 or value > max then
			lib.fail("invalid_arguments", "invalid cell wait/output budget")
		end
		return value
	end
	local function snapshot(cell, options)
		local content = mare.json.array()
		local bytes = integer(options.max_output_tokens, 10000, 1000000) * 4
		local error_index = (
			cell.done
			and type(cell.result) == "table"
			and type(cell.result.error) == "string"
			and cell.result.error ~= ""
			and #cell.items > cell.cursor
		)
				and #cell.items
			or nil
		local error_reserve = error_index
				and type(cell.items[error_index].text) == "string"
				and math.min(#cell.items[error_index].text, math.floor(bytes / 2))
			or 0
		local body_budget = math.max(0, bytes - error_reserve)
		for index = cell.cursor + 1, #cell.items do
			local block = table.copy(cell.items[index])
			if block.type == "text" then
				local limit = index == error_index and (body_budget + error_reserve) or body_budget
				if #block.text > limit then
					block.text =
						mare.text.truncate(block.text, { max_bytes = limit, max_lines = 1000000, keep = "middle" })
					block.text = block.text .. "\n[script output truncated to this call's budget]"
				end
				if index < (error_index or (#cell.items + 1)) then
					body_budget = math.max(0, body_budget - #block.text)
				end
			end
			content[#content + 1] = block
		end
		cell.cursor = #cell.items
		local status = cell.done
				and (cell.result.cancelled and "cancelled" or cell.result.is_error and "failed" or "completed")
			or "running"
		local function clone_json(value)
			return mare.json.decode_lossless(mare.json.encode_strict(value))
		end
		local result = {
			cell_id = cell.cell_id,
			status = status,
			content = clone_json(content),
			result = clone_json(cell.done and cell.result.result or mare.json.null),
			error = cell.done and cell.result.error or nil,
			stop_unconfirmed = cell.done and cell.result.stop_unconfirmed or nil,
		}
		if cell.done then
			cells[cell.key] = nil
		end
		return result
	end
	local function wait(cell_id, options, owner)
		options = options or {}
		local cell = cells[cell_key(owner, cell_id)]
		if not cell then
			lib.fail("cells.unknown_cell", "unknown cell: " .. tostring(cell_id))
		end
		if options.terminate then
			cell.cancel:cancel()
		end
		local deadline = mare.task.clock() + integer(options.yield_time_ms, 10000, 600000)
		if options.terminate then
			deadline = mare.task.clock() + 5000
		end
		local cancel_tok = resolve_token(options.cancel)
		while not cell.done and not cell.notified and mare.task.clock() < deadline do
			if cancel_tok and cancel_tok:is_cancelled() then
				return {
					cell_id = cell.cell_id,
					status = "running",
					content = mare.json.array(),
					error = "Wait cancelled; script still running",
				}
			end
			mare.task.sleep(5)
		end
		cell.notified = false
		if options.terminate and not cell.done then
			return {
				cell_id = cell.cell_id,
				status = "running",
				content = mare.json.array(),
				stop_unconfirmed = true,
				error = "Script stop is unconfirmed",
			}
		end
		return snapshot(cell, options)
	end
	local function start(code, options, caller)
		options = table.copy(options or {})
		options.yield_time_ms = integer(options.yield_time_ms, 10000, 600000)
		options.max_output_tokens = integer(options.max_output_tokens, 10000, 1000000)
		local count = 0
		for _ in pairs(cells) do
			count = count + 1
		end
		if count >= 32 then
			lib.fail("cells.cell_limit_reached", "script cell limit reached; consume or terminate an existing cell")
		end
		if
			options.cell_id ~= nil
			and (type(options.cell_id) ~= "string" or #options.cell_id == 0 or #options.cell_id > 128)
		then
			lib.fail("invalid_arguments", "cell_id must be a non-empty string up to 128 bytes")
		end
		local owner = lib.caller_id(caller) or lib.caller_id(ctx)
		local cell_id = options.cell_id or mare.crypto.uuid()
		local key = cell_key(owner, cell_id)
		if cells[key] then
			lib.fail("cells.duplicate_cell", "cell_id is already active: " .. cell_id)
		end
		local caller_cancel = resolve_token(options.cancel)
		local token = caller_cancel and caller_cancel:child() or mare.task.token()
		local initial_done = false
		local wait_cancel = resolve_token(options.wait_cancel)
		local watcher
		if wait_cancel then
			watcher = mare.task.spawn(function()
				wait_cancel:wait()
				token:cancel()
			end)
		end
		local initial_guard <close> = setmetatable({}, {
			__close = function()
				if watcher then
					watcher:abort()
				end
				if not initial_done then
					token:cancel()
					cells[key] = nil
				end
			end,
		})
		local cell = {
			owner = owner,
			key = key,
			cell_id = cell_id,
			cancel = token,
			items = {},
			cursor = 0,
			done = false,
			notified = false,
		}
		cells[key] = cell
		options.cell_id = cell_id
		options.cancel = token
		local channel, channel_error
		if lib.callable(ctx.tools.output_channel) then
			channel, channel_error = ctx.tools:output_channel()
		else
			channel_error = "active_output_unavailable"
		end
		cell.output_channel = channel
		options.output_channel = channel
		options.on_output = function(block)
			cell.items[#cell.items + 1] = table.copy(block)
		end
		options.on_yield = function()
			cell.notified = true
		end
		options.on_notify = function(block)
			if not channel then
				error("notify unavailable: " .. tostring(channel_error), 0)
			end
			-- Release the original waiter so its real initial result can commit first.
			cell.notified = true
			while true do
				local receipt = ctx.tools:send_output(channel, mare.json.array({ block }), { cancel = token })
				if receipt.status == "accepted" then
					return
				end
				if receipt.error ~= "initial_result_pending" or token:is_cancelled() then
					error("notify rejected: " .. tostring(receipt.error), 0)
				end
				mare.task.sleep(1, token)
			end
		end
		cell.task = ctx:spawn(function()
			local ok, result = pcall(run, nil, caller, code, options)
			if not ok then
				result = { output = "", is_error = true, error = tostring(result), cancelled = token:is_cancelled() }
			end
			-- Final retention may be shorter than admitted streaming output. Keep
			-- already consumed entries, replace only the undelivered suffix.
			if result.output_items then
				for i = #cell.items, cell.cursor + 1, -1 do
					cell.items[i] = nil
				end
				for i = cell.cursor + 1, #result.output_items do
					cell.items[#cell.items + 1] = table.copy(result.output_items[i])
				end
			end
			if channel then
				channel:close()
			end
			cell.result = result
			cell.done = true
			if result.error then
				cell.items[#cell.items + 1] = { type = "text", text = result.error }
			end
		end)
		local waiting = table.copy(options)
		waiting.cancel = wait_cancel or caller_cancel
		local result = wait(cell_id, waiting, owner)
		initial_done = not (wait_cancel and wait_cancel:is_cancelled())
		return result
	end
	local function cancel(owner, cell_id)
		local cell = cells[cell_key(owner, cell_id)]
		if not cell then
			return nil
		end
		local running = not cell.done
		cell.cancel:cancel()
		return running
	end
	local function list(owner)
		local out = mare.json.array()
		for _, cell in pairs(cells) do
			if not owner or cell.owner == owner then
				local st = cell.done
						and (cell.result.cancelled and "cancelled" or cell.result.is_error and "failed" or "completed")
					or "running"
				out[#out + 1] = { cell_id = cell.cell_id, status = st }
			end
		end
		table.sort(out, function(a, b)
			return a.cell_id < b.cell_id
		end)
		return out
	end
	local function clear(owner)
		for key, cell in pairs(cells) do
			if not owner or cell.owner == owner then
				if cell.output_channel then
					cell.output_channel:close()
				end
				cell.cancel:cancel()
				cells[key] = nil
			end
		end
	end
	ctx:on("session/changed", function()
		clear()
	end)
	ctx:effect(function()
		return function()
			clear()
		end
	end, "cleanup")
	return { start = start, wait = wait, cancel = cancel, list = list, clear = clear }
end
