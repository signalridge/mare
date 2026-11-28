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

	--- cumulative file tracking from tool calls (+ lists carried by the previous summary),
	--- preserving recency order (oldest to newest) so rendered checkpoints can favour recent paths
	--- while retaining the complete lists in the compaction details record.
	local function track_files(msgs, prev)
		local read, modified = {}, {}
		local seq = 0
		local function touch(t, path)
			if type(path) == "string" and path ~= "" then
				seq = seq + 1
				t[path] = seq
			end
		end
		for _, p in ipairs(prev and prev.read_files or {}) do
			touch(read, p)
		end
		for _, p in ipairs(prev and prev.modified_files or {}) do
			touch(modified, p)
		end
		for _, m in ipairs(msgs) do
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" then
						local a = b.arguments or {}
						if b.name == "read" and a.path then
							touch(read, a.path)
						elseif (b.name == "write" or b.name == "edit") and a.path then
							touch(modified, a.path)
						elseif b.name == "apply_patch" and a.input then
							for _, path in string.gmatch(a.input, "%*%*%* (%a+) File: ([^\r\n]-)%s*\r?\n") do
								touch(modified, path)
							end
							for path in string.gmatch(a.input, "%*%*%* Move to: ([^\r\n]-)%s*\r?\n") do
								touch(modified, path)
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
			table.sort(out, function(a, b)
				if t[a] ~= t[b] then
					return t[a] < t[b]
				end
				return a < b
			end)
			return out
		end
		return { read_files = list(read, modified), modified_files = list(modified) }
	end

	return { cut_point = cut_point, turn_start = turn_start, track_files = track_files }
end
