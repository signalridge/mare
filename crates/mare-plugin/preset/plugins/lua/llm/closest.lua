-- llm.closest: rank catalog models closest to an unlisted/misspelled model ref.
-- Ported from magpie (`https://github.com/yetone/magpie`, `internal/provider/closest.go`),
-- MIT License, Copyright (c) 2026 yetone.
local M = {}

--- Split `s` into lowercase runs of ASCII letters and runs of ASCII digits.
--- Punctuation, whitespace, and transitions between letters and digits break runs.
local function split_words(s)
	local out = {}
	if type(s) ~= "string" then
		return out
	end
	local lower = string.lower(s)
	local i, n = 1, #lower
	while i <= n do
		local b = string.byte(lower, i)
		if b >= 97 and b <= 122 then
			local j = i + 1
			while j <= n do
				local c = string.byte(lower, j)
				if c < 97 or c > 122 then
					break
				end
				j = j + 1
			end
			out[#out + 1] = string.sub(lower, i, j - 1)
			i = j
		elseif b >= 48 and b <= 57 then
			local j = i + 1
			while j <= n do
				local c = string.byte(lower, j)
				if c < 48 or c > 57 then
					break
				end
				j = j + 1
			end
			out[#out + 1] = string.sub(lower, i, j - 1)
			i = j
		else
			i = i + 1
		end
	end
	return out
end

--- Lowercase `s` keeping only ASCII alphanumeric characters.
local function squash(s)
	if type(s) ~= "string" then
		return ""
	end
	return (string.lower(s):gsub("[^a-z0-9]", ""))
end

local function word_set(words)
	local set = {}
	for _, w in ipairs(words) do
		set[w] = true
	end
	return set
end

--- Return up to `limit` (default 3) catalog model refs closest to `asked`.
--- Scores each visible model by word hits in its display name (+4), model id (+3),
--- and provider id/label (+1), requiring at least one hit on the model's own name or id
--- so provider-only matches are skipped; adds +4 when the squashed query is a substring
--- of the squashed name or ref. Ties preserve deterministic ref order.
function M.models(asked, models, providers, limit)
	limit = limit or 3
	if type(asked) ~= "string" or limit <= 0 then
		return {}
	end
	local trimmed = asked:match("^%s*(.-)%s*$")
	if trimmed == "" then
		return {}
	end
	local asked_prov, asked_id = trimmed:match("^([^/]+)/(.+)$")
	local has_known_prov = asked_prov ~= nil and type(providers) == "table" and providers[asked_prov] ~= nil
	local want_own = split_words(has_known_prov and asked_id or trimmed)
	local want_prov = has_known_prov and split_words(asked_prov) or {}
	if #want_own == 0 then
		return {}
	end
	local flat = squash(has_known_prov and asked_id or trimmed)

	-- Collect visible models in deterministic ref order before scoring so ties are stable.
	local sorted = {}
	for _, m in pairs(models or {}) do
		if type(m) == "table" and type(m.ref) == "string" and not m.hidden then
			sorted[#sorted + 1] = m
		end
	end
	table.sort(sorted, function(a, b)
		return a.ref < b.ref
	end)

	local hits = {}
	for idx, m in ipairs(sorted) do
		local name = type(m.name) == "string" and m.name or ""
		local id = type(m.id) == "string" and m.id or ""
		local prov = type(m.provider) == "string" and m.provider or ""
		local p = providers and providers[prov]
		local prov_label = type(p) == "table" and type(p.label) == "string" and p.label or ""

		local name_words = word_set(split_words(name))
		local id_words = word_set(split_words(id))
		local prov_words = word_set(split_words(prov .. " " .. prov_label))

		local score, own = 0, 0
		for _, w in ipairs(want_own) do
			if name_words[w] then
				score = score + 4
				own = own + 4
			elseif id_words[w] then
				score = score + 3
				own = own + 3
			elseif prov_words[w] then
				score = score + 1
			end
		end
		for _, w in ipairs(want_prov) do
			if prov_words[w] then
				score = score + 1
			end
		end
		if own > 0 then
			if #flat >= 3 then
				if squash(name):find(flat, 1, true) or squash(m.ref):find(flat, 1, true) then
					score = score + 4
				end
			end
			hits[#hits + 1] = { ref = m.ref, score = score, ord = idx }
		end
	end

	table.sort(hits, function(a, b)
		if a.score ~= b.score then
			return a.score > b.score
		end
		return a.ord < b.ord
	end)

	local out, seen = {}, {}
	for _, h in ipairs(hits) do
		if not seen[h.ref] then
			seen[h.ref] = true
			out[#out + 1] = h.ref
			if #out >= limit then
				break
			end
		end
	end
	return out
end

--- Format a user-facing warning for an explicit `provider/id` not listed in its provider's catalog.
function M.format_unlisted(m, models, providers)
	local candidates = M.models(m.ref, models, providers, 3)
	if #candidates > 0 then
		return ("Model %s is not in %s's catalog (closest: %s)"):format(
			m.ref,
			m.provider,
			table.concat(candidates, ", ")
		)
	end
	return ("Model %s is not in %s's catalog"):format(m.ref, m.provider)
end

return M
