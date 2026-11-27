-- Generic stable operation results. Claims coordinate callers; only session:commit records
-- receipts, in the same native journal batch as the operation's state change. Neither a
-- claim nor a receipt is an authorization grant.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local claims = {}
	local function copy(value)
		return mare.json.decode(mare.json.encode(value))
	end
	local function identity(value)
		if type(value) ~= "table" then
			return nil, "invalid operation identity", "invalid_arguments"
		end
		local id, kind, fingerprint = value.request_id, value.kind, value.fingerprint
		if type(id) ~= "string" or id == "" or #id > 512 then
			return nil, "invalid operation request_id", "invalid_arguments"
		end
		if type(kind) ~= "string" or kind == "" or #kind > 128 then
			return nil, "invalid operation kind", "invalid_arguments"
		end
		if type(fingerprint) ~= "string" or #fingerprint ~= 64 or not fingerprint:match("^[a-f0-9]+$") then
			return nil, "invalid operation fingerprint", "invalid_arguments"
		end
		if value.session_id and value.session_id ~= S.id then
			return nil, "operation owner changed", "stale_session"
		end
		if value.session_epoch and value.session_epoch ~= D.store.current_epoch() then
			return nil, "operation owner changed", "stale_session"
		end
		return { request_id = id, kind = kind, fingerprint = fingerprint, session_id = S.id }
	end
	local function same(a, b)
		return a.kind == b.kind and a.fingerprint == b.fingerprint
	end
	local function key(sid, id)
		return sid .. "\0" .. id
	end
	local function receipt_for(id)
		local receipt = (S.operations or {})[id.request_id]
		if receipt and receipt.session_id == id.session_id then
			if not same(receipt, id) then
				return nil, "operation request_id conflicts with its committed input"
			end
			return receipt
		end
	end
	local function finish(claim, receipt, err)
		if claims[claim.key] ~= claim then
			return
		end
		claims[claim.key] = nil
		claim.finished:cancel()
		for _, waiter in pairs(claim.waiters) do
			waiter:send({ receipt = receipt, error = err })
		end
	end

	function session.operation_receipt(_, _, value)
		local id, id_err, id_code = identity(value)
		if not id then
			return nil, id_err, id_code
		end
		local receipt, err = receipt_for(id)
		if err then
			return nil, err, "conflict"
		end
		if receipt then
			return { status = "replayed", receipt = copy(receipt) }
		end
		return { status = "missing" }
	end

	function session.claim_operation(_, _, value, cancel)
		local id, id_err, id_code = identity(value)
		if not id then
			return nil, id_err, id_code
		end
		local receipt, err = receipt_for(id)
		if err then
			return nil, err, "conflict"
		end
		if receipt then
			return { status = "replayed", receipt = copy(receipt) }
		end
		assert(type(cancel) == "userdata", "operation claims require their caller cancellation token")
		if cancel:is_cancelled() then
			return nil, "operation caller was cancelled", "cancelled"
		end
		local k = key(id.session_id, id.request_id)
		local pending = claims[k]
		if pending then
			if not same(pending.identity, id) then
				return nil, "operation request_id conflicts with its pending input", "conflict"
			end
			local tx, rx = mare.task.channel()
			local waiter = {}
			pending.waiters[waiter] = tx
			local cleanup <close> = setmetatable({}, {
				__close = function()
					pending.waiters[waiter] = nil
				end,
			})
			local selected, result = mare.task.select(function()
				return rx:recv()
			end, function()
				cancel:wait()
			end)
			if selected ~= 1 then
				return nil, "operation caller was cancelled", "cancelled"
			end
			if not result or result.error then
				local msg = (result and result.error) or "operation owner was released"
				return nil, msg, (result and result.code) or "released"
			end
			return { status = "replayed", receipt = copy(result.receipt) }
		end
		local claim = {
			identity = id,
			key = k,
			generation = S.generation,
			session_epoch = D.store.current_epoch(),
			claim_id = mare.crypto.random_hex(16),
			waiters = {},
			finished = mare.task.token(),
			cancel = cancel,
		}
		claims[k] = claim
		local watching, watch_error = pcall(ctx.spawn, ctx, function()
			local selected = mare.task.select(function()
				cancel:wait()
			end, function()
				claim.finished:wait()
			end)
			if selected == 1 then
				finish(claim, nil, "operation caller was cancelled before commit")
			end
		end)
		if not watching then
			finish(claim, nil, tostring(watch_error))
			return nil, tostring(watch_error), "error"
		end
		return {
			status = "claimed",
			request_id = id.request_id,
			session_id = id.session_id,
			session_epoch = claim.session_epoch,
			claim_id = claim.claim_id,
		}
	end

	function session.release_operation(_, _, value, reason)
		if type(value) ~= "table" then
			return false
		end
		local claim = claims[key(value.session_id or S.id, value.request_id or "")]
		if not claim or claim.claim_id ~= value.claim_id then
			return false
		end
		finish(claim, nil, tostring(reason or "operation ended before commit"))
		return true
	end

	local function prepare(value)
		local id, id_err = identity(value)
		if not id then
			error(id_err, 0)
		end
		local known, err = receipt_for(id)
		if err then
			return nil, err
		end
		if known then
			return nil, "operation is already committed; claim its recorded result"
		end
		local claim = claims[key(id.session_id, id.request_id)]
		if not claim or claim.claim_id ~= value.claim_id or claim.generation ~= S.generation then
			return nil, "operation claim became stale"
		end
		if value.result == nil then
			return nil, "operation commit requires its complete result"
		end
		local receipt = copy({
			version = 1,
			session_id = id.session_id,
			request_id = id.request_id,
			kind = id.kind,
			fingerprint = id.fingerprint,
			result = value.result,
		})
		local function valid()
			return claims[claim.key] == claim
				and S.id == id.session_id
				and S.generation == claim.generation
				and not claim.cancel:is_cancelled()
		end
		return {
			entry = {
				type = "custom",
				kind = "session/operation",
				data = receipt,
				created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
			},
			receipt = receipt,
			valid = valid,
			install = function()
				S.operations = S.operations or {}
				S.operations[id.request_id] = copy(receipt)
			end,
			complete = function()
				finish(claim, receipt)
			end,
		}
	end
	local function dispose_claims()
		for _, claim in pairs(claims) do
			finish(claim, nil, "session operation owner was disposed")
		end
	end
	if D.lazy_operations then
		D.on_dispose_operations = dispose_claims
	else
		ctx:effect(function()
			return dispose_claims
		end, "session operation claims")
	end
	return { prepare = prepare }
end
