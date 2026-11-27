-- Generic fenced-code presentation registry. Parser offsets identify occurrences; cached
-- nodes never replace source text. All callbacks run in owned tasks, outside the draw path.
return function(D, service)
	local ctx, U = D.ctx, D.U
	local stacks, documents, owners, generation, next_registration, next_request_id = {}, {}, {}, 0, 0, 0
	local refresh
	local registry_revision = 0

	local function cancel(doc)
		for _, request in pairs(doc.requests) do
			request.token:cancel()
		end
		doc.requests = {}
	end

	local function index_fences(fences)
		local sources = {}
		for _, fence in ipairs(fences) do
			sources[fence.start] = fence
		end
		return sources
	end

	local function node(doc, expanded)
		local blocks = {}
		local sources = nil
		if expanded and next(doc.blocks) ~= nil then
			if
				doc.fence_index
				and doc.fence_index_source == doc.source
				and doc.fence_index_revision == doc.revision
			then
				sources = doc.fence_index
			else
				sources = index_fences(U.markdown_blocks(doc.source))
				doc.fence_index = sources
				doc.fence_index_source = doc.source
				doc.fence_index_revision = doc.revision
			end
		end
		for start, value in pairs(doc.blocks) do
			if expanded then
				local expanded_value = doc.expanded_blocks and doc.expanded_blocks[start]
				if expanded_value then
					value = expanded_value
				end
				if sources and sources[start] then
					local fence = sources[start]
					value = U.Lines({ value, U.Code(fence.source, fence.language) })
				end
			end
			blocks[#blocks + 1] = { start = start, node = value }
		end
		table.sort(blocks, function(a, b)
			return a.start < b.start
		end)
		local result = U.PresentedMarkdown(doc.source, blocks)
		if not doc.base then
			return result
		end
		local function project(value)
			if type(value) ~= "table" then
				return value
			end
			if value.type == "markdown" and value.text == doc.source then
				return result
			end
			local copy = {}
			for key, child in pairs(value) do
				copy[key] = project(child)
			end
			return copy
		end
		return project(doc.base)
	end

	local function locate_images(value, previous, used, path)
		if type(value) ~= "table" then
			return
		end
		if value.type == "image" then
			local id = previous[path] or U.image_id()
			value.image_id, used[path] = id, id
		end
		for key, child in pairs(value) do
			locate_images(child, previous, used, path .. "/" .. mare.json.encode(key))
		end
	end

	local function project_wire(value)
		if value ~= nil and mare.projection and mare.projection.active() then
			return mare.projection.project(value, { lossy = true })
		end
		return value
	end

	local function supports_artifact_images(doc)
		if doc.remote then
			return doc.viewport ~= nil and doc.viewport.supports_artifact_images == true
		end
		return doc.viewport == nil or doc.viewport.supports_artifact_images == true
	end

	local function has_image_resource(value, seen)
		if type(value) ~= "table" then
			return false
		end
		seen = seen or {}
		if seen[value] then
			return false
		end
		seen[value] = true
		if value.type == "image" and value.resource ~= nil then
			return true
		end
		for _, child in pairs(value) do
			if has_image_resource(child, seen) then
				return true
			end
		end
		return false
	end

	local function make_snapshot(doc, encoded_node, encoded_expanded, source, diagnostic)
		local pending = 0
		for _, request in pairs(doc.requests) do
			if not request.done then
				pending = pending + 1
			end
		end
		return {
			document_id = doc.id,
			source = source,
			revision = doc.revision,
			render_seq = generation,
			session_id = doc.session.id,
			session_epoch = doc.session.epoch,
			pending = pending,
			enhanced = next(doc.blocks) ~= nil,
			supports_artifact_images = (doc.viewport and doc.viewport.supports_artifact_images == true) or false,
			diagnostic = diagnostic,
			node = encoded_node,
			expanded_node = encoded_expanded,
		}
	end

	local function snapshot(doc)
		return make_snapshot(
			doc,
			U.snapshot_node(node(doc)),
			U.snapshot_node(node(doc, true)),
			doc.source,
			doc.diagnostic
		)
	end

	local function encode_wire_node(raw_node, can_artifacts)
		if not can_artifacts and has_image_resource(raw_node) then
			return nil, "Consumer did not negotiate artifact images; complete source is shown."
		end
		local ok, encoded = pcall(U.node_to_wire, raw_node)
		if not ok then
			return nil, tostring(encoded)
		end
		return encoded, nil
	end

	local function wire_snapshot(doc)
		local can_artifacts = supports_artifact_images(doc)
		local diagnostic = doc.diagnostic
		local compact, err = encode_wire_node(node(doc), can_artifacts)
		local expanded
		if compact then
			expanded, err = encode_wire_node(node(doc, true), can_artifacts)
		end
		if not compact or not expanded then
			local fallback_raw = U.PresentedMarkdown(doc.source, {})
			compact = U.node_to_wire(fallback_raw)
			expanded = U.node_to_wire(fallback_raw)
			if not diagnostic then
				diagnostic = err
			end
		end
		return make_snapshot(
			doc,
			project_wire(compact),
			project_wire(expanded),
			project_wire(doc.source),
			project_wire(diagnostic)
		)
	end

	local function deliver(doc)
		if doc.update then
			local state = snapshot(doc)
			local ok, err = pcall(doc.update, state.node, state.revision, state)
			if not ok then
				ctx.logger:warn("Markdown presentation update: " .. tostring(err))
			end
		elseif doc.remote then
			ctx.bus:emit("ui/markdown-changed", {
				owner = doc.owner,
				presentation = wire_snapshot(doc),
			})
		end
	end

	refresh = function(doc, force)
		if doc.cancelled then
			return false
		end
		local viewport = doc.viewport or {}
		local width, height = viewport.width or U.width(), viewport.height or U.height()
		local theme_service = ctx:get("theme")
		local theme = theme_service and theme_service:get() or {}
		local theme_info = {
			active = theme.active,
			appearance = theme.appearance,
			revision = theme.revision,
			code_background = U.style("MareCodeBlock").bg,
		}
		local capabilities = U.caps()
		local layout_revision = U.layout_revision()
		local image_capability = capabilities.images
		local supports_images = viewport.supports_images
		if supports_images == nil then
			supports_images = U.connected() and image_capability ~= nil and image_capability ~= "none"
		end
		local supports_artifacts = supports_artifact_images(doc)
		local profile = mare.json.encode({
			width,
			height,
			theme_info,
			capabilities,
			layout_revision,
			supports_images,
			supports_artifacts,
			doc.environment,
			registry_revision,
			doc.replay,
			doc.session.id,
			doc.session.generation,
		})
		if not force and doc.rendered_source == doc.source and doc.profile == profile then
			return false
		end
		doc.rendered_source, doc.profile = doc.source, profile
		doc.revision = doc.revision + 1
		local previous, previous_ids = doc.requests, doc.image_ids or {}
		doc.requests, doc.blocks, doc.expanded_blocks, doc.image_ids = {}, {}, {}, {}
		doc.diagnostic = nil
		local revision, owned_generation = doc.revision, generation
		local fences = next(stacks) and U.markdown_blocks(doc.source) or nil
		if fences then
			doc.fence_index = index_fences(fences)
			doc.fence_index_source = doc.source
			doc.fence_index_revision = revision
		else
			doc.fence_index = nil
			doc.fence_index_source = nil
			doc.fence_index_revision = nil
		end
		for _, block in ipairs(fences or {}) do
			local stack = stacks[block.language]
			local registration = stack and stack[#stack]
			if
				registration
				and registration.requires_artifact_images
				and supports_images
				and not supports_artifacts
			then
				registration = nil
				doc.diagnostic = "Consumer did not negotiate artifact images; complete source is shown."
			end
			if registration then
				doc.image_ids[block.start] = previous_ids[block.start] or {}
			end
			if registration and (block.closed or registration.preview) then
				local prior = previous[block.start]
				local key = mare.json.encode({
					registration.id,
					block.source,
					block.info,
					block.closed,
					width,
					height,
					doc.replay,
					doc.environment,
					supports_images,
					theme_info,
					capabilities,
					layout_revision,
					doc.session.id,
					doc.session.generation,
				})
				if prior and prior.key == key and not prior.token:is_cancelled() then
					doc.requests[block.start] = prior
					doc.blocks[block.start] = prior.node
					doc.expanded_blocks[block.start] = prior.expanded_node
					previous[block.start] = nil
					goto continue
				end
				local token = mare.task.token()
				next_request_id = next_request_id + 1
				local request_id = next_request_id
				local pending = { key = key, token = token, request_id = request_id }
				doc.requests[block.start] = pending
				local request = {
					document_id = doc.id,
					session_id = doc.session.id,
					session_epoch = doc.session.epoch,
					block_id = doc.id .. ":" .. block.start,
					start = block.start,
					source = block.source,
					language = block.language,
					info = block.info,
					phase = block.closed and "closed" or "open",
					revision = revision,
					render_seq = owned_generation,
					request_id = request_id,
					width = width,
					height = height,
					capabilities = U.snapshot_node(capabilities),
					layout_revision = layout_revision,
					supports_images = supports_images,
					supports_artifact_images = supports_artifacts,
					theme = U.snapshot_node(theme_info),
					replay = doc.replay,
					cancel = token,
				}
				-- the render runs the presenter's function, so it is the registering plugin's task;
				-- a kernel-held caller (an RPC connection) has no tasks of its own and its render
				-- runs here, stopped by `registration.active` once the connection goes
				local owner = type(registration.caller.spawn) == "function" and registration.caller or ctx
				owner:spawn(function()
					if token:is_cancelled() or not registration.active or doc.requests[block.start] ~= pending then
						return
					end
					local ok, finished, value = pcall(mare.task.timeout, registration.timeout_ms, function()
						return registration.render(request)
					end)
					if
						token:is_cancelled()
						or not registration.active
						or documents[doc] ~= doc
						or doc.requests[block.start] ~= pending
						or generation ~= owned_generation
					then
						if documents[doc] == doc and doc.requests[block.start] == pending then
							pending.done = true
							deliver(doc)
						end
						ctx.bus:emit("ui/presentation-render-completed", {
							document_id = doc.id,
							block_id = request.block_id,
							revision = revision,
							render_seq = owned_generation,
							request_id = request_id,
							accepted = false,
						})
						return
					end
					if ok and finished and value ~= nil then
						-- Validate before accepting; malformed descriptors keep the source fallback.
						local compact_raw, expanded_raw
						if type(value) == "table" then
							if value.type ~= nil then
								compact_raw = value
							elseif value.compact ~= nil then
								local valid_keys = true
								for key in pairs(value) do
									if key ~= "compact" and key ~= "expanded" then
										valid_keys = false
										break
									end
								end
								if valid_keys then
									compact_raw, expanded_raw = value.compact, value.expanded
								end
							end
						end
						local function accept(raw)
							if raw == nil then
								return nil, true
							end
							local copied, candidate = pcall(U.snapshot_node, raw)
							local valid, accepted, info = pcall(U.validate_node, copied and candidate or nil)
							if copied and valid and accepted and (supports_images or not info.images) then
								return candidate, true
							end
							return nil, false
						end
						local candidate, compact_ok = accept(compact_raw)
						local expanded_candidate, expanded_ok = accept(expanded_raw)
						if compact_raw ~= nil and compact_ok and expanded_ok then
							local image_ids = {}
							locate_images(candidate, doc.image_ids[block.start], image_ids, "")
							if expanded_candidate then
								locate_images(expanded_candidate, doc.image_ids[block.start], image_ids, "expanded")
							end
							doc.blocks[block.start] = candidate
							doc.expanded_blocks[block.start] = expanded_candidate
							if
								pcall(U.snapshot_node, node(doc))
								and (not expanded_candidate or pcall(U.snapshot_node, node(doc, true)))
							then
								doc.image_ids[block.start] = image_ids
								pending.node = candidate
								pending.expanded_node = expanded_candidate
							else
								doc.blocks[block.start] = nil
								doc.expanded_blocks[block.start] = nil
							end
						end
					end
					if not ok or not finished then
						token:cancel()
					end
					pending.done = true
					deliver(doc)
					ctx.bus:emit("ui/presentation-render-completed", {
						document_id = doc.id,
						block_id = request.block_id,
						revision = revision,
						render_seq = owned_generation,
						request_id = request_id,
						accepted = ok and finished and value ~= nil and doc.blocks[block.start] ~= nil,
					})
				end)
			end
			::continue::
		end
		for _, pending in pairs(previous) do
			pending.token:cancel()
		end
		return true
	end

	-- documents are kept per calling plugin: an old ctx's id or a cordis fiber's uid
	local function owner_id(caller)
		local id = require("mare_lib").caller_id(caller)
		assert(id ~= nil, "a Markdown document needs a calling plugin")
		return id
	end

	function service.markdown_presenter(_, caller, language, def)
		assert(type(language) == "string" and language:match("^[%w_+%-]+$"), "invalid fence language")
		assert(type(def) == "table" and type(def.render) == "function", "presenter requires render callback")
		for key in pairs(def) do
			assert(
				key == "render" or key == "preview" or key == "timeout_ms" or key == "requires_artifact_images",
				"unknown presenter option: " .. tostring(key)
			)
		end
		assert(def.preview == nil or type(def.preview) == "boolean", "invalid preview flag")
		assert(
			def.requires_artifact_images == nil or type(def.requires_artifact_images) == "boolean",
			"invalid artifact capability flag"
		)
		local timeout = def.timeout_ms == nil and 15000 or def.timeout_ms
		assert(
			type(timeout) == "number" and timeout % 1 == 0 and timeout >= 1 and timeout <= 120000,
			"invalid presenter timeout"
		)
		language = language:lower()
		next_registration = next_registration + 1
		local registration = {
			id = next_registration,
			caller = caller,
			render = def.render,
			preview = def.preview == true,
			requires_artifact_images = def.requires_artifact_images == true,
			timeout_ms = timeout,
			active = true,
		}
		stacks[language] = stacks[language] or {}
		table.insert(stacks[language], registration)
		registry_revision = registry_revision + 1
		local function redraw()
			for _, doc in pairs(documents) do
				if refresh(doc) then
					deliver(doc)
				end
			end
		end
		local function remove()
			if not registration.active then
				return
			end
			registration.active = false
			registry_revision = registry_revision + 1
			local stack = stacks[language]
			for index, value in ipairs(stack) do
				if value == registration then
					table.remove(stack, index)
					break
				end
			end
			if #stack == 0 then
				stacks[language] = nil
			end
			redraw()
			ctx.bus:emit(
				"ui/presenter-changed",
				{ language = language, registration = registration.id, action = "removed" }
			)
		end
		caller:effect(function()
			return remove
		end, "ui markdown presenter " .. language)
		redraw()
		ctx.bus:emit(
			"ui/presenter-changed",
			{ language = language, registration = registration.id, action = "registered" }
		)
		return remove
	end

	local function invalid_arg(ok, msg)
		if not ok then
			require("mare_lib").fail("invalid_arguments", msg)
		end
	end

	local function upsert_markdown_document(caller, id, source, opts, format_snapshot)
		invalid_arg(type(id) == "string" and id ~= "" and #id <= 256 and not id:find("%c"), "invalid document id")
		invalid_arg(
			type(source) == "string" and #source <= 8 * 1024 * 1024,
			"document source must be a string up to 8 MiB"
		)
		invalid_arg(utf8.len(source) ~= nil, "document source must be UTF-8")
		if opts == nil then
			opts = {}
		end
		invalid_arg(type(opts) == "table", "document options must be a table")
		for key in pairs(opts) do
			invalid_arg(
				key == "on_update" or key == "replay" or key == "base_node" or key == "viewport",
				"unknown document option: " .. tostring(key)
			)
		end
		invalid_arg(opts.replay == nil or type(opts.replay) == "boolean", "invalid replay flag")
		if opts.viewport ~= nil then
			invalid_arg(type(opts.viewport) == "table", "viewport must be a table")
			for key, value in pairs(opts.viewport) do
				if key == "width" or key == "height" then
					invalid_arg(
						type(value) == "number" and value % 1 == 0 and value >= 1 and value <= 10000,
						"invalid viewport size"
					)
				elseif key == "supports_images" or key == "supports_artifact_images" then
					invalid_arg(type(value) == "boolean", "invalid image capability")
				else
					invalid_arg(false, "unknown viewport option: " .. tostring(key))
				end
			end
		end
		invalid_arg(opts.on_update == nil or type(opts.on_update) == "function", "invalid update callback")
		local ok_base, base = pcall(function()
			return opts.base_node ~= nil and U.snapshot_node(opts.base_node) or nil
		end)
		invalid_arg(ok_base and (base == nil or U.validate_node(base)), "invalid base node")
		local viewport = opts.viewport and U.snapshot_node(opts.viewport) or nil
		local owner = owner_id(caller)
		local owned = owners[owner]
		if not owned then
			owned = {}
			owners[owner] = owned
			-- the caller's documents go with its fiber
			caller:effect(function()
				return function()
					for _, doc in pairs(owned) do
						cancel(doc)
						documents[doc] = nil
					end
					if owners[owner] == owned then
						owners[owner] = nil
					end
				end
			end, "ui markdown documents")
		end
		local doc = owned[id]
		if not doc then
			doc = {
				id = id,
				owner = owner,
				revision = 0,
				requests = {},
				blocks = {},
				environment = 0,
			}
			documents[doc], owned[id] = doc, doc
		end
		doc.cancelled = false
		local session_service = ctx:get("session")
		local info = session_service and session_service:info() or {}
		doc.session = { id = info.session_id, generation = info.session_epoch, epoch = info.session_epoch }
		doc.source, doc.replay, doc.update = source, opts.replay == true, opts.on_update
		doc.remote = tostring(require("mare_lib").caller_name(caller) or ""):sub(1, 4) == "rpc:" and doc.update == nil
		doc.base, doc.viewport = base, viewport
		refresh(doc, true)
		return format_snapshot(doc)
	end

	function D.has_markdown_presenters()
		return next(stacks) ~= nil
	end

	-- Same-process TUI updates need native nodes (including artifact resolver_id and raw bytes);
	-- service exports go through guard.leave (strict_lua_to_json) and RPC wire encoding.
	function D.native_markdown_document(caller, id, source, opts)
		return upsert_markdown_document(caller, id, source, opts, snapshot)
	end

	function service.render_markdown(_, caller, id, source, opts)
		return upsert_markdown_document(caller, id, source, opts, wire_snapshot)
	end

	function service.delete_markdown(_, caller, id)
		local owner = owner_id(caller)
		local doc = owners[owner] and owners[owner][id]
		if not doc then
			return {}
		end
		assert(doc.owner == owner, "document belongs to another plugin")
		cancel(doc)
		documents[doc], owners[owner][id] = nil, nil
		return {}
	end

	function service.get_markdown(_, caller, id)
		local owner = owner_id(caller)
		local doc = owners[owner] and owners[owner][id]
		if not doc then
			error(
				{ code = "ui.unknown_document", message = ("unknown markdown document `%s`"):format(tostring(id)) },
				0
			)
		end
		return wire_snapshot(doc)
	end

	function service.list_markdown_presenters()
		local out = {}
		for language, stack in pairs(stacks) do
			local top = stack[#stack]
			out[#out + 1] = {
				language = language,
				preview = top.preview,
				timeout_ms = top.timeout_ms,
				requires_artifact_images = top.requires_artifact_images,
			}
		end
		table.sort(out, function(a, b)
			return a.language < b.language
		end)
		return { data = mare.json.array(out) }
	end

	function service.cancel_markdown(_, caller, id)
		local owner = owner_id(caller)
		local doc = owners[owner] and owners[owner][id]
		if not doc then
			return {}
		end
		cancel(doc)
		doc.cancelled = true
		doc.blocks = {}
		doc.expanded_blocks = {}
		doc.fence_index = nil
		doc.fence_index_source = nil
		doc.fence_index_revision = nil
		doc.revision = doc.revision + 1
		deliver(doc)
		return {}
	end

	function D.invalidate_presentations()
		ctx.bus:emit("ui/presentation-invalidate", {})
		return {}
	end

	local function reset()
		generation = generation + 1
		for _, doc in pairs(documents) do
			cancel(doc)
		end
		for _, owned in pairs(owners) do
			for id in pairs(owned) do
				owned[id] = nil
			end
		end
		documents = {}
	end
	ctx:on("session/changed", reset, { prepend = true })
	ctx:on("session/loaded", reset, { prepend = true })
	ctx:on("session/new", reset, { prepend = true })
	ctx:effect(function()
		return reset
	end, "ui markdown presentations")
	for _, event in ipairs({
		"ui/resize",
		"ui/background",
		"theme/changed",
		"ui/reflow",
		"ui/caps-changed",
		"ui/presentation-invalidate",
	}) do
		ctx:on(event, function()
			for _, doc in pairs(documents) do
				if event == "ui/presentation-invalidate" then
					doc.environment = doc.environment + 1
				end
				if refresh(doc) then
					deliver(doc)
				end
			end
		end)
	end
end
