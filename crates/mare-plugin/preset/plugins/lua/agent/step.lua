-- Generic model-step participants. prepare stages JSON data only; one cancel-aware journal
-- record commits all participants. Arbitrary callback side effects are not transactional.
return function(D)
	local ctx = D.ctx
	-- (on mare.Ctx `policy` is still typed as the transition layer's method)
	local policy = ctx.policy --[[@as mare.PolicyService]]
	local participants = {}
	local reserved
	ctx:on("turn/completed", function()
		reserved = nil
	end)
	local function copy(value)
		return mare.activity.capture(function()
			return value
		end)
	end
	local function snapshot()
		local session = D.follow.session()
		local records = session and require("mare_lib").all_records(session, "agent/step-state") or {}
		local record = records[#records]
		return record and record.states or {}
	end
	local M = {}
	function M.register(caller, name, prepare, opts)
		if type(name) ~= "string" or not name:match("^[%w][%w_.-]*$") or type(prepare) ~= "function" then
			error("step participant requires a name and prepare function", 0)
		end
		if participants[name] then
			error("step participant already registered: " .. name, 0)
		end
		opts = opts or {}
		if type(opts) ~= "table" then
			error("step participant options must be a table", 0)
		end
		for key in pairs(opts) do
			if key ~= "failure_policy" and key ~= "output" then
				error("unknown step participant option: " .. tostring(key), 0)
			end
		end
		local failure = opts.failure_policy or "required"
		if failure ~= "required" and failure ~= "defer" then
			error("invalid step failure_policy", 0)
		end
		local output = opts.output or "state"
		if output ~= "state" and output ~= "transaction" then
			error("invalid step participant output", 0)
		end
		local participant = { prepare = prepare, failure = failure, output = output }
		participants[name] = participant
		local function remove()
			if participants[name] == participant then
				participants[name] = nil
			end
		end
		caller:effect(function()
			return remove
		end, "agent step participant " .. name)
		return remove
	end
	-- Disclosure content: a non-empty list of text blocks (copied).
	local function text_content(content)
		if type(content) ~= "table" or #content < 1 or #content > 64 then
			error("disclosure content must be 1..64 text blocks")
		end
		local out = {}
		for i, block in ipairs(content) do
			if type(block) ~= "table" or block.type ~= "text" or type(block.text) ~= "string" then
				error("disclosure content must be text blocks")
			end
			if #block.text > 65536 then
				error("invalid disclosure text")
			end
			out[i] = { type = "text", text = block.text }
		end
		return out
	end
	local function visible_state(name, messages)
		messages = messages or D.A.messages
		for i = #messages, 1, -1 do
			local disclosure = messages[i].context_disclosure
			if disclosure and disclosure.namespace == name then
				return disclosure.state
			end
		end
	end
	function M.state(name)
		local participant = participants[name]
		local value = participant and participant.output == "transaction" and visible_state(name) or nil
		if not participant or participant.output ~= "transaction" then
			value = snapshot()[name]
		end
		return value ~= nil and copy(value) or nil
	end
	function M.release(event)
		if reserved == event then
			reserved = nil
		end
	end
	function M.preview(name, step_id)
		if step_id == nil then
			return M.state(name)
		end
		if type(step_id) ~= "string" or not reserved or reserved.id ~= step_id then
			return nil, "inactive step preview"
		end
		local candidate = reserved.candidates[name]
		if candidate and candidate.ok and candidate.value ~= nil then
			return copy(candidate.value)
		end
		return M.state(name)
	end
	function M.begin(step, cancel, turn_id, on_admit)
		if cancel:is_cancelled() then
			return nil, "aborted", "cancelled"
		end
		local event = { id = mare.crypto.random_hex(12), step = step, cancel = cancel, turn_id = turn_id }
		local verdict = policy:decide("agent/step-ready", { event })
		if not verdict or verdict.decision ~= "allow" then
			return nil, verdict and verdict.reason or "model step was not admitted"
		end
		if on_admit then
			local ok_a, err_a = on_admit()
			if not ok_a then
				return nil, err_a
			end
		end
		event.candidates = {}
		local states, names = snapshot(), {}
		for name, participant in pairs(participants) do
			if participant.output == "state" then
				names[#names + 1] = name
			end
		end
		table.sort(names)
		for _, name in ipairs(names) do
			if cancel:is_cancelled() then
				return nil, "aborted", "cancelled"
			end
			local participant = participants[name]
			if participant then
				local ok, value = pcall(function()
					return participant.prepare(copy(states[name]), {
						step_id = event.id,
						turn_id = turn_id,
						step = step,
						cancel = cancel,
						messages = copy(D.A.messages),
					})
				end)
				if cancel:is_cancelled() then
					return nil, "aborted", "cancelled"
				end
				if participants[name] == participant then
					if ok and value ~= nil then
						ok, value = pcall(copy, value)
					end
					if not ok and participant.failure == "required" then
						return nil, tostring(value)
					end
					event.candidates[name] = { ok = ok, value = value, participant = participant }
				end
			end
		end
		reserved = event
		return event
	end
	function M.accept(step, cancel, turn_id, visible_messages, event, on_commit, requested_tools)
		if not event then
			return nil, "model step reservation is missing"
		end
		if cancel:is_cancelled() then
			return nil, "aborted"
		end
		local tool_names
		if requested_tools ~= nil then
			tool_names = mare.json.array()
			for _, tool in ipairs(requested_tools) do
				if type(tool) == "string" then
					tool_names[#tool_names + 1] = tool
				end
			end
		end
		local states, names, changed, required = copy(snapshot()), {}, false, false
		local disclosures = {}
		for name in pairs(participants) do
			names[#names + 1] = name
		end
		table.sort(names)
		for _, name in ipairs(names) do
			if cancel:is_cancelled() then
				return nil, "aborted"
			end
			local participant = participants[name]
			if participant then
				local baseline = states[name]
				if participant.output == "transaction" then
					baseline = visible_state(name, visible_messages)
				end
				local ok, value
				if participant.output == "state" then
					local candidate = event.candidates[name]
					if candidate and candidate.participant == participant then
						ok, value = candidate.ok, candidate.value
					else
						ok, value = true, nil
					end
				else
					ok, value = pcall(function()
						return participant.prepare(copy(baseline), {
							step_id = event.id,
							turn_id = turn_id,
							step = step,
							cancel = cancel,
							messages = copy(visible_messages or D.A.messages),
							tools = tool_names and copy(tool_names) or nil,
						})
					end)
					if cancel:is_cancelled() then
						return nil, "aborted"
					end
					if participants[name] ~= participant then
						ok, value = true, nil
					elseif ok and value ~= nil then
						ok, value = pcall(copy, value)
					end
				end
				if not ok then
					if participant.failure == "required" then
						return nil, tostring(value)
					end
					mare.log("warn", "step participant " .. name .. " deferred: " .. tostring(value))
				elseif value ~= nil then
					if participant.output == "transaction" then
						local valid, candidate = pcall(function()
							if type(value) ~= "table" then
								error("transaction output must be an object")
							end
							for key in pairs(value) do
								if key ~= "state" and key ~= "disclosures" then
									error("unknown transaction field")
								end
							end
							if
								value.state == nil
								or type(value.disclosures) ~= "table"
								or #value.disclosures < 1
								or #value.disclosures > 16
							then
								error("transaction requires state and 1..16 disclosures")
							end
							local staged = {}
							for _, item in ipairs(value.disclosures) do
								for key in pairs(item) do
									if key ~= "source" and key ~= "text" and key ~= "content" and key ~= "metadata" then
										error("unknown disclosure field")
									end
								end
								if type(item.source) ~= "string" or #item.source < 1 or #item.source > 128 then
									error("disclosure requires bounded source")
								end
								if (item.text == nil) == (item.content == nil) then
									error("disclosure requires text or content")
								end
								local content = item.content
								if item.text ~= nil then
									if type(item.text) ~= "string" or #item.text > 65536 then
										error("invalid disclosure text")
									end
									content = { { type = "text", text = item.text } }
								end
								content = text_content(content)
								staged[#staged + 1] = {
									role = "user",
									notification = true,
									turn_id = turn_id,
									input_source = item.source,
									content = content,
									timestamp = mare.task.now(),
									context_disclosure = {
										namespace = name,
										source = item.source,
										metadata = item.metadata,
										state = value.state,
										step_id = event.id,
									},
								}
							end
							return staged
						end)
						if not valid then
							if participant.failure == "required" then
								return nil, tostring(candidate)
							end
							mare.log("warn", "step transaction deferred: " .. tostring(candidate))
						else
							for _, message in ipairs(candidate) do
								disclosures[#disclosures + 1] = message
							end
							states[name], changed = value.state, true
							required = required or participant.failure == "required"
						end
					else
						states[name], changed = value, true
						required = required or participant.failure == "required"
					end
				end
			end
		end
		if cancel:is_cancelled() then
			return nil, "aborted"
		end
		if changed then
			local session = D.follow.session()
			if not session then
				return nil, "session service is unavailable for step state"
			end
			local info = session:info()
			local persistent = not info.ephemeral and not info.readonly and mare.mode ~= "child"
			local ok, err = session:commit_step(
				{
					version = 1,
					step_id = event.id,
					turn_id = turn_id,
					step = step,
					states = states,
				},
				disclosures,
				{
					persist = persistent,
					cancel = cancel,
					session_id = info.session_id,
					session_epoch = info.session_epoch,
				}
			)
			if not ok then
				if cancel:is_cancelled() then
					return nil, "aborted"
				end
				if required then
					return nil, tostring(err)
				end
				mare.log("warn", "step state deferred: " .. tostring(err))
				disclosures = {}
			else
				if on_commit then
					on_commit(disclosures)
				end
				for _, message in ipairs(disclosures) do
					local entry_id = session:entry_id(message)
					local item_id = entry_id or mare.crypto.random_hex(8)
					local item_type = (message.role == "user" and "user_message")
						or (message.role == "assistant" and "assistant_message")
						or "custom"
					local item = {
						type = item_type,
						entry_id = entry_id,
						message = copy(message),
					}
					ctx.bus:emit("item/started", { turn_id = turn_id, item_id = item_id, item = item })
					ctx.bus:emit("item/completed", {
						turn_id = turn_id,
						item_id = item_id,
						status = "completed",
						item = item,
					})
				end
				-- This is an observation after the durable linearization point. Subscribers
				-- must query step_state; their errors cannot undo a committed transaction.
				ctx.bus:emit("agent/step-committed", { step_id = event.id, step = step, turn_id = turn_id })
			end
		end
		return true, nil, event.id, disclosures
	end
	return M
end
