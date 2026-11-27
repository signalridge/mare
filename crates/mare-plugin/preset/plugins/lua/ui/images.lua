-- Output-backed image references. Storage authority stays with output; the renderer keeps
-- only locator/owner/digest/dimensions and a scoped native cache identity.
return function(D, service)
	local ctx, U = D.ctx, D.U
	local function load(key)
		local ref = mare.json.decode(key)
		assert(type(ref) == "table", "invalid output image reference")
		local session, spill = ctx:get("session"), ctx:get("spill")
		assert(session and spill, "output image services unavailable")
		local function owned()
			local owner = session:info()
			return owner.session_id == ref.session_id
				and owner.session_epoch == ref.session_epoch
				and not session:switching()
		end
		assert(owned(), "output image owner changed")
		local page = spill:read_bytes(ref.session_id, ref.spill_id, { limit = 8 * 1024 * 1024 })
		assert(owned(), "output image owner changed during read")
		assert(
			page.next_offset == nil and page.size <= 8 * 1024 * 1024 and #page.bytes == page.size,
			"output image exceeds byte limit"
		)
		assert(mare.crypto.sha256(page.bytes) == ref.sha256, "output image digest changed")
		return page.bytes
	end
	local revoke
	local function rotate()
		if revoke then
			revoke()
		end
		revoke = U.image_resolver("output", load)
	end
	rotate()
	for _, event in ipairs({ "session/changed", "session/loaded", "session/new" }) do
		ctx:on(event, rotate, { prepend = true })
	end
	ctx:effect(function()
		return function()
			if revoke then
				revoke()
			end
		end
	end, "ui output image resolver")
	function service.artifact_image(_, _, artifact, opts)
		local locator = type(artifact) == "table" and artifact.spill_id
		assert(type(locator) == "string" and #locator <= 65536, "invalid output image locator")
		assert(
			type(artifact.sha256) == "string" and artifact.sha256:match("^%x+$") and #artifact.sha256 == 64,
			"invalid image digest"
		)
		local session = ctx:get("session")
		assert(session, "output image session unavailable")
		local owner = session:info()
		assert(
			owner.session_id == artifact.session_id and not session:switching(),
			"output image belongs to another session"
		)
		opts = opts or {}
		local key = mare.json.encode({
			session_id = owner.session_id,
			session_epoch = owner.session_epoch,
			spill_id = locator,
			sha256 = artifact.sha256,
		})
		return U.image({
			resource = U.image_resource("output", key, artifact.width, artifact.height),
			max_rows = opts.max_rows,
			max_cols = opts.max_cols,
			image_id = opts.image_id,
			fallback = opts.fallback,
		})
	end
end
