-- llm.history: history conversion on a model / provider switch (transform_messages);
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
-- ---- history conversion on a mid-session model / provider switch ---------------
-- runs on a copy of the history for every request; the session keeps the original.

local lib = require("mare_lib")

local function sanitize_id(id, max)
	local s = string.gsub(tostring(id or ""), "[^%w_%-]", "_")
	if max and #s > max then
		s = s:sub(1, max)
	end
	return s
end

--- ids valid and unused keep their value; invalid or repeated raw ids across/within turns are
--- sanitized and made unique (`_2`, `_3`, …), pairing each `tool_result` with its call.
local function assign_call_ids(messages, policy)
	local used, pending, last_id = {}, {}, {}
	local call_map, result_map = {}, {}
	local function alloc_unique(id)
		local n = sanitize_id(id, policy.max)
		if n ~= "" and not used[n] then
			used[n] = true
			return n
		end
		local base = n ~= "" and n or "tool_call"
		local cand, i = base, 1
		while used[cand] do
			i = i + 1
			local suffix = "_" .. i
			cand = (policy.max and base:sub(1, policy.max - #suffix) or base) .. suffix
		end
		used[cand] = true
		return cand
	end
	for _, m in ipairs(messages) do
		if m.role == "assistant" then
			for _, b in ipairs(m.content or {}) do
				if b.type == "tool_call" and not b.hosted then
					local uid = alloc_unique(b.id)
					call_map[b] = uid
					local raw_key = b.id or ""
					local q = pending[raw_key]
					if not q then
						q = {}
						pending[raw_key] = q
					end
					q[#q + 1] = uid
					last_id[raw_key] = uid
				end
			end
		elseif m.role == "tool_result" then
			local raw_key = m.tool_call_id or ""
			local q = pending[raw_key]
			local uid = (q and #q > 0 and table.remove(q, 1)) or last_id[raw_key] or alloc_unique(m.tool_call_id)
			result_map[m] = uid
		end
	end
	return call_map, result_map
end

local function args_text(a)
	if type(a) ~= "table" then
		return tostring(a or "")
	end
	local ok, s = pcall(mare.json.encode, a)
	return ok and s or ""
end

--- transform_messages(messages, model, tools, id_policy, route?, default_route?) -> converted copy
---   - same model (`provider_id`, `model_id`, `route`): keep `native` wire metadata and blocks verbatim
---   - cross-model / cross-route: strip `native` from the assistant message and every block,
---     convert non-redacted `reasoning` to plain `text` (drop empty/redacted), convert dotted
---     extension blocks (`<wire>.<name>`) to plain `text` when `b.text` is non-empty else drop
---   - tool call ids normalized for the target provider (calls and results together)
---   - calls to tools that are no longer offered → "called X(args)" text, and their results → text
---   - images → "Attachment: <path>" text when the model takes no images
local function strip_native(b)
	if type(b) ~= "table" or b.native == nil then
		return b
	end
	local c = {}
	for k, v in pairs(b) do
		if k ~= "native" then
			c[k] = v
		end
	end
	return c
end

local function transform_messages(messages, model, tools, id_policy, route, default_route, wire, force_no_images)
	local offered
	if tools then
		offered = {}
		for _, t in ipairs(tools) do
			-- hosted/server tools (`{ type = "hosted", name = "web_search" }`) are executed by the
			-- provider, never by the client loop, so they don't make a client function call "offered"
			if t.type ~= "hosted" and t.name ~= nil then
				offered[t.name] = true
			end
		end
	end
	local no_images = lib.model_images(model) == false or force_no_images == true
	local call_map, result_map = assign_call_ids(messages, id_policy or {})
	local textified = {} -- unique tool call ids turned into text
	local function image_text(b)
		return {
			type = "text",
			text = b.path and ("Attachment: " .. b.path) or "(image omitted: model does not support images)",
		}
	end
	local out = {}
	local deferred_text_results = {}
	local function flush_deferred_text_results()
		for i = 1, #deferred_text_results do
			out[#out + 1] = deferred_text_results[i]
		end
		deferred_text_results = {}
	end
	for _, m in ipairs(messages) do
		if m.role ~= "tool_result" and #deferred_text_results > 0 then
			flush_deferred_text_results()
		end
		if m.role == "assistant" then
			local same = (m.provider_id == nil or m.provider_id == model.provider)
				and (m.model_id == nil or m.model_id == model.id)
				and (route == nil or (m.route or default_route) == route)
			local content = mare.json.array()
			for _, b in ipairs(m.content or {}) do
				if b.type == "reasoning" then
					if same then
						content[#content + 1] = b
					elseif not b.redacted and type(b.text) == "string" and b.text:match("%S") then
						content[#content + 1] = { type = "text", text = b.text }
					end
				elseif b.type == "tool_call" then
					if b.hosted then
						if same then
							content[#content + 1] = b
						end
					else
						local mapped_id = call_map[b] or b.id
						if offered and not offered[b.name] then
							textified[mapped_id] = b.name
							content[#content + 1] = {
								type = "text",
								text = ("[called tool %s(%s) — no longer available]"):format(
									b.name,
									args_text(b.arguments)
								),
							}
						else
							local c = {}
							for k, v in pairs(b) do
								if same or k ~= "native" then
									c[k] = v
								end
							end
							c.id = mapped_id
							content[#content + 1] = c
						end
					end
				elseif type(b.type) == "string" and b.type:find(".", 1, true) then
					local block_wire = b.type:match("^([^.]+)%.")
					local same_wire = wire == nil or block_wire == nil or block_wire == wire
					if same and same_wire then
						content[#content + 1] = b
					elseif type(b.text) == "string" and b.text:match("%S") then
						content[#content + 1] = { type = "text", text = b.text }
					end
				else
					content[#content + 1] = same and b or strip_native(b)
				end
			end
			local c = {}
			for k, v in pairs(m) do
				if same or k ~= "native" then
					c[k] = v
				end
			end
			c.content = content
			out[#out + 1] = c
		elseif m.role == "tool_result" then
			local mapped_id = result_map[m] or m.tool_call_id
			local name = textified[mapped_id]
			if name then
				local parts = {}
				for _, b in ipairs(m.content or {}) do
					if b.type == "text" then
						parts[#parts + 1] = b.text
					elseif b.type == "image" then
						parts[#parts + 1] = image_text(b).text
					end
				end
				deferred_text_results[#deferred_text_results + 1] = {
					role = "user",
					content = {
						{ type = "text", text = ("[result of %s]\n%s"):format(name, table.concat(parts, "\n")) },
					},
				}
			else
				local c = {}
				for k, v in pairs(m) do
					c[k] = v
				end
				c.tool_call_id = mapped_id
				if no_images then
					local content = mare.json.array()
					for _, b in ipairs(m.content or {}) do
						content[#content + 1] = b.type == "image" and image_text(b) or b
					end
					c.content = content
				end
				local has_content = false
				for _, b in ipairs(c.content or {}) do
					if (b.type == "text" and b.text and b.text:match("%S")) or b.type == "image" then
						has_content = true
						break
					end
				end
				if not has_content then
					c.content = { { type = "text", text = "(no tool output)" } }
				end
				out[#out + 1] = c
			end
		elseif m.role == "custom" then
			local content = {}
			if type(m.content) == "string" then
				content[1] = { type = "text", text = m.content }
			else
				for _, b in ipairs(m.content or {}) do
					content[#content + 1] = (no_images and b.type == "image") and image_text(b) or b
				end
			end
			if #content == 0 then
				content[1] = { type = "text", text = "" }
			end
			out[#out + 1] = { role = "user", content = content, timestamp = m.timestamp }
		elseif m.role == "user" and no_images then
			local content = mare.json.array()
			for _, b in ipairs(m.content or {}) do
				content[#content + 1] = b.type == "image" and image_text(b) or b
			end
			local c = {}
			for k, v in pairs(m) do
				c[k] = v
			end
			c.content = content
			out[#out + 1] = c
		else
			out[#out + 1] = m
		end
	end
	if #deferred_text_results > 0 then
		flush_deferred_text_results()
	end
	return out
end

return { transform_messages = transform_messages }
