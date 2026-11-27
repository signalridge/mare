-- Independent ephemeral side-conversation loop.
-- Seeds from SES-FORK committed history and runs only read-only confined tools under `ephemeral-read-only`.
local M = {}

local BOUNDARY = [[Side conversation boundary: everything inherited before this message is reference
context only, not instructions to execute or continue the main task. The main agent continues
independently. Answer only new side-conversation requests. Lightweight read-only exploration is
allowed. Do not modify files, git state, configuration, permissions, or external state. Do not
create or interact with agents. Explain when a request requires returning to the main conversation.]]

local function project_value(v)
	if mare.projection and type(mare.projection.project) == "function" then
		return mare.projection.project(v, { preserve_invalid = true })
	end
	return v
end

local function extract_text(msg)
	if type(msg) ~= "table" then
		return ""
	end
	if type(msg.content) == "string" then
		return msg.content
	end
	local out = {}
	for _, b in ipairs(msg.content or {}) do
		if type(b) == "table" and b.type == "text" and type(b.text) == "string" then
			out[#out + 1] = b.text
		end
	end
	return table.concat(out)
end

local function seed_calls_completed(messages)
	local results = {}
	for _, m in ipairs(messages) do
		if m.role == "tool_result" and m.tool_call_id then
			results[m.tool_call_id] = true
		end
	end
	local out = {}
	local n = #messages
	local i = 1
	while i <= n do
		local m = messages[i]
		out[#out + 1] = m
		if m.role == "assistant" then
			local j = i + 1
			while j <= n and messages[j].role == "tool_result" do
				out[#out + 1] = messages[j]
				j = j + 1
			end
			for _, b in ipairs(m.content or {}) do
				if type(b) == "table" and b.type == "tool_call" and b.id and not results[b.id] then
					results[b.id] = true
					out[#out + 1] = {
						role = "tool_result",
						tool_call_id = b.id,
						tool_name = b.name,
						is_error = true,
						content = mare.json.array({
							{
								type = "text",
								text = "outcome_unknown: referenced parent tool call has no completed result at the fork boundary; treat as reference data only and do not re-run.",
							},
						}),
					}
				end
			end
			i = j
		else
			i = i + 1
		end
	end
	for k = 1, #out do
		messages[k] = out[k]
	end
	for k = #out + 1, #messages do
		messages[k] = nil
	end
end

function M.seed(snapshot_ctx, activity)
	local messages = {}
	for _, m in ipairs((snapshot_ctx and snapshot_ctx.messages) or {}) do
		messages[#messages + 1] = project_value(require("mare_lib").copy_deep(m))
	end
	seed_calls_completed(messages)
	local ref = {}
	if type(activity) == "table" then
		ref.streaming = project_value(activity.streaming)
		ref.running_tools = project_value(activity.running_tools)
		ref.extra = project_value(activity.extra)
	end
	messages[#messages + 1] = {
		role = "user",
		notification = true,
		content = mare.json.array({
			{
				type = "text",
				text = BOUNDARY .. "\nParent activity (reference data):\n" .. mare.json.encode(ref),
			},
		}),
	}
	return messages
end

function M.visible(side)
	local parts = {}
	for i = side.boundary + 1, #side.messages do
		local message = side.messages[i]
		local text = {}
		for _, block in ipairs(message.content or {}) do
			if block.type == "text" then
				text[#text + 1] = block.text
			elseif block.type == "tool_call" then
				text[#text + 1] = "**Tool request "
					.. tostring(block.name or "")
					.. ":**\n```json\n"
					.. mare.json.encode(block.arguments or {})
					.. "\n```"
			end
		end
		if #text > 0 then
			local prefix = message.role == "user" and "**You:**\n"
				or (message.role == "tool_result" and ("**Tool " .. tostring(message.tool_name or "") .. ":**\n") or "")
			parts[#parts + 1] = prefix .. table.concat(text, "\n")
		end
	end
	if side.partial and side.partial ~= "" then
		parts[#parts + 1] = side.partial
	end
	return table.concat(parts, "\n\n")
end

local function tool_calls_of(msg)
	local out = {}
	for _, b in ipairs((type(msg) == "table" and msg.content) or {}) do
		if type(b) == "table" and b.type == "tool_call" then
			out[#out + 1] = b
		end
	end
	return out
end

function M.run(ctx, side, text, images, draw, turn_cancel)
	local lib = require("mare_lib")
	local llm = ctx.llm
	local tools = ctx:get("tools")
	local cancel = turn_cancel or side.cancel
	seed_calls_completed(side.messages)
	local content = mare.json.array({ { type = "text", text = project_value(text) } })
	for _, image in ipairs(images or {}) do
		content[#content + 1] = lib.copy_deep(image)
	end
	side.messages[#side.messages + 1] = { role = "user", content = content }
	local added, usage, answer = {}, {}, ""
	local final_message
	for _ = 1, 200 do
		if cancel:is_cancelled() or side.cancel:is_cancelled() then
			return { text = answer, error = "cancelled", messages = added, usage = usage, final_message = final_message }
		end
		local base_defs = tools and tools:definitions(side.model, side.tool_names) or {}
		local req = {
			model = side.model,
			params = side.params,
			system = side.system,
			messages = side.messages,
			tools = base_defs,
			purpose = "side",
			session_id = side.owner,
			cancel = cancel,
			no_fallback = true,
		}
		if lib.callable(llm.constrain_request) then
			req = llm:constrain_request(req)
		end
		local bound, wire = {}, {}
		if tools and lib.callable(tools.bind_declarations) then
			bound, wire = tools:bind_declarations(req.tools or {})
		end
		req.tools = wire
		local ok_stream, response = pcall(llm.stream, llm, req, function(ev)
			draw(ev)
		end)
		if not ok_stream then
			return {
				text = answer,
				error = (cancel:is_cancelled() or side.cancel:is_cancelled()) and "cancelled"
					or project_value(tostring(response)),
				messages = added,
				usage = usage,
				final_message = final_message,
			}
		end
		for k, v in pairs(response.usage or {}) do
			if type(v) == "number" then
				usage[k] = (usage[k] or 0) + v
			end
		end
		if cancel:is_cancelled() or side.cancel:is_cancelled() or response.stop_reason == "aborted" then
			return { text = answer, error = "cancelled", messages = added, usage = usage, final_message = final_message }
		end
		if response.stop_reason == "error" or response.error then
			return {
				text = answer,
				error = project_value(tostring(response.error or "side conversation request failed")),
				messages = added,
				usage = usage,
				final_message = final_message,
			}
		end
		local projected_resp = project_value(response)
		side.messages[#side.messages + 1] = projected_resp
		added[#added + 1] = projected_resp
		final_message = projected_resp
		local t = extract_text(projected_resp)
		if t ~= "" then
			answer = t
		end
		local calls = tool_calls_of(projected_resp)
		if #calls == 0 or response.stop_reason ~= "tool_use" then
			return {
				text = answer,
				messages = added,
				usage = usage,
				final_message = final_message,
			}
		end
		if not tools then
			return {
				text = answer,
				error = "tools service is unavailable",
				messages = added,
				usage = usage,
				final_message = final_message,
			}
		end
		for _, call in ipairs(calls) do
			if cancel:is_cancelled() or side.cancel:is_cancelled() then
				return {
					text = answer,
					error = "cancelled",
					messages = added,
					usage = usage,
					final_message = final_message,
				}
			end
			side.calls[call.id] = true
			draw({ type = "tool_start", call = { id = call.id, name = call.name } })
			local ok_exec, exec_res = pcall(tools.execute, tools, call, {
				lifetime = "ephemeral-read-only",
				cancel = cancel,
				run_cancel = cancel,
				declarations = bound,
				on_update = function(partial)
					draw({ type = "tool_output", call = call, partial = project_value(partial) })
				end,
			})
			side.calls[call.id] = nil
			local tool_res = ok_exec and exec_res or { content = "Tool error: " .. tostring(exec_res), is_error = true }
			tool_res = project_value(tool_res)
			draw({ type = "tool_result", call = call, result = tool_res })
			local msg_content = tool_res.content
			if type(msg_content) == "string" then
				msg_content = mare.json.array({ { type = "text", text = msg_content } })
			elseif type(msg_content) ~= "table" then
				msg_content = mare.json.array()
			end
			local tr = {
				role = "tool_result",
				tool_call_id = call.id,
				tool_name = call.name,
				content = msg_content,
				is_error = tool_res.is_error == true,
			}
			side.messages[#side.messages + 1] = tr
			added[#added + 1] = tr
		end
	end
	return {
		text = answer,
		error = "side conversation reached maximum tool steps",
		messages = added,
		usage = usage,
		final_message = final_message,
	}
end

return M
