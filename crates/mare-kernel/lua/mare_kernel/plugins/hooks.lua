-- Host plugin `hooks`: unified `hooks.subscribe` service
-- for Cordis bus events and policy decision/rewrite points, used by both Lua plugins and RPC
-- clients (through `callback.call` and `registrations.release`).
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

local function fail(code, message)
	error(calls.coded(code, message), 0)
end

local function in_list(list, value)
	for _, item in ipairs(list or {}) do
		if item == value then
			return true
		end
	end
	return false
end

local function is_envelope(v)
	return type(v) == "table" and getmetatable(v) == nil
end

local function is_null(rpc, v)
	return v == nil or v == cordis.null or (rpc ~= nil and rpc.null ~= nil and v == rpc.null)
end

return function(prims, Service)
	local Hooks = cordis.class("HooksService", Service)

	function Hooks:constructor(ctx)
		local owner = prims.owner.open("hooks", "root")
		ctx:effect(function()
			return function()
				prims.owner.close(owner)
			end
		end, "owner")
		self.inflight = {}
		ctx:on("option/changed", function()
			local rpc = prims.rpc
			for slot in pairs(self.inflight) do
				if not slot.revoked then
					slot.revoked = rpc.revoked(slot.conn_id, slot.event, "hooks", "subscribe")
				end
			end
		end)
		prims.owner.enter(owner, Hooks.super.constructor, self, ctx, "hooks")
	end

	local function call_tracked(self, conn_id, event, fn, ...)
		if not conn_id then
			return true, fn(...)
		end
		local slot = { conn_id = conn_id, event = event }
		self.inflight[slot] = true
		local _cleanup <close> = setmetatable({}, {
			__close = function()
				self.inflight[slot] = nil
			end,
		})
		local ok, reply = pcall(fn, ...)
		self.inflight[slot] = nil
		return ok, reply, slot.revoked
	end

	local function pack_args(rpc, conn_id, event, ...)
		local n = select("#", ...)
		if n > 0 and getmetatable((select(n, ...))) == "mare.EventEnvelope" then
			n = n - 1
		end
		if conn_id then
			if n == 0 then
				return rpc.project(event)
			end
			local packed = table.pack(...)
			return rpc.project(event, table.unpack(packed, 1, n))
		end
		if n == 0 then
			return {}
		end
		local packed = table.pack(...)
		local args = {}
		for i = 1, n do
			args[i] = packed[i]
		end
		return args
	end

	local function invoke_checked(this, rpc, conn_id, event, fn, deny_on_revoke, ...)
		local ok, reply, mid_revoked = call_tracked(this, conn_id, event, fn, ...)
		if not ok then
			error(reply, 0)
		end
		if conn_id then
			local why = mid_revoked or rpc.revoked(conn_id, event, "hooks", "subscribe")
			if why then
				if deny_on_revoke then
					return false, { decision = "deny", reason = "RPC host hook: " .. why }
				end
				fail("denied", why)
			end
		end
		return true, reply
	end

	local function invoke_handle(rpc, timeout_ms, handle, event, mode, args)
		if timeout_ms ~= nil then
			return rpc.with_timeout(timeout_ms, handle, event, mode, args)
		end
		return handle(event, mode, args)
	end

	function Hooks:subscribe(event, opts)
		opts = opts or {}
		local handle = opts.handle
		if type(handle) ~= "function" then
			fail("invalid_arguments", "`handle` callback is required")
		end
		local this = self
		local caller = self.ctx
		local rpc = prims.rpc
		local rpc_svc = caller:get("rpc")
		local conn_id, owner_id = calls.resolve_caller(caller and caller.fiber, rpc_svc and rpc_svc.ids)
		local prepend = opts.prepend == true
		local timeout_ms = opts.timeout_ms
		if timeout_ms ~= nil then
			if type(timeout_ms) ~= "number" or timeout_ms < 1 or timeout_ms % 1 ~= 0 or timeout_ms >= math.huge then
				fail("invalid_arguments", "timeout_ms must be a positive integer")
			end
		end
		if conn_id then
			local host_cap = prims.policy and prims.policy.timeout_ms and math.max(1, prims.policy.timeout_ms())
				or 30000
			timeout_ms = math.min(timeout_ms or host_cap, host_cap)
		end
		local points = prims.policy and prims.policy.points or {}
		local rewrite_points = prims.policy and prims.policy.rewrite_points or {}
		local is_policy = in_list(points, event)
		local is_rewrite = in_list(rewrite_points, event)

		if is_policy or is_rewrite then
			prims.policy.unconfined(("a %s on `%s`"):format(is_policy and "voter" or "rewriter", event))
			if conn_id then
				local refused = rpc.may_subscribe(conn_id, event)
				if refused then
					fail("invalid_arguments", refused)
				end
			end
			local policy = caller:get("policy")
			if not policy then
				fail("unavailable", "the policy host plugin is not running")
			end
			local function make_policy_fn()
				if is_policy then
					return function(...)
						if conn_id then
							local why = rpc.revoked(conn_id, event, "hooks", "subscribe")
							if why then
								return { decision = "deny", reason = "RPC host hook: " .. why }
							end
						end
						local args = pack_args(rpc, conn_id, event, ...)
						local passed, reply =
							invoke_checked(this, rpc, conn_id, event, handle, true, event, "policy", args)
						if not passed then
							return reply
						end
						if not is_envelope(reply) then
							fail(
								"invalid_result",
								("remote voter on `%s` answered no { value } envelope"):format(event)
							)
						end
						if is_null(rpc, reply.value) then
							return nil
						end
						return reply.value
					end
				end
				return function(call)
					if conn_id and rpc.revoked(conn_id, event, "hooks", "subscribe") then
						return nil
					end
					local args, token = pack_args(rpc, conn_id, event, call)
					local _, reply = invoke_checked(this, rpc, conn_id, event, handle, false, event, "waterfall", args)
					if not is_envelope(reply) then
						fail("invalid_result", ("remote hook on `%s` answered neither next nor value"):format(event))
					end
					if reply.next == true and is_null(rpc, reply.value) then
						return nil
					end
					if not is_null(rpc, reply.value) then
						local rewritten = conn_id and rpc.restore(token, reply.value) or reply.value
						if type(rewritten) ~= "table" or type(rewritten.arguments) ~= "table" then
							fail(
								"invalid_result",
								("remote rewrite on `%s` answered no call with arguments"):format(event)
							)
						end
						return rewritten.arguments
					end
					fail("invalid_result", ("remote hook on `%s` answered neither next nor value"):format(event))
				end
			end
			local fn = make_policy_fn()
			local reg_opts = { prepend = prepend, timeout_ms = timeout_ms }
			local function do_register()
				if is_policy then
					return policy:voter(event, fn, reg_opts)
				end
				return policy:rewriter(event, fn, reg_opts)
			end
			local dispose = calls.as_owner(prims, owner_id, do_register)
			return function()
				return dispose()
			end
		end

		local mode = rpc.remote_mode(event)
		if conn_id then
			if mode == nil then
				fail("invalid_arguments", ("`%s` does not allow remote participants"):format(event))
			end
			local refused = rpc.may_subscribe(conn_id, event)
			if refused then
				fail("invalid_arguments", refused)
			end
		else
			mode = mode or "emit"
		end

		local listener
		if mode == "emit" then
			listener = function(...)
				if
					conn_id and (rpc.revoked(conn_id, event, "hooks", "subscribe") or not rpc.callbacks_active(conn_id))
				then
					return
				end
				local args = pack_args(rpc, conn_id, event, ...)
				invoke_handle(rpc, timeout_ms, handle, event, "emit", args)
			end
		elseif mode == "parallel" or mode == "serial" then
			listener = function(...)
				if conn_id and rpc.revoked(conn_id, event, "hooks", "subscribe") then
					return nil
				end
				local args = pack_args(rpc, conn_id, event, ...)
				local _, reply = invoke_checked(
					this,
					rpc,
					conn_id,
					event,
					invoke_handle,
					false,
					rpc,
					timeout_ms,
					handle,
					event,
					mode,
					args
				)
				if not is_envelope(reply) then
					fail("invalid_result", ("remote hook on `%s` answered no { value } envelope"):format(event))
				end
				if is_null(rpc, reply.value) then
					return nil
				end
				return reply.value
			end
		elseif mode == "waterfall" then
			listener = function(...)
				local n = select("#", ...)
				local ev = select(n, ...)
				local nextFn = (type(ev) == "table" and ev.next) or ev
				if conn_id and rpc.revoked(conn_id, event, "hooks", "subscribe") then
					return nextFn()
				end
				local packed = table.pack(...)
				local args, token = pack_args(rpc, conn_id, event, table.unpack(packed, 1, n - 1))
				local _, reply = invoke_checked(
					this,
					rpc,
					conn_id,
					event,
					invoke_handle,
					false,
					rpc,
					timeout_ms,
					handle,
					event,
					"waterfall",
					args
				)
				if not is_envelope(reply) then
					fail("invalid_result", ("remote hook on `%s` answered neither next nor value"):format(event))
				end
				if not is_null(rpc, reply.value) then
					local restored = conn_id and rpc.restore(token, reply.value) or reply.value
					if reply.next == true then
						return nextFn(restored, table.unpack(packed, 2, n - 1))
					end
					return restored
				end
				if reply.next == true then
					return nextFn()
				end
				fail("invalid_result", ("remote hook on `%s` answered neither next nor value"):format(event))
			end
		else
			fail("invalid_arguments", ("unknown hook mode `%s`"):format(tostring(mode)))
		end

		local dispose = calls.as_owner(prims, owner_id, function()
			return caller:on(event, listener, prepend)
		end)
		return function()
			return dispose()
		end
	end

	return Hooks
end
