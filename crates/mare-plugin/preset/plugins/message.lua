-- message: custom message types (validate + render), delivery-aware append that enters model
-- context while respecting display=true/false, distinct from display-only session entries.
return {
	name = "message",
	-- `task` gives ctx:spawn
	inject = { "agent", "task" },
	provide = { "message" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local stacks = {} -- custom_type -> list of spec entries
		local volatile_receipts = {} -- gen_key -> { request_id -> { fingerprint, result } }

		local function fail_invalid(msg)
			lib.fail("invalid_arguments", msg)
		end

		local function assert_valid(cond, msg)
			if not cond then
				fail_invalid(msg)
			end
		end

		local function wrap_render(fn, wire)
			if fn == nil then
				return nil
			end
			return function(msg, opts)
				return lib.normalize_render_result(fn(msg, opts), { wire = wire })
			end
		end

		local function normalize_spec(spec, wire)
			if spec == nil then
				spec = {}
			end
			assert_valid(type(spec) == "table", "message type spec must be a table")
			local validate = spec.validate
			if validate ~= nil then
				assert_valid(type(validate) == "function", "message type validate must be a function")
			end
			local render = spec.render
			local compact = spec.compact or render
			local expanded = spec.expanded or render or compact
			for k, fn in pairs({ render = render, compact = compact, expanded = expanded }) do
				if fn ~= nil then
					assert_valid(type(fn) == "function", ("message type %s must be a function"):format(k))
				end
			end
			return {
				validate = validate,
				render = wrap_render(render or compact, wire),
				compact = wrap_render(compact, wire),
				expanded = wrap_render(expanded, wire),
			}
		end

		local function normalize_blocks(content)
			if type(content) == "string" then
				return mare.json.array({ { type = "text", text = content } })
			end
			assert_valid(type(content) == "table", "custom message content must be a string or content block array")
			local out, count = {}, 0
			for k in pairs(content) do
				count = count + 1
				assert_valid(
					type(k) == "number" and k % 1 == 0 and k >= 1 and k <= #content,
					"content blocks must be a dense array"
				)
			end
			assert_valid(count == #content, "content blocks must be a dense array")
			for _, b in ipairs(content) do
				assert_valid(type(b) == "table" and type(b.type) == "string", "invalid content block")
				if b.type == "text" then
					assert_valid(type(b.text) == "string", "text block requires string text")
					out[#out + 1] = { type = "text", text = b.text }
				elseif b.type == "image" then
					assert_valid(
						type(b.data) == "string" and type(b.mime) == "string",
						"image block requires data and mime"
					)
					out[#out + 1] = { type = "image", data = b.data, mime = b.mime }
				else
					fail_invalid("unsupported custom message content block type: " .. tostring(b.type))
				end
			end
			return mare.json.array(out)
		end

		local function normalize_delivery(delivery)
			if delivery == nil then
				return {}
			end
			assert_valid(type(delivery) == "table", "delivery options must be a table")
			local as_kind = delivery.deliver_as
			if as_kind ~= nil then
				assert_valid(
					as_kind == "steer" or as_kind == "follow_up" or as_kind == "next_turn",
					"invalid deliver_as: " .. tostring(as_kind)
				)
			end
			local trigger = delivery.trigger_turn
			assert_valid(trigger == nil or type(trigger) == "boolean", "trigger_turn must be a boolean")
			return {
				deliver_as = as_kind,
				trigger_turn = trigger == true,
				wait = delivery.wait == true,
			}
		end

		local function summary_text(msg)
			local parts = {}
			for _, b in ipairs(msg.content or {}) do
				if b.type == "text" then
					parts[#parts + 1] = b.text
				elseif b.type == "image" then
					parts[#parts + 1] = "[image]"
				end
			end
			local body = table.concat(parts, " ")
			return ("[%s] %s"):format(msg.custom_type, body)
		end

		local service = {}

		function service.register_type(_, caller, custom_type, spec)
			if type(custom_type) ~= "string" or not custom_type:match("%S") then
				lib.fail("invalid_arguments", "custom message type must be a non-empty string")
			end
			local entry = normalize_spec(spec, lib.rpc_caller_id(caller) ~= nil)
			local st = stacks[custom_type] or {}
			stacks[custom_type] = st
			table.insert(st, entry)
			local removed = false
			local function remove()
				if removed then
					return
				end
				removed = true
				local i = table.find(st, entry)
				if i then
					table.remove(st, i)
				end
				if #st == 0 and stacks[custom_type] == st then
					stacks[custom_type] = nil
				end
			end
			-- withdrawn with the registering plugin's fiber
			return lib.owned(caller, remove, "message type " .. custom_type)
		end

		function service.get_type(_, _, custom_type)
			local st = type(custom_type) == "string" and stacks[custom_type]
			return st and st[#st] or nil
		end

		function service.list_types(_, _)
			local keys = {}
			for k in pairs(stacks) do
				keys[#keys + 1] = k
			end
			table.sort(keys)
			local out = mare.json.array()
			for _, k in ipairs(keys) do
				out[#out + 1] = { type = k }
			end
			return { data = out }
		end

		local function validate_message(spec, custom_type, msg)
			if not (spec and spec.validate) then
				return
			end
			local ok, valid, reason = pcall(spec.validate, msg)
			if not ok then
				if type(valid) == "table" and type(valid.code) == "string" then
					error(valid, 0)
				end
				error("custom message validation failed: " .. tostring(valid), 0)
			end
			if type(valid) == "table" and valid.valid ~= nil then
				valid, reason = valid.valid, valid.reason
			end
			if valid == false or (valid == nil and reason ~= nil) then
				lib.fail("invalid_arguments", tostring(reason or ("invalid custom message for type " .. custom_type)))
			end
		end

		function service.append(_, _, opts)
			assert_valid(type(opts) == "table", "message.append requires an arguments table")
			local custom_type = opts.type
			assert_valid(
				type(custom_type) == "string" and custom_type:match("%S"),
				"custom message type must be a non-empty string"
			)
			assert_valid(
				opts.display == nil or type(opts.display) == "boolean",
				"custom message display must be a boolean"
			)
			local request_id = opts.request_id
			if request_id ~= nil then
				assert_valid(
					type(request_id) == "string" and request_id ~= "" and #request_id <= 512,
					"request_id must be a non-empty string up to 512 bytes"
				)
			end
			local d = normalize_delivery(opts.delivery)
			local blocks = normalize_blocks(opts.content)
			local display = opts.display ~= false

			local session = ctx:get("session")
			local info = session and session.info and session:info() or nil
			local gen_key = info
					and info.session_id
					and info.session_epoch
					and (info.session_id .. "\0" .. tostring(info.session_epoch))
				or nil
			local claim, identity
			local committed = false
			if request_id and session and session.claim_operation then
				identity = {
					request_id = request_id,
					kind = "message/append",
					fingerprint = mare.crypto.sha256(mare.json.encode({
						type = custom_type,
						content = blocks,
						display = display,
						details = opts.details,
						delivery = d,
					})),
				}
				local err
				claim, err = session:claim_operation(identity, mare.task.token())
				if not claim then
					if tostring(err):find("conflict", 1, true) then
						lib.fail("request_id_conflict", tostring(err))
					end
					lib.fail("cancelled", tostring(err))
				end
				if claim.status == "replayed" then
					local replayed = (claim.receipt and claim.receipt.result) or {}
					replayed.duplicate = true
					return replayed
				end
				local volatile = gen_key and volatile_receipts[gen_key] and volatile_receipts[gen_key][request_id]
				if volatile then
					local release_claim = claim
					claim = nil
					if release_claim then
						session:release_operation(release_claim, "volatile request replay")
					end
					if volatile.fingerprint ~= identity.fingerprint then
						lib.fail("request_id_conflict", "operation request_id conflicts with its volatile input")
					end
					local replayed = lib.copy_deep(volatile.result)
					replayed.duplicate = true
					return replayed
				end
			end
			local release_guard <close> = setmetatable({}, {
				__close = function()
					if claim and not committed and session and session.release_operation then
						pcall(session.release_operation, session, claim, "aborted before commit")
					end
				end,
			})

			local st = stacks[custom_type]
			local spec = st and st[#st] or nil
			local created_at_ms = lib.to_epoch_ms(opts.created_at_ms)
			validate_message(spec, custom_type, {
				type = custom_type,
				content = opts.content,
				display = opts.display,
				details = opts.details,
				created_at_ms = created_at_ms,
			})

			local canonical = {
				role = "custom",
				custom_type = custom_type,
				content = blocks,
				display = display,
				details = opts.details,
				timestamp = created_at_ms or lib.to_epoch_ms(mare.task.now()),
			}

			local can_persist_receipt = claim ~= nil
				and identity ~= nil
				and session ~= nil
				and session.commit ~= nil
				and not (info and info.readonly)
			local function make_operation(result)
				if not can_persist_receipt then
					return nil
				end
				return {
					request_id = identity.request_id,
					kind = identity.kind,
					fingerprint = identity.fingerprint,
					claim_id = claim.claim_id,
					result = result,
				}
			end

			local res
			if d.deliver_as == "next_turn" then
				ctx.agent:queue_custom(canonical, "next_turn")
				res = { status = "queued", message = canonical }
				if request_id and identity and gen_key then
					volatile_receipts[gen_key] = volatile_receipts[gen_key] or {}
					volatile_receipts[gen_key][request_id] = {
						fingerprint = identity.fingerprint,
						result = lib.copy_deep(res),
					}
				end
			elseif ctx.agent:activity().busy then
				if d.deliver_as == "steer" or (d.deliver_as ~= "follow_up" and d.trigger_turn) then
					res = { status = "queued", message = canonical }
					ctx.agent:queue_custom(canonical, "steer", {
						text = summary_text(canonical),
						operation = make_operation(res),
					})
					committed = can_persist_receipt
				elseif d.deliver_as == "follow_up" then
					res = { status = "queued", message = canonical }
					ctx.agent:queue_custom(canonical, "follow_up", {
						text = summary_text(canonical),
						operation = make_operation(res),
					})
					committed = can_persist_receipt
				else
					ctx.agent:queue_custom(canonical, "pending")
					res = { status = "deferred", message = canonical }
					if request_id and identity and gen_key then
						volatile_receipts[gen_key] = volatile_receipts[gen_key] or {}
						volatile_receipts[gen_key][request_id] = {
							fingerprint = identity.fingerprint,
							result = lib.copy_deep(res),
						}
					end
				end
			elseif d.trigger_turn and d.wait then
				local saved, err = ctx.agent:append(canonical)
				if not saved then
					error(tostring(err or "failed to append custom message"), 0)
				end
				local started = ctx.agent:continue()
				local run_res = started and started.turn_id and ctx.agent:wait(started.turn_id) or started
				res = { status = "triggered", message = canonical, result = run_res }
				local op = make_operation(res)
				if op then
					local ok_commit, cerr = ctx.agent:append(nil, { operation = op })
					if not ok_commit then
						error(tostring(cerr or "failed to commit message.append operation receipt"), 0)
					end
					committed = true
				end
			else
				res = {
					status = d.trigger_turn and "triggered" or "appended",
					message = canonical,
				}
				local saved, err = ctx.agent:append(canonical, { operation = make_operation(res) })
				if not saved then
					error(tostring(err or "failed to append custom message"), 0)
				end
				committed = can_persist_receipt
				if d.trigger_turn then
					ctx:spawn(function()
						ctx.agent:continue()
					end)
				end
			end

			return res
		end

		-- a mare.Service: `caller` in a method is the calling plugin's ctx
		lib.service(ctx, "message", service)
	end,
}
