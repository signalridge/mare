-- compact.stages.cut: cut points, turn starts and file tracking; part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
return function(D)
	local msg_tokens = D.msg_tokens

	-- ---- cut points ----------------------------------------------------------------------

	--- first kept index: keep about `keep` tokens verbatim; cut only at a user message (clean)
	--- or an assistant message (split turn), never at a tool result (it would orphan its call)
	local function cut_point(msgs, keep)
		local acc, cut = 0, nil
		for i = #msgs, 1, -1 do
			acc = acc + msg_tokens(msgs[i])
			local role = msgs[i].role
			if
				(role == "user" and not msgs[i].compaction and not msgs[i].preserved and not msgs[i].context_disclosure)
				or role == "assistant"
			then
				cut = i
			end
			if acc >= keep and cut then
				break
			end
		end
		return cut
	end
	local function turn_start(msgs, idx)
		for i = idx, 1, -1 do
			if
				msgs[i].role == "user"
				and not msgs[i].compaction
				and not msgs[i].preserved
				and not msgs[i].context_disclosure
			then
				return i
			end
		end
		return nil
	end

	--- cumulative file tracking from tool calls (+ lists carried by the previous summary)
	local function track_files(msgs, prev)
		local read, modified = {}, {}
		for _, p in ipairs(prev and prev.read_files or {}) do
			read[p] = true
		end
		for _, p in ipairs(prev and prev.modified_files or {}) do
			modified[p] = true
		end
		for _, m in ipairs(msgs) do
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" then
						local a = b.arguments or {}
						if b.name == "read" and a.path then
							read[a.path] = true
						elseif (b.name == "write" or b.name == "edit") and a.path then
							modified[a.path] = true
						elseif b.name == "apply_patch" and a.input then
							for _, path in string.gmatch(a.input, "%*%*%* (%a+) File: ([^\r\n]-)%s*\r?\n") do
								modified[path] = true
							end
							for path in string.gmatch(a.input, "%*%*%* Move to: ([^\r\n]-)%s*\r?\n") do
								modified[path] = true
							end
						end
					end
				end
			end
		end
		local function list(t, exclude)
			local out = {}
			for p in pairs(t) do
				if not (exclude and exclude[p]) then
					out[#out + 1] = p
				end
			end
			table.sort(out)
			return out
		end
		return { read_files = list(read, modified), modified_files = list(modified) }
	end

	return { cut_point = cut_point, turn_start = turn_start, track_files = track_files }
end
