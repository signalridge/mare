-- Generic deferred-tool discovery and owner-bound search sources.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local tools, excluded = D.registry.tools, D.registry.excluded
	local native_tool_update = D.registry.native_tool_update
	local sources, sequence = {}, 0

	local MAX_SELECT_TOOLS = 32
	local RESULT_TOKEN_BUDGET = 6000
	local RESULT_BYTE_BUDGET = RESULT_TOKEN_BUDGET * 4
	local MIN_DESC_CHARS = 120
	local MAX_OMITTED_NAMES_BYTES = 1024

	local function estimate_tokens(s)
		return math.ceil(#s / 4)
	end

	local function truncate_desc(desc, max_chars)
		if #desc <= max_chars then
			return desc
		end
		if max_chars <= 3 then
			return desc:sub(1, max_chars)
		end
		return mare.text.trunc_width(desc, max_chars - 3) .. "..."
	end

	local function resolve_schema_json(def, model, mode)
		local params = def.parameters
		if type(params) == "function" then
			local ok, resolved = pcall(params, model, mode)
			params = ok and resolved or {}
		end
		if type(params) ~= "table" then
			params = {}
		end
		return mare.json.encode(params)
	end

	local function format_omitted_list(entries, start_idx, max_bytes)
		local total = #entries - start_idx + 1
		if total <= 0 then
			return ""
		end
		local budget = math.max(32, max_bytes or MAX_OMITTED_NAMES_BYTES)
		local shown = {}
		local used = 0
		for i = start_idx, #entries do
			local name = entries[i].name
			local remaining_after = #entries - i
			local sep = #shown > 0 and ", " or ""
			local suffix = remaining_after > 0 and (", ... (+" .. tostring(remaining_after) .. " more)") or ""
			if used + #sep + #name + #suffix <= budget then
				shown[#shown + 1] = name
				used = used + #sep + #name
			else
				if #shown == 0 then
					local reserve = remaining_after > 0 and (#suffix + 3) or 3
					local cap = math.max(1, budget - reserve)
					shown[1] = mare.text.trunc_width(name, cap) .. "..."
					if remaining_after > 0 then
						return shown[1] .. suffix
					end
					return shown[1]
				end
				return table.concat(shown, ", ") .. ", ... (+" .. tostring(#entries - i + 1) .. " more)"
			end
		end
		return table.concat(shown, ", ")
	end

	local function render_entries(entries, count, desc_cap, footer_notes)
		local lines
		if count == 0 then
			local first = entries[1]
			local first_name = first and mare.text.trunc_width(first.name, 128) or "unknown"
			local first_bytes = first and #first.schema_json or 0
			lines = {
				("No tool schemas loaded: %s alone exceeds the %d-token (%d-byte) search result budget (schema is %d bytes)."):format(
					first_name,
					RESULT_TOKEN_BUDGET,
					RESULT_BYTE_BUDGET,
					first_bytes
				),
			}
		else
			lines = { "Loaded tools:" }
			for i = 1, count do
				local e = entries[i]
				local desc = desc_cap and truncate_desc(e.desc, desc_cap) or e.desc
				if desc ~= "" then
					lines[#lines + 1] = ("- %s: %s"):format(e.name, desc)
				else
					lines[#lines + 1] = ("- %s"):format(e.name)
				end
				lines[#lines + 1] = "  parameters: " .. e.schema_json
			end
		end
		if count < #entries then
			local prefix = count == 0 and "Matching tools (schemas omitted to fit budget): "
				or "Additional matching tools (omitted to fit budget — query with select:<name> to load their schemas): "
			local footer_bytes = 0
			for _, note in ipairs(footer_notes) do
				footer_bytes = footer_bytes + #note + 1
			end
			local used_bytes = #table.concat(lines, "\n") + 1 + #prefix + footer_bytes
			local omitted_cap = math.min(MAX_OMITTED_NAMES_BYTES, math.max(32, RESULT_BYTE_BUDGET - used_bytes))
			lines[#lines + 1] = prefix .. format_omitted_list(entries, count + 1, omitted_cap)
		end
		for _, note in ipairs(footer_notes) do
			lines[#lines + 1] = note
		end
		local rendered = table.concat(lines, "\n")
		if count == 0 and #rendered > RESULT_BYTE_BUDGET then
			rendered = mare.text.trunc_width(rendered, RESULT_BYTE_BUDGET - 3) .. "..."
		end
		return rendered
	end

	local function format_hits(hits, model, footer_notes)
		local mode = tools.edit_mode(tools, ctx, model)
		local entries = {}
		local max_desc = 0
		for i, def in ipairs(hits) do
			local desc = type(def.description) == "string" and def.description or ""
			if #desc > max_desc then
				max_desc = #desc
			end
			entries[i] = {
				name = def.name,
				desc = desc,
				schema_json = resolve_schema_json(def, model, mode),
			}
		end

		local text = render_entries(entries, #entries, nil, footer_notes)
		if estimate_tokens(text) <= RESULT_TOKEN_BUDGET then
			local names = {}
			for i, e in ipairs(entries) do
				names[i] = e.name
			end
			return text, names
		end

		-- Pass 2: progressively shorten descriptions (down to MIN_DESC_CHARS) while keeping all schemas.
		if max_desc > MIN_DESC_CHARS then
			local candidate_min = render_entries(entries, #entries, MIN_DESC_CHARS, footer_notes)
			if estimate_tokens(candidate_min) <= RESULT_TOKEN_BUDGET then
				local lo, hi = MIN_DESC_CHARS, max_desc
				local best_text = candidate_min
				while lo <= hi do
					local mid = math.floor((lo + hi) / 2)
					local cand = render_entries(entries, #entries, mid, footer_notes)
					if estimate_tokens(cand) <= RESULT_TOKEN_BUDGET then
						best_text = cand
						lo = mid + 1
					else
						hi = mid - 1
					end
				end
				local names = {}
				for i, e in ipairs(entries) do
					names[i] = e.name
				end
				return best_text, names
			end
		end

		-- Pass 3: keep full schema for leading K hits within budget (K may be 0 if even the
		-- leading schema exceeds budget); list remaining hits by name only.
		local keep = 0
		local lo, hi = 1, #entries
		while lo <= hi do
			local mid = math.floor((lo + hi) / 2)
			local cand = render_entries(entries, mid, MIN_DESC_CHARS, footer_notes)
			if estimate_tokens(cand) <= RESULT_TOKEN_BUDGET then
				keep = mid
				lo = mid + 1
			else
				hi = mid - 1
			end
		end
		local names = mare.json.array()
		for i = 1, keep do
			names[i] = entries[i].name
		end
		return render_entries(entries, keep, MIN_DESC_CHARS, footer_notes), names
	end

	function tools.register_search_source(_, caller, def)
		assert(type(def) == "table" and type(def.prepare) == "function", "search source requires prepare")
		sequence = sequence + 1
		local id = sequence
		sources[id] = { prepare = def.prepare, prefixes = def.prefixes }
		local function remove()
			if sources[id] then
				sources[id] = nil
				tools:invalidate(ctx)
			end
		end
		remove = require("mare_lib").owned(caller, remove, "tool search source")
		tools:invalidate(ctx)
		return remove
	end

	function tools.discovery_prefixes(_, _)
		local seen, out = {}, mare.json.array()
		for i = 1, sequence do
			local src = sources[i]
			local raw = src and src.prefixes
			if type(raw) == "function" then
				local ok, list = pcall(raw)
				raw = ok and list or nil
			end
			if type(raw) == "table" then
				for _, p in ipairs(raw) do
					if type(p) == "string" and p ~= "" and not seen[p] then
						seen[p] = true
						out[#out + 1] = p
					end
				end
			end
		end
		table.sort(out)
		return out
	end

	local function catalog()
		local out = {}
		for _, def in ipairs(tools.list(tools, ctx)) do
			-- a tool the user excluded is never declared, so it is never reported as loaded
			if def.exposure == "deferred" and not excluded(def.name) then
				out[#out + 1] = def
			end
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return out
	end

	local function has_deferred()
		return next(sources) ~= nil or #catalog() > 0
	end

	tools.register(tools, ctx, "tool_search", {
		snippet = "Discover and load deferred tools before calling them",
		description = function(model)
			if native_tool_update(model) then
				return "Find deferred tools and load them. Query with keywords "
					.. '(e.g. "github issues"), or "select:<tool>,<tool>" for exact names. '
					.. "Loaded tools can be called directly from your next step. "
					.. "Batch multiple exact names in one select:a,b query when known."
			end
			return "Find deferred tools and inspect their input schemas. Query with keywords "
				.. '(e.g. "github issues"), or "select:<tool>,<tool>" for exact names. '
				.. "Invoke discovered tools via call_tool. "
				.. "Batch multiple exact names in one select:a,b query when known."
		end,
		parameters = {
			type = "object",
			properties = {
				query = { type = "string", description = 'Keywords, or "select:name1,name2"' },
				max_results = { type = "integer", description = "Maximum tools to load (default 5)" },
			},
			required = { "query" },
		},
		side_effect = "read",
		enabled = function()
			return has_deferred()
		end,
		summary = function(a)
			return tostring(a.query or "")
		end,
		execute = function(a, tctx)
			for i = 1, sequence do
				local src = sources[i]
				local prepare = src and src.prepare
				if prepare then
					local ok, err = pcall(prepare)
					if not ok then
						mare.log("warn", "tool search source: " .. tostring(err))
					end
				end
			end
			local q = tostring(a.query or "")
			local limit = tonumber(a.max_results) or cfg.search_results or 5
			local all, hits, footer_notes = catalog(), {}, {}
			local sel = q:match("^%s*select:(.+)$")
			if sel then
				local seen = {}
				local total_matched = 0
				for want in sel:gmatch("[^,%s]+") do
					for _, def in ipairs(all) do
						local aliases = def.search and def.search.aliases or {}
						if not seen[def.name] and (def.name == want or table.find(aliases, want)) then
							seen[def.name] = true
							total_matched = total_matched + 1
							if #hits < MAX_SELECT_TOOLS then
								hits[#hits + 1] = def
							end
						end
					end
				end
				if total_matched > MAX_SELECT_TOOLS then
					footer_notes[#footer_notes + 1] = ("Note: select: returned the first %d of %d matching tools; query the remaining names in another tool_search call."):format(
						MAX_SELECT_TOOLS,
						total_matched
					)
				end
			else
				local words = {}
				for w in q:lower():gmatch("[%w_%-]+") do
					words[#words + 1] = w
				end
				local scored = {}
				for _, def in ipairs(all) do
					local search = def.search or {}
					local name = (def.name .. " " .. table.concat(search.aliases or {}, " ")):lower()
					local description = type(def.description) == "string" and def.description or ""
					local desc = (description .. " " .. (search.text or "")):lower()
					local score = 0
					for _, w in ipairs(words) do
						if name:find(w, 1, true) then
							score = score + 2
						end
						if desc:find(w, 1, true) then
							score = score + 1
						end
					end
					if score > 0 then
						scored[#scored + 1] = { def = def, score = score }
					end
				end
				table.sort(scored, function(x, y)
					return x.score > y.score or (x.score == y.score and x.def.name < y.def.name)
				end)
				for i = 1, math.min(limit, #scored) do
					hits[#hits + 1] = scored[i].def
				end
			end
			if #hits == 0 then
				return { content = ("No tools match %q (%d deferred tools known)."):format(q, #all) }
			end
			local llm = D.svc.llm
			local model = (tctx and tctx.model) or (llm and llm:current()) or nil
			local content, names = format_hits(hits, model, footer_notes)
			if #names > 0 then
				ctx.bus:emit("tools/loaded", { tools = names })
			end
			return {
				content = content,
				details = { loaded_tools = names },
			}
		end,
	})

	return {
		has_deferred = has_deferred,
	}
end
