-- faux: scripted provider for tests and offline demos.
-- Script via env MARE_FAUX_SCRIPT (path to a JSON array of assistant turns) or config
-- [plugins.provider-faux] script_json = "[...]". Each turn: {text=, reasoning=, tool_calls={{name=, arguments={}}},
-- error=, retryable=, kind=, delay_ms= (pause per 8-byte chunk), stop= (stop_reason override)}.
-- Without a script it echoes the last user message.
return {
	name = "provider-faux",
	-- it only registers on the llm service
	inject = { "llm" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	providers = { "faux" },
	config_schema = {
		script_json = { type = "string", desc = "Scripted replies as JSON." },
		script = { type = "string", desc = "Scripted replies." },
	},
	apply = function(ctx, cfg)
		if cfg.script ~= nil then
			error("provider-faux: script table configuration is not supported; use script_json or MARE_FAUX_SCRIPT")
		end
		local function decode_script(raw, source)
			if type(raw) ~= "string" then
				error("provider-faux: " .. source .. " must be a JSON string")
			end
			local value, err = mare.json.decode_lossless(raw)
			if type(value) ~= "table" or not mare.json.is_array(value) then
				error("provider-faux: " .. source .. " must contain a JSON array: " .. tostring(err or "invalid root"))
			end
			return value
		end
		local lib = require("mare_lib")
		local script = cfg.script_json ~= nil and decode_script(cfg.script_json, "script_json") or nil
		local script_path = mare.env("MARE_FAUX_SCRIPT")
		local active_script = lib.lazy(function()
			if script_path then
				local raw, read_err = mare.fs.read(script_path)
				if not raw then
					error(
						("provider-faux: cannot read MARE_FAUX_SCRIPT (%s): %s"):format(
							tostring(script_path),
							tostring(read_err or "read failed")
						),
						0
					)
				end
				return decode_script(raw, "MARE_FAUX_SCRIPT (" .. tostring(script_path) .. ")")
			end
			return script
		end)
		local turn = 0

		local function first_text(m)
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					return b.text
				end
			end
		end

		local function last_user_text(messages)
			for _, skip_disclosures in ipairs({ true, false }) do
				for i = #messages, 1, -1 do
					local m = messages[i]
					if m.role == "user" and (not skip_disclosures or not m.context_disclosure) then
						local text = first_text(m)
						if text then
							return text
						end
					end
				end
			end
			return ""
		end

		ctx.llm:register_provider({
			provider = "faux",
			default_model = "echo",
			is_configured = function()
				return mare.env("MARE_FAUX_SCRIPT") ~= nil or cfg.script_json ~= nil
			end,
			models = {
				{
					model_id = "echo",
					name = "Faux Echo",
					context_window = 100000,
					max_output = 4096,
					reasoning = true,
					efforts = { "low", "medium", "high" },
					cost = { input = 1, output = 2 },
				},
				-- a second model, for tests that switch models
				{ model_id = "echo2", name = "Faux Echo 2", context_window = 100000, max_output = 4096 },
			},
			stream = function(req, sink)
				local cur_script = active_script()
				turn = turn + 1
				local step = cur_script and cur_script[turn] or { text = "echo: " .. last_user_text(req.messages) }
				local content = {}
				if step.error then
					error({ message = step.error, retryable = step.retryable, kind = step.kind })
				end
				if step.reasoning then
					sink({ type = "reasoning_delta", delta = step.reasoning })
					table.insert(content, { type = "reasoning", text = step.reasoning })
				end
				if step.text then
					-- stream in small chunks
					local i = 1
					while i <= #step.text do
						-- ~8 bytes, never splitting a UTF-8 character (deltas are valid UTF-8, like
						-- real providers'): end before a continuation byte (0x80–0xBF)
						local j = math.min(i + 7, #step.text)
						while
							j < #step.text
							and (string.byte(step.text, j + 1) or 0) >= 0x80
							and (string.byte(step.text, j + 1) or 0) < 0xC0
						do
							j = j + 1
						end
						local chunk = string.sub(step.text, i, j)
						sink({ type = "text_delta", delta = chunk })
						i = j + 1
						if step.delay_ms then -- tests: a slow stream (abort / steer mid-turn)
							mare.task.sleep(step.delay_ms, req.cancel)
						end
						if req.cancel and req.cancel:is_cancelled() then
							error("aborted")
						end
					end
					table.insert(content, { type = "text", text = step.text })
				end
				for n, tc in ipairs(step.tool_calls or {}) do
					local id = "call_" .. turn .. "_" .. n
					sink({ type = "tool_call_start", tool_call_id = id, name = tc.name })
					table.insert(content, {
						type = "tool_call",
						id = id,
						name = tc.name,
						arguments = tc.arguments,
						parse_error = type(tc.arguments) ~= "table" and "arguments are not a JSON object" or nil,
					})
				end
				local input_tokens = 0
				for _, m in ipairs(req.messages) do
					input_tokens = input_tokens + mare.text.tokens(m)
				end
				return {
					role = "assistant",
					content = content,
					stop_reason = step.stop or ((#(step.tool_calls or {}) > 0) and "tool_use" or "stop"),
					usage = {
						input = input_tokens,
						output = mare.text.tokens(step.text or ""),
						cache_read = 0,
						cache_write = 0,
					},
				}
			end,
		})
	end,
}
