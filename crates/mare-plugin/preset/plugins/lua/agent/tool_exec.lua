-- agent.tool_exec: tool call extraction, result messages, model truncation policy, call states
-- and run_tool; part of the `agent` plugin (plugins/agent.lua), loaded at the start of its apply.
return function(D)
	local ctx, A = D.ctx, D.A
	local flush_pending, persist = D.persist.flush_pending, D.persist.persist

	local apply_truncation_policy -- defined below (needs ctx.llm)
	local function tool_calls(msg)
		local out = {}
		for _, b in ipairs(msg.content or {}) do
			if b.type == "tool_call" and not b.hosted then
				out[#out + 1] = b
			end
		end
		return out
	end

	local function normalize_content_block(b)
		if type(b) ~= "table" then
			return { type = "text", text = tostring(b) }
		end
		if b.type == "image" then
			local mime = (type(b.mime) == "string" and b.mime ~= "" and b.mime)
				or (type(b.media_type) == "string" and b.media_type ~= "" and b.media_type)
				or nil
			local data = type(b.data) == "string" and b.data ~= "" and b.data or nil
			local bytes = type(b.bytes) == "number" and b.bytes >= 0 and math.floor(b.bytes) or nil
			if b.truncated == true and mime then
				local out = { type = "image", mime = mime, bytes = bytes or 0, truncated = true }
				if type(b.path) == "string" and b.path ~= "" then
					out.path = b.path
				end
				return out
			end
			if data and mime then
				local out = {}
				for k, v in pairs(b) do
					if k ~= "media_type" then
						out[k] = v
					end
				end
				out.type = "image"
				out.mime = mime
				out.data = data
				if out.truncated == true then
					out.truncated = nil
				end
				return out
			end
			-- Degrade malformed image blocks lacking mime or data/truncated-ref to text so providers never reject the request.
			return { type = "text", text = "[invalid image block]" }
		end
		if b.type == "text" then
			if type(b.text) == "string" and (b.truncated == nil or type(b.truncated) == "boolean") then
				return b
			end
			local out = {}
			for k, v in pairs(b) do
				out[k] = v
			end
			out.text = b.text == nil and "" or tostring(b.text)
			if out.truncated ~= nil and type(out.truncated) ~= "boolean" then
				out.truncated = out.truncated and true or false
			end
			return out
		end
		return b
	end

	local function array_content(content)
		if type(content) == "string" then
			return mare.json.array({ { type = "text", text = content } })
		end
		if type(content) ~= "table" then
			return mare.json.array()
		end
		local out = mare.json.array()
		for i, b in ipairs(content) do
			out[i] = normalize_content_block(b)
		end
		return out
	end

	local function result_message(call, res)
		return {
			role = "tool_result",
			tool_call_id = call.id,
			tool_name = call.name,
			content = array_content(res.content),
			is_error = res.is_error or false,
			details = res.details,
			timestamp = mare.task.now(),
		}
	end

	--- Model-specific output budget: GPT models use a
	--- token budget (e.g. 10000 tokens ≈ 4 bytes/token); others keep the tools plugin's
	--- 2000 lines / 50KB. Keeps head and tail like the generic truncation.
	apply_truncation_policy = function(res)
		local m = ctx.llm and ctx.llm:current()
		local pol = m and m.truncation
		if not pol or not pol.limit or res.no_truncate then
			return res
		end
		local max_bytes = pol.mode == "tokens" and pol.limit * 4 or pol.limit
		if type(res.output_notice) == "string" and type(res.content) == "string" then
			local text = res.content
			local suffix = res.output_notice
			if text:sub(-#suffix) == suffix then
				text = text:sub(1, -#suffix - 1):gsub("\n$", "")
			end
			res.content = mare.text.retain(text, { max_bytes = max_bytes, max_lines = 1000000000, suffix = suffix })
			return res
		end
		local function cut(text)
			if #text <= max_bytes then
				return text
			end
			local t, info = mare.text.truncate(text, { max_lines = 1e9, max_bytes = max_bytes, keep = "middle" })
			return t
				.. ("\n[output truncated to the model's %d-%s budget: %d of %d lines shown]"):format(
					pol.limit,
					pol.mode == "tokens" and "token" or "byte",
					info.shown_lines,
					info.total_lines
				)
		end
		local content = res.content
		if type(content) == "string" then
			res.content = cut(content)
		elseif type(content) == "table" then
			for _, b in ipairs(content) do
				if type(b) == "table" and b.type == "text" and type(b.text) == "string" then
					b.text = cut(b.text)
				end
			end
		end
		return res
	end

	--- record a tool call state durably (prepared → started → completed); nil, err on failure
	local function tool_state(call, state, info)
		local st = A.tool_states[call.id] or {}
		st.state, st.side_effect, st.idempotent = state, info.side_effect, info.idempotent
		A.tool_states[call.id] = st
		return persist({
			kind = "state",
			payload = {
				tool_call_id = call.id,
				name = call.name,
				state = state,
				side_effect = info.side_effect,
				idempotent = info.idempotent,
			},
		})
	end

	--- Run one tool call through the built-in tools service (tools:execute: rewrite → frozen
	--- snapshot → policy vote → approval → re-check → controlled execution
	--- one). The agent only supplies persistence: `prepared` / `started` records must reach
	--- the session before the entry goes on (a failure refuses the call).
	local function run_tool(call, token, declarations, context)
		-- persistence fault: every tool stops (read-only ones also write prepared + result)
		-- until the missing records are written again
		local function not_saved(err)
			A.halt = "the session can't be saved (" .. tostring(err) .. ")"
			return {
				content = "Not run: the session can't be saved ("
					.. tostring(err)
					.. "); no tool runs until session writes work again.",
				is_error = true,
				error_code = "persist_failed",
			}
		end
		if A.persist_error and not flush_pending() then
			return not_saved(A.persist_error)
		end
		local prev_out = ""
		local out_offset = 0
		local output_gate = context and context.output_gate or nil
		local on_output = output_gate
				and function(blocks, send_opts)
					if output_gate.closed then
						return { status = "rejected", error = output_gate.error or "channel_closed" }
					end
					if not output_gate.committed then
						return { status = "rejected", error = "initial_result_pending" }
					end
					if A.session_id ~= output_gate.session_id then
						output_gate.closed = true
						output_gate.error = "session_changed"
						return { status = "rejected", error = "session_changed" }
					end
					if send_opts and send_opts.cancel and send_opts.cancel:is_cancelled() then
						return { status = "rejected", error = "cancelled" }
					end
					local texts = {}
					for _, b in ipairs(blocks) do
						if
							type(b) ~= "table"
							or b.type ~= "text"
							or type(b.text) ~= "string"
							or not b.text:find("%S")
						then
							return { status = "rejected", error = "unsupported_block" }
						end
						texts[#texts + 1] = b.text
					end
					if #texts == 0 then
						return { status = "rejected", error = "invalid_blocks" }
					end
					if not D.notify_output then
						return { status = "rejected", error = "active_output_unavailable" }
					end
					local nok, nerr = D.notify_output(table.concat(texts, "\n"), { wake = true })
					if not nok then
						return { status = "rejected", error = tostring(nerr or "active_output_unavailable") }
					end
					return { status = "accepted" }
				end
			or nil
		local ok, res = pcall(function()
			return ctx.tools:execute(call, {
				cancel = token,
				run_cancel = token,
				declarations = declarations,
				route_snapshot = context and context.route_snapshot or nil,
				model = context and context.model or nil,
				step_id = context and context.step_id or nil,
				on_output = on_output,
				on_state = function(c, state, info)
					local sok, serr = tool_state(c, state, info)
					if sok and state == "started" then
						if context and context.started_calls then
							context.started_calls[call.id] = true
						end
						ctx.bus:emit("pipeline/tool-dispatching", { call = c, info = info })
						local copy_deep = require("mare_lib").copy_deep
						local item_arguments = copy_deep(call.arguments)
						local item_info = copy_deep(info)
						ctx.bus:emit("item/started", {
							turn_id = A.turn_id,
							item_id = call.id,
							step_id = context and context.step_id or nil,
							item = {
								type = "tool_call",
								tool_call_id = call.id,
								provider_call_id = call.provider_call_id,
								name = call.name,
								arguments = item_arguments,
								status = "running",
								info = item_info,
							},
						})
					end
					return sok, serr
				end,
				on_update = function(partial)
					if type(partial) == "table" and type(partial.content) == "table" then
						partial.content = array_content(partial.content)
					end
					local raw = ""
					if type(partial) == "table" then
						if type(partial.delta) == "string" then
							raw = partial.delta
						elseif type(partial.output) == "string" then
							raw = partial.output
						elseif type(partial.content) == "string" then
							raw = partial.content
						end
					elseif type(partial) == "string" then
						raw = partial
					end
					local delta
					if type(partial) == "table" and type(partial.delta) == "string" then
						delta = partial.delta
					elseif prev_out ~= "" and raw:sub(1, #prev_out) == prev_out then
						delta = raw:sub(#prev_out + 1)
						prev_out = raw
					else
						delta = raw
						if raw ~= "" then
							prev_out = raw
						end
					end
					local off = out_offset
					out_offset = off + #delta
					ctx.bus:emit("item/delta", {
						turn_id = A.turn_id,
						item_id = call.id,
						kind = "tool_output",
						delta = delta,
						offset = off,
						tool_call_id = call.id,
						partial = partial,
					})
				end,
			})
		end)
		if not ok then
			res = { content = "Tool error: " .. tostring(res), is_error = true }
		end
		if res.error_code == "persist_failed" then
			A.halt = A.halt or "the session can't be saved"
			res._mare_result_status = "persist_failed"
			return res
		end
		if res.refused then
			-- never started (denied, invalid, blocked): no tool/after, no truncation
			res.refused = nil
			res._mare_result_status = "denied"
			return res
		end
		local cancelled = res.cancelled == true
		if mare.projection and mare.projection.project then
			local out, lossy = mare.projection.project(res, { lossy = true })
			if lossy and mare.ui and mare.ui.notify then
				mare.ui.notify("tool result contained non-UTF-8 bytes; replaced lossily with U+FFFD", "warning")
			end
			res = out
		end
		res = apply_truncation_policy(res)
		if type(res.content) == "table" then
			res.content = array_content(res.content)
		end
		if cancelled or res.cancelled then
			res._mare_result_status = "cancelled"
		else
			res._mare_result_status = res.is_error and "error" or "success"
		end
		-- `completed` is written with the result (commit below): result first, then the
		-- marker; recovery goes by whether the result exists
		return res
	end

	return {
		tool_calls = tool_calls,
		result_message = result_message,
		tool_state = tool_state,
		run_tool = run_tool,
	}
end
