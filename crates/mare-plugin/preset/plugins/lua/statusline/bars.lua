-- statusline.bars: formatting helpers, MCP/quota/job probes and missing-permissions warning;
-- part of the `statusline` plugin (plugins/statusline.lua).
local M = {}

local MODE_STYLE = {
	["read-only"] = "MareModeReadOnly",
	auto = "MareModeAuto",
	["full-access"] = "MareModeFull",
	custom = "MareModeCustom",
}
local MODE_TEXT = { ["read-only"] = "READ", auto = "AUTO", ["full-access"] = "FULL", custom = "CUSTOM" }

local function fmt_k(n)
	if n >= 1e6 then
		return ("%.1fM"):format(n / 1e6)
	end
	if n >= 1000 then
		return ("%dk"):format(math.floor(n / 1000))
	end
	return tostring(n)
end

local function mcp_connecting(ctx)
	local mcp = ctx:get("mcp")
	if not (mcp and mcp.list_servers) then
		return 0
	end
	local ok, st = pcall(mcp.list_servers, mcp)
	local n = 0
	for _, sv in ipairs(ok and st and st.data or {}) do
		if sv.state == "connecting" then
			n = n + 1
		end
	end
	return n
end

local function window_label(w)
	local m = w.window_minutes
	if not m then
		return nil
	end
	if m >= 7 * 24 * 60 - 60 then
		return "wk"
	end
	if m >= 24 * 60 then
		return ("%dd"):format(math.floor(m / 1440 + 0.5))
	end
	return ("%dh"):format(math.max(1, math.floor(m / 60 + 0.5)))
end

local function window_expired(w, now_ms)
	return type(w.reset_at_ms) == "number" and w.reset_at_ms <= now_ms
end

local function quota_text(limits)
	if not limits then
		return nil
	end
	local now_ms = mare.task.now()
	local parts = {}
	for _, key in ipairs({ "primary", "secondary" }) do
		local w = limits[key]
		if w and w.used_percent and not window_expired(w, now_ms) then
			local left = math.max(0, 100 - w.used_percent)
			local label = window_label(w)
			parts[#parts + 1] = ("%s%d%%"):format(label and (label .. " ") or "", math.floor(left + 0.5))
		end
	end
	if #parts == 0 then
		return nil
	end
	local low = false
	for _, key in ipairs({ "primary", "secondary" }) do
		local w = limits[key]
		if w and w.used_percent and not window_expired(w, now_ms) and w.used_percent >= 90 then
			low = true
		end
	end
	return table.concat(parts, " · ") .. " left", low
end

local function peer_allow()
	local p = (mare.o and mare.o.peer) or (mare.config and mare.config.peer) or nil
	local a = type(p) == "table" and p.allow or nil
	return type(a) == "table" and #a > 0 and a or nil
end

local function count_keys(t)
	local n = 0
	for _ in pairs(t) do
		n = n + 1
	end
	return n
end

function M.warn_no_permissions()
	local policy = mare.proc.sandbox.current()
	local WARN = policy
			and ("Built-in permissions service is off: sandbox %s is active; built-in approvals are unavailable. Other policy hooks may apply."):format(
				tostring(policy.level or "custom")
			)
		or "Built-in permissions service is off: no session sandbox is set and built-in approvals are unavailable. Other policy hooks may apply."
	if mare.ui.connected() then
		mare.ui.notify(WARN, "error")
	else
		io.stderr:write("mare: warning: " .. WARN .. "\n")
	end
end

function M.helpers(ctx, S)
	return {
		fmt_k = fmt_k,
		MODE_TEXT = MODE_TEXT,
		MODE_STYLE = MODE_STYLE,
		peer_allow = peer_allow,
		mcp_connecting = function()
			return mcp_connecting(ctx)
		end,
		quota_text = function()
			local m = ctx.llm and ctx.llm:current()
			local prov = m and m.provider
			return quota_text(prov and S.limits_by_provider and S.limits_by_provider[prov] or nil)
		end,
		count_jobs = function()
			return count_keys(S.jobs_running)
		end,
	}
end

return M
