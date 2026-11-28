-- llm.mdev: models.dev reasoning_options derivation, normalised metadata lookup,
-- cross-provider consensus fallback, and fetch failure backoff.
--
-- Adapted from magpie (https://github.com/yetone/magpie, MIT License, Copyright (c) 2026 yetone):
--   internal/catalog/samemodel.go, internal/catalog/catalog.go, internal/catalog/fresh.go
-- and opencode (https://github.com/anomalyco/opencode, MIT License):
--   packages/core/src/models-dev/v1.ts.
local M = {}

local MIN_CONTEXT_WINDOW = 2048
M.MIN_CONTEXT_WINDOW = MIN_CONTEXT_WINDOW

local RETRY_FIRST_MS = 2 * 60 * 1000
local RETRY_MAX_MS = 60 * 60 * 1000
M.RETRY_FIRST_MS = RETRY_FIRST_MS
M.RETRY_MAX_MS = RETRY_MAX_MS

function M.next_retry_ms(prev_ms)
	local prev = tonumber(prev_ms) or 0
	if prev <= 0 then
		return RETRY_FIRST_MS
	end
	return math.min(prev * 2, RETRY_MAX_MS)
end

function M.valid_window(n)
	n = tonumber(n)
	return (n and n >= MIN_CONTEXT_WINDOW) and n or nil
end

function M.window_of_limit(lim)
	if type(lim) ~= "table" then
		return nil
	end
	local lim_ctx = M.valid_window(lim.context)
	local lim_in = M.valid_window(lim.input)
	if lim_in and (not lim_ctx or lim_in <= lim_ctx) then
		return lim_in
	end
	return lim_ctx
end

function M.bare_id(id)
	if type(id) ~= "string" then
		return ""
	end
	local s = id:match("^%s*(.-)%s*$") or ""
	local tail = s:match("([^/]+)$")
	return tail or s
end

local function is_snapshot_date(s)
	if type(s) ~= "string" or not s:match("^%d+$") then
		return false
	end
	if #s == 8 and s:sub(1, 2) == "20" then
		return true
	end
	if #s == 6 then
		local yy = tonumber(s:sub(1, 2))
		local mm = tonumber(s:sub(3, 4))
		local dd = tonumber(s:sub(5, 6))
		return yy >= 23 and yy <= 39 and mm >= 1 and mm <= 12 and dd >= 1 and dd <= 31
	end
	return false
end

--- Normalised same-model key for metadata matching only (never model resolution):
--- lowercase, strip prefix up to last '/', collapse spaces/'_'/'-' runs into '-',
--- turn version '.' or 'p' between digits into '-', and strip trailing YYYYMMDD
--- or YYMMDD snapshot date after '-' or '@'.
function M.same_model(id)
	if type(id) ~= "string" then
		return ""
	end
	local k = (id:match("^%s*(.-)%s*$") or ""):lower()
	local tail = k:match("([^/]+)$")
	if tail then
		k = tail
	end
	k = k:gsub("[%s_%-]+", "-")
	k = k:gsub("^%-+", ""):gsub("%-+$", "")
	while true do
		local next_k, n = k:gsub("(%d)[%.p](%d)", "%1-%2")
		k = next_k
		if n == 0 then
			break
		end
	end
	local prefix, sep_tail = k:match("^(.*)[%-@]([^%-@]+)$")
	if prefix and prefix ~= "" and is_snapshot_date(sep_tail) then
		k = prefix
	end
	return k
end

--- Extract effort levels from a models.dev entry's `reasoning_options`.
--- Maps `"none"` -> `"off"`. Adds `"off"` from a `"toggle"` option only when
--- `allow_toggle_off` is true (verified provider declaration).
--- Returns `efforts_or_nil, is_budgeted`.
function M.efforts_from_entry(e, allow_toggle_off)
	if type(e) ~= "table" or type(e.reasoning_options) ~= "table" then
		return nil, false
	end
	local out
	local seen = {}
	local toggle = false
	local budgeted = false
	for _, opt in ipairs(e.reasoning_options) do
		if type(opt) == "table" then
			local kind = opt.type
			if kind == "toggle" then
				toggle = true
			elseif kind == "budget_tokens" then
				budgeted = true
			elseif kind == "effort" and type(opt.values) == "table" then
				out = {}
				seen = {}
				for _, raw in ipairs(opt.values) do
					if type(raw) == "string" and raw ~= "" and raw ~= "null" then
						local v = raw == "none" and "off" or raw
						if not seen[v] then
							seen[v] = true
							out[#out + 1] = v
						end
					end
				end
			end
		end
	end
	if not out or #out == 0 then
		return nil, budgeted
	end
	if allow_toggle_off and toggle and not seen.off then
		local with_off = { "off" }
		for i = 1, #out do
			with_off[i + 1] = out[i]
		end
		return with_off, budgeted
	end
	return out, budgeted
end

--- Return reasoning effort levels for `e` within provider table `md_prov`:
--- its own `reasoning_options` levels, else (for a thinking non-budgeted model
--- with a `family`) those of the newest same-family thinking model in `md_prov`
--- released on or before `e.release_date`.
function M.levels_for_entry(md_prov, entry_id, e, allow_toggle_off)
	if type(e) ~= "table" then
		return nil
	end
	local own, budgeted = M.efforts_from_entry(e, allow_toggle_off)
	if own then
		return own
	end
	if e.reasoning ~= true or budgeted or type(e.family) ~= "string" or e.family == "" or type(md_prov) ~= "table" then
		return nil
	end
	local rel = type(e.release_date) == "string" and e.release_date or ""
	local kin_id, kin_date, kin_efforts
	for xid, x in pairs(md_prov) do
		if type(x) == "table" and xid ~= entry_id and x.family == e.family and x.reasoning == true then
			local xrel = type(x.release_date) == "string" and x.release_date or ""
			if not (rel ~= "" and xrel > rel) then
				if not kin_efforts or xrel > kin_date or (xrel == kin_date and xid > kin_id) then
					local xe = M.efforts_from_entry(x, allow_toggle_off)
					if xe then
						kin_id, kin_date, kin_efforts = xid, xrel, xe
					end
				end
			end
		end
	end
	return kin_efforts
end

function M.is_hidden_signal(e)
	if type(e) ~= "table" then
		return false
	end
	if e.status == "deprecated" then
		return true
	end
	if type(e.type) == "string" and e.type ~= "" and e.type ~= "chat" then
		return true
	end
	if e.tool_call == false then
		return true
	end
	return false
end

local function most_counted_number(counts)
	local best_v, best_n
	for v, n in pairs(counts) do
		if not best_n or n > best_n or (n == best_n and v > best_v) then
			best_v, best_n = v, n
		end
	end
	return best_v
end

local function most_counted_string(counts)
	local best_s, best_n
	for s, n in pairs(counts) do
		if not best_n or n > best_n or (n == best_n and s < best_s) then
			best_s, best_n = s, n
		end
	end
	return best_s
end

local function canonical_input_key(inp)
	if type(inp) ~= "table" then
		return nil
	end
	local has_text, has_image = false, false
	for _, x in ipairs(inp) do
		if x == "text" then
			has_text = true
		elseif x == "image" then
			has_image = true
		end
	end
	if has_text and has_image then
		return "text,image"
	elseif has_text then
		return "text"
	elseif has_image then
		return "image"
	end
	return nil
end

local function split_csv(s)
	local out = {}
	for part in s:gmatch("[^,]+") do
		out[#out + 1] = part
	end
	return out
end

local index_cache = setmetatable({}, { __mode = "k" })

local function build_provider_index(pm)
	local bare = {}
	local same = {}
	local ids = {}
	for mid in pairs(pm) do
		if type(mid) == "string" then
			ids[#ids + 1] = mid
		end
	end
	table.sort(ids)
	for _, mid in ipairs(ids) do
		local e = pm[mid]
		if type(e) == "table" then
			local b = M.bare_id(mid)
			if b ~= "" and not bare[b] then
				bare[b] = { id = mid, entry = e }
			end
			local sk = M.same_model(mid)
			if sk ~= "" and not same[sk] then
				same[sk] = { id = mid, entry = e }
			end
		end
	end
	return { exact = pm, bare = bare, same = same }
end

local function vote_single(bucket, bare, val)
	if val == nil then
		return
	end
	bucket[bare] = bucket[bare] or {}
	bucket[bare][val] = (bucket[bare][val] or 0) + 1
end

--- Record one provider's value for an attribute of `bare`.
--- Same-provider conflict rule: if multiple entries within the same provider disagree on an
--- attribute for the same bare id, that provider abstains for that attribute (order-independent).
local function record_provider_value(slot, val)
	if val == nil or slot.conflict then
		return
	end
	if slot.value == nil then
		slot.value = val
	elseif slot.value ~= val then
		slot.value = nil
		slot.conflict = true
	end
end

local function build_consensus(providers_map)
	local spelt = {}
	local named = {}
	local sizes = {}
	local outs = {}
	local reasons = {}
	local inputs = {}
	local levels = {}

	for _, pm in pairs(providers_map) do
		if type(pm) == "table" then
			local per_bare = {}
			for mid, e in pairs(pm) do
				if type(mid) == "string" and type(e) == "table" then
					local bare = M.bare_id(mid)
					if bare ~= "" then
						local g = per_bare[bare]
						if not g then
							g = { name = {}, eff = {}, reason = {}, input = {}, win = {}, out = {} }
							per_bare[bare] = g
						end
						if type(e.name) == "string" and e.name ~= "" and e.name ~= mid and e.name ~= bare then
							record_provider_value(g.name, e.name)
						end
						-- Consensus votes never apply provider-specific toggle-off.
						local eff = M.efforts_from_entry(e, false)
						if eff then
							record_provider_value(g.eff, table.concat(eff, ","))
						end
						if e.reasoning == true or eff ~= nil then
							record_provider_value(g.reason, true)
						elseif e.reasoning == false then
							record_provider_value(g.reason, false)
						end
						local ikey = canonical_input_key(e.input)
						if ikey then
							record_provider_value(g.input, ikey)
						end
						local win = M.window_of_limit(e.limit)
						if win then
							record_provider_value(g.win, win)
						end
						local lim_out = type(e.limit) == "table" and tonumber(e.limit.output) or nil
						if lim_out and lim_out > 0 then
							record_provider_value(g.out, lim_out)
						end
					end
				end
			end
			-- Each provider casts at most one vote per attribute per bare id.
			for bare, g in pairs(per_bare) do
				spelt[bare] = (spelt[bare] or 0) + 1
				vote_single(named, bare, g.name.value)
				vote_single(levels, bare, g.eff.value)
				if g.reason.value == true then
					reasons[bare] = (reasons[bare] or 0) + 1
				elseif g.reason.value == false then
					reasons[bare] = (reasons[bare] or 0) - 1
				end
				vote_single(inputs, bare, g.input.value)
				vote_single(sizes, bare, g.win.value)
				vote_single(outs, bare, g.out.value)
			end
		end
	end

	local by_bare = {}
	local normal = {}
	for bare, count in pairs(spelt) do
		local entry = {}
		local best_name = named[bare] and most_counted_string(named[bare]) or nil
		if best_name then
			entry.name = best_name
		end
		local best_win = sizes[bare] and most_counted_number(sizes[bare]) or nil
		local best_out = outs[bare] and most_counted_number(outs[bare]) or nil
		if best_win or best_out then
			entry.limit = { context = best_win, output = best_out }
		end
		local rv = reasons[bare]
		if rv and rv > 0 then
			entry.reasoning = true
		elseif rv and rv < 0 then
			entry.reasoning = false
		end
		local best_inp = inputs[bare] and most_counted_string(inputs[bare]) or nil
		if best_inp then
			entry.input = split_csv(best_inp)
		end
		local best_eff = levels[bare] and most_counted_string(levels[bare]) or nil
		if best_eff then
			entry.efforts = split_csv(best_eff)
		end
		if next(entry) ~= nil then
			by_bare[bare] = entry
		end

		local sk = M.same_model(bare)
		if sk ~= "" then
			local prev = normal[sk]
			if not prev or count > spelt[prev] or (count == spelt[prev] and bare < prev) then
				normal[sk] = bare
			end
		end
	end

	return { by_bare = by_bare, normal = normal }
end

local function get_index(md_table)
	local idx = index_cache[md_table]
	if not idx then
		idx = { providers = {}, consensus = nil }
		index_cache[md_table] = idx
	end
	return idx
end

--- Normalised metadata lookup (for metadata enrichment only, never model resolution).
--- Order:
---   1. Exact `id` under `md_table.providers[pid]`
---   2. Bare id after `/` under `md_table.providers[pid]`
---   3. Same-model key under `md_table.providers[pid]`
---   4. Cross-provider majority consensus for the bare id (or same-model canonical bare id)
--- Returns `entry, matched_id, provider_models_or_nil, is_provider_entry`.
function M.lookup(md_table, pid, id)
	if type(md_table) ~= "table" or type(md_table.providers) ~= "table" or type(id) ~= "string" or id == "" then
		return nil, nil, nil, false
	end
	local idx = get_index(md_table)
	local pm = pid and md_table.providers[pid] or nil
	local bare = M.bare_id(id)
	local sk = M.same_model(id)
	if type(pm) == "table" then
		local exact = pm[id]
		if type(exact) == "table" then
			return exact, id, pm, true
		end
		local pidx = idx.providers[pid]
		if not pidx then
			pidx = build_provider_index(pm)
			idx.providers[pid] = pidx
		end
		if bare ~= "" and bare ~= id then
			if type(pm[bare]) == "table" then
				return pm[bare], bare, pm, true
			end
			local hit = pidx.bare[bare]
			if hit then
				return hit.entry, hit.id, pm, true
			end
		end
		if sk ~= "" then
			local hit = pidx.same[sk]
			if hit then
				return hit.entry, hit.id, pm, true
			end
		end
	end

	if not idx.consensus then
		idx.consensus = build_consensus(md_table.providers)
	end
	local cons = idx.consensus
	if bare ~= "" and cons.by_bare[bare] then
		return cons.by_bare[bare], bare, nil, false
	end
	if sk ~= "" then
		local canon = cons.normal[sk]
		if canon and cons.by_bare[canon] then
			return cons.by_bare[canon], canon, nil, false
		end
	end
	return nil, nil, nil, false
end

return M
