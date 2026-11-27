-- ui.service: the `ui` service (widgets, status segments, keys, renderers, styles), the
-- interface shell options and the `keys` config bindings;
-- part of the `ui` plugin (plugins/ui.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, U = D.ctx, D.cfg, D.U

	local widgets = {} -- id -> def

	local service = {}

	--- rows currently available to the widget area (re-evaluated on every draw / resize)
	local function widget_max_height()
		local h = U.height()
		return math.max(1, math.floor(h * cfg.widget_area_ratio) - cfg.widget_reserved_rows)
	end

	--- widget(id, {slot="above-editor"|"below-editor", order=,
	---              render=function(width, max_height) -> node})
	function service.widget(_, caller, id, def)
		widgets[id] = def
		def.draw = function()
			local ok, node = pcall(def.render, U.width(), widget_max_height())
			if ok then
				U.set_widget(id, def.slot, node, def.order)
			else
				ctx.logger:warn("widget " .. id .. ": " .. tostring(node))
			end
		end
		def.draw()
		caller:effect(function()
			return function()
				if widgets[id] == def then
					widgets[id] = nil
					U.set_widget(id, def.slot, nil)
				end
			end
		end, "ui widget " .. tostring(id))
	end

	--- status(id, {align="left"|"right", order=, keep=, render=function(width) -> node}) -> release
	local caller_id = require("mare_lib").caller_id
	local status_owners = {}
	function service.status(_, caller, id, def)
		if type(def) == "table" and def.align ~= nil and def.align ~= "left" and def.align ~= "right" then
			error('invalid status align "' .. tostring(def.align) .. '": expected "left" or "right"', 2)
		end
		local owner = caller_id(caller) or caller
		local cur = status_owners[id]
		assert(not cur or cur.owner == owner, "ui.status: segment '" .. tostring(id) .. "' is owned by another caller")
		local inner = U.status(id, def)
		local token = {}
		status_owners[id] = { owner = owner, token = token }
		local released = false
		local function release()
			if released then
				return
			end
			released = true
			if status_owners[id] and status_owners[id].token == token then
				status_owners[id] = nil
			end
			inner()
		end
		caller:effect(function()
			return release
		end, "ui status " .. tostring(id))
		return release
	end

	--- invalidate(component?, opts?): re-run render() once, or update viewport and emit ui/resize
	function service.invalidate(_, _, component, opts)
		local lib = require("mare_lib")
		opts = type(opts) == "table" and opts or {}
		local vp = opts.viewport
		if type(vp) == "table" then
			local ok, changed_or_err = pcall(U.set_viewport, vp.width, vp.height)
			if not ok then
				lib.fail("invalid_arguments", tostring(changed_or_err))
			end
			if changed_or_err then
				ctx.bus:emit("ui/resize", {})
			end
		end
		if type(component) == "string" and component ~= "" then
			local id = component:match("^widget:(.+)$") or component
			if widgets[id] then
				widgets[id].draw()
			else
				U.status_redraw(id)
			end
		elseif type(vp) ~= "table" then
			ctx.bus:emit("ui/resize", {})
		end
		return {}
	end

	--- invalidate_presentations(): emit ui/presentation-invalidate under the `ui` service owner
	function service.invalidate_presentations()
		return D.invalidate_presentations()
	end

	--- get_keymap(context?): snapshot contexts, actions, bindings, and conflicts
	function service.get_keymap(_, _, filter_context)
		local lib = require("mare_lib")
		if filter_context ~= nil and type(filter_context) ~= "string" then
			lib.fail("invalid_arguments", "context must be a string")
		end
		local ok, snap = pcall(mare.keymap.list, { snapshot = true, context = filter_context })
		if not ok then
			lib.fail("invalid_arguments", tostring(snap))
		end
		return snap
	end

	local function caller_source(lib, caller)
		local rpc_id = lib.rpc_caller_id(caller)
		if rpc_id ~= nil then
			return "rpc:" .. tostring(rpc_id), "plugin"
		end
		local cname = lib.caller_name(caller)
		if type(cname) == "string" and cname ~= "" and cname ~= "root" then
			local st = mare.plugins and mare.plugins.status and mare.plugins.status(cname)
			if type(st) == "table" and st.origin == "builtin" then
				return "plugin:" .. cname, "default"
			end
			return "plugin:" .. cname, "plugin"
		end
		return "runtime", "plugin"
	end

	local core_actions_cache = nil
	local function is_core_action(name)
		if not core_actions_cache then
			core_actions_cache = {}
			for _, a in ipairs(mare.keymap.actions()) do
				if a.core then
					core_actions_cache[a.name] = true
				end
			end
		end
		return core_actions_cache[name] == true
	end

	--- set_keymap(context, key, action, opts?) -> disposer
	function service.set_keymap(_, caller, context, key, action, opts)
		local lib = require("mare_lib")
		opts = type(opts) == "table" and opts or {}
		if (type(action) ~= "string" and action ~= false) or (type(action) == "boolean" and action ~= false) then
			lib.fail("invalid_arguments", "ui.set_keymap: action must be a string or false")
		end
		local norm = U.normalize_key(key)
		if mare.keymap.is_reserved(norm) then
			lib.fail("invalid_arguments", ("mare.keymap.set: %s is reserved by the core"):format(norm))
		end
		local owner, layer = caller_source(lib, caller)
		local ok, handle = pcall(mare.keymap.set, context, key, action, {
			when = opts.when,
			unique = opts.unique,
			priority = opts.priority,
			desc = opts.desc,
			owner = owner,
			layer = layer,
		})
		if not ok then
			lib.fail("invalid_arguments", tostring(handle))
		end
		local norm_key = handle.key or norm
		ctx.bus:emit("ui/keymap-changed", {
			context = context,
			key = norm_key,
			action = type(action) == "string" and action or false,
		})
		return lib.owned(caller, function()
			mare.keymap.remove(handle)
			ctx.bus:emit("ui/keymap-changed", {
				context = context,
				key = norm_key,
			})
		end, "ui keymap " .. tostring(context) .. " " .. tostring(norm_key))
	end

	--- delete_keymap(context, key) -> { context, key }
	function service.delete_keymap(_, caller, context, key)
		local lib = require("mare_lib")
		local owner, layer = caller_source(lib, caller)
		local ok, removed, norm_key = pcall(mare.keymap.del, context, key, {
			owner = owner,
			layer = layer,
		})
		if not ok then
			lib.fail("invalid_arguments", tostring(removed))
		end
		if not removed then
			lib.fail("ui.unknown_keymap", ("no runtime keymap binding matched %s in %s"):format(norm_key, context))
		end
		ctx.bus:emit("ui/keymap-changed", {
			context = context,
			key = norm_key,
		})
		return { context = context, key = norm_key }
	end

	local function unwrap_shell(lib, res, allow_empty)
		if type(res) ~= "table" then
			lib.fail("internal", "invalid rpc shell response")
		end
		if res.empty_reason and allow_empty then
			return nil, res.empty_reason
		end
		if res.error then
			lib.fail(res.error.code or "internal", res.error.message or "rpc shell error")
		end
		return res.value
	end

	local function submit_editor_internal(text, opts, allow_empty_from_keymap)
		local lib = require("mare_lib")
		opts = type(opts) == "table" and opts or {}
		local mode = opts.mode or "prompt"
		if mode ~= "prompt" and mode ~= "steer" and mode ~= "follow_up" then
			lib.fail("invalid_arguments", ("unknown editor submit mode `%s`"):format(tostring(mode)))
		end
		if U.frontend and U.frontend.send_editor then
			local raw = U.frontend.send_editor(text, mode, opts.attachments, allow_empty_from_keymap == true)
			return unwrap_shell(lib, raw, allow_empty_from_keymap == true)
		end
		local ed = type(U.get_editor) == "function" and U.get_editor() or ""
		local submitted = type(text) == "string" and text or ed
		local atts = type(opts.attachments) == "table" and opts.attachments or {}
		if not submitted:match("%S") and #atts == 0 then
			if allow_empty_from_keymap then
				return { revision = 0, handled = false }, "editor buffer is empty"
			end
			lib.fail("invalid_arguments", "submitted editor text must not be empty")
		end
		if type(text) ~= "string" and type(U.set_editor) == "function" then
			U.set_editor("")
		end
		local ev = { text = submitted, mode = mode, handled = false }
		ctx.bus:emit("input/submit", ev)
		local handled = ev.handled == true
		local res
		if not handled then
			local ag = ctx:get("agent")
			if ag and ag[mode] then
				res = ag[mode](ag, submitted, { source = "rpc", attachments = #atts > 0 and atts or nil })
			end
		end
		return { revision = 0, handled = handled, result = type(res) == "table" and res or nil }
	end

	--- call_keymap(context?, key?, action?) -> dispatch result
	function service.call_keymap(_, _, context, key, action)
		local lib = require("mare_lib")
		local target_ctx = context or "editor"
		local function run_core(act)
			if act == "interrupt" then
				local ag = ctx:get("agent")
				if ag and ag.abort then
					pcall(ag.abort, ag)
				end
				return true, nil
			elseif act == "editor.submit" then
				local r, empty_reason = submit_editor_internal(nil, { mode = "prompt" }, true)
				if empty_reason then
					return false, empty_reason
				end
				return r ~= nil, nil
			elseif is_core_action(act) then
				return false, "frontend_owned"
			else
				lib.fail("invalid_arguments", ("unknown keymap action `%s`"):format(tostring(act)))
			end
		end

		if action ~= nil then
			if type(action) ~= "string" or not action:match("%S") then
				lib.fail("invalid_arguments", "action must not be empty")
			end
			local handled_lua = mare.keymap.invoke(action, { key = key, context = target_ctx })
			if handled_lua ~= nil then
				return {
					handled = handled_lua,
					declined = not handled_lua,
					masked = false,
					context = target_ctx,
					action = action,
				}
			end
			local handled, reason = run_core(action)
			return {
				handled = handled,
				declined = false,
				masked = false,
				context = target_ctx,
				action = action,
				reason = reason,
			}
		end

		if type(key) ~= "string" or not key:match("%S") then
			lib.fail("invalid_arguments", "ui.call_keymap requires either `key` or `action`")
		end
		local norm_seq = U.normalize_key(key)
		local sh = U.frontend
		local ed_empty = (sh and sh.editor_is_empty and sh.editor_is_empty())
			or (not sh and type(U.editor_state) == "function" and U.editor_state().empty)
			or false
		local wk = (sh and sh.is_working and sh.is_working()) or false
		local ok, resolved = pcall(mare.keymap.resolve, target_ctx, norm_seq, {
			editor_empty = ed_empty,
			working = wk,
		})
		if not ok then
			lib.fail("invalid_arguments", tostring(resolved))
		end
		local declined = false
		for _, cand in ipairs(resolved.candidates) do
			local r = mare.keymap.invoke(cand.action, { key = resolved.key, context = cand.context })
			if r == true then
				return {
					handled = true,
					declined = false,
					masked = false,
					context = cand.context,
					action = cand.action,
				}
			elseif r == false then
				declined = true
			else
				local handled, reason = run_core(cand.action)
				return {
					handled = handled,
					declined = false,
					masked = false,
					context = cand.context,
					action = cand.action,
					reason = reason,
				}
			end
		end
		return {
			handled = false,
			declined = declined,
			masked = resolved.masked == true,
			context = target_ctx,
			reason = resolved.masked and "masked" or nil,
		}
	end

	--- update_editor(text, opts?) -> synchronized editor state
	function service.update_editor(_, _, text, opts)
		local lib = require("mare_lib")
		opts = type(opts) == "table" and opts or {}
		if U.frontend and U.frontend.update_editor then
			return unwrap_shell(lib, U.frontend.update_editor(text, opts), false)
		end
		if type(U.set_editor) == "function" then
			U.set_editor(text)
		end
		return {
			text = text,
			offset = opts.offset or #text,
			shell_mode = opts.shell_mode == true,
			image_markers = opts.image_markers,
			attachments = opts.attachments,
			external_editor_running = false,
			revision = 0,
		}
	end

	--- send_editor(text?, opts?) -> { revision, handled, result? }
	function service.send_editor(_, _, text, opts)
		local res = submit_editor_internal(text, opts, false)
		return res
	end

	--- set_external_editor(action, text?) -> { running, text, revision, program? }
	function service.set_external_editor(_, _, action, text)
		local lib = require("mare_lib")
		if U.frontend and U.frontend.set_external_editor then
			return unwrap_shell(lib, U.frontend.set_external_editor(action, text), false)
		end
		lib.fail("unavailable", "external editor control is only available on an RPC host")
	end

	--- get_select_preview(select_id, index, tab?) -> { preview? }
	function service.get_select_preview(_, caller, select_id, index, tab)
		local lib = require("mare_lib")
		if type(select_id) ~= "string" or select_id == "" or math.type(index) ~= "integer" or index < 0 then
			lib.fail("invalid_arguments", "select_id must be a non-empty string and index a non-negative integer")
		end
		if tab ~= nil and (math.type(tab) ~= "integer" or tab < 0) then
			lib.fail("invalid_arguments", "tab must be a non-negative integer")
		end
		local caller_id = lib.rpc_caller_id(caller)
		local res = caller_id
			and U.frontend
			and U.frontend.get_select_preview
			and U.frontend.get_select_preview(caller_id, select_id, index, tab or 0)
		if type(res) ~= "table" or not res.found then
			lib.fail("ui.unknown_select", "unknown or inactive select: " .. tostring(select_id))
		end
		if res.preview ~= nil then
			return { preview = res.preview }
		end
		return {}
	end

	--- action(name, fn, opts) — a plugin-scoped named action (mare.keymap.action), gone when
	--- the plugin is disposed; any default `opts.keys` bound by the plugin are removed with it.
	function service.action(_, caller, name, fn, opts)
		local lib = require("mare_lib")
		local owner, layer = caller_source(lib, caller)
		local act_opts = {}
		if type(opts) == "table" then
			for k, v in pairs(opts) do
				act_opts[k] = v
			end
		end
		act_opts.owner = owner
		act_opts.layer = layer
		local h = mare.keymap.action(name, fn, act_opts)
		caller:effect(function()
			return h.remove
		end, "ui action " .. tostring(name))
		return h
	end

	--- context(name, opts) — a plugin-scoped keymap subcontext (mare.keymap.context), gone
	--- along with its descendants and bindings when the plugin is disposed
	function service.context(_, caller, name, opts)
		local h = mare.keymap.context(name, opts)
		caller:effect(function()
			return h.remove
		end, "ui context " .. tostring(name))
		return h
	end

	--- renderer(kind, r) — plugin-scoped mare.ui.renderer
	function service.renderer(_, caller, kind, r)
		local remove = U.renderer(kind, r)
		caller:effect(function()
			return remove
		end, "ui renderer " .. tostring(kind))
	end

	--- register_entry_renderer(kind, r) — plugin- and RPC-scoped entry renderer registration
	function service.register_entry_renderer(_, caller, kind, r)
		local lib = require("mare_lib")
		if type(kind) ~= "string" or not kind:match("%S") then
			lib.fail("invalid_arguments", "entry kind must be a non-empty string")
		end
		local target = r
		if lib.rpc_caller_id(caller) ~= nil then
			if type(r) ~= "table" or type(r.render) ~= "function" then
				lib.fail("invalid_arguments", "entry renderer requires render callback")
			end
			local cb = r.render
			target = function(entry, opts)
				local expanded = type(opts) == "table" and opts.expanded == true or false
				return lib.normalize_render_result(cb(kind, entry, expanded), { wire = true })
			end
		end
		return lib.owned(caller, U.entry_renderer(kind, target), "ui entry renderer " .. tostring(kind))
	end

	--- register_markdown_transform(opts) — plugin- and RPC-scoped Markdown transform registration
	function service.register_markdown_transform(_, caller, opts)
		local lib = require("mare_lib")
		opts = type(opts) == "table" and opts or {}
		local fn = opts.transform
		if type(fn) ~= "function" then
			lib.fail("invalid_arguments", "`transform` callback is required")
		end
		local wrapped = function(text, md_ctx)
			local out = fn(text, md_ctx)
			if type(out) == "table" then
				return out.text
			end
			return out
		end
		return lib.owned(
			caller,
			U.markdown_transform(wrapped, { message_types = opts.message_types }),
			"ui markdown transform"
		)
	end

	--- transform_markdown(text, context?) — run registered markdown transformers
	function service.transform_markdown(_, _, text, context)
		local out = U.transform_markdown(text, context)
		if mare.projection and mare.projection.active() then
			out = mare.projection.project(out, { lossy = true })
		end
		return { text = out }
	end

	--- style(name, spec) — plugin-scoped mare.ui.style; the previous value returns on dispose
	function service.style(_, caller, name, spec)
		local prev = U.style(name)
		U.style(name, spec)
		caller:effect(function()
			return function()
				U.reset_style(name, prev)
			end
		end, "ui style " .. tostring(name))
	end

	--- register_override(name, fn) — plugin-scoped mare.ui.override; released when caller is disposed
	function service.register_override(_, caller, name, fn)
		local release = U.override(name, fn)
		caller:effect(function()
			return release
		end, "ui override " .. tostring(name))
		return release
	end

	--- completion_register(spec) — plugin-scoped mare.ui.completion.register; retracted on dispose
	function service.completion_register(_, caller, spec)
		local release = U.completion.register(spec)
		caller:effect(function()
			return release
		end, "ui completion " .. tostring(type(spec) == "table" and spec.name or ""))
		return release
	end

	-- passthrough helpers (dynamic lookup: replaced mare.ui.select etc. are honoured)
	for _, k in ipairs({
		"notify",
		"select",
		"input",
		"confirm",
		"editor",
		"get_editor",
		"editor_state",
		"editor_cursor",
		"set_editor_cursor",
		"editor_selection",
		"set_editor_selection",
		"editor_insert",
		"editor_replace",
		"questions",
		"overlay",
		"print",
		"set_editor",
		"editor_hint",
		"editor_config",
		"transcript",
		"transcript_title",
		"transcript_close",
		"flag",
		"set_queued",
		"set_queue_mode",
		"keymap",
		"theme",
		"clear",
		"exit",
		"exit_guard",
		"render_text",
		"working",
		"width",
	}) do
		service[k] = function(_, _, ...)
			return U[k](...)
		end
	end

	--- card(spec) — emit a durable, reflowable titled markdown card in the transcript.
	---@param spec { title: string, markdown: string, kind?: string, card_id?: string, persist?: boolean, after_entry_id?: string, meta?: table }
	function service.card(_, _, spec)
		if type(spec) ~= "table" then
			return nil, "ui.card: spec table required"
		end
		local title = type(spec.title) == "string" and spec.title or ""
		local markdown = type(spec.markdown) == "string" and spec.markdown or ""
		local kind = type(spec.kind) == "string" and spec.kind or "info"
		local card_id = spec.card_id
		local after_entry_id = spec.after_entry_id
		if spec.persist then
			local sess = ctx:get("session")
			if
				not sess
				or not require("mare_lib").callable(sess.add_record)
				or not require("mare_lib").callable(sess.info)
			then
				return nil, "ui.card: session service unavailable for persistent card"
			end
			local owner = sess:info()
			if
				type(owner) ~= "table"
				or type(owner.session_id) ~= "string"
				or type(owner.session_epoch) ~= "string"
			then
				return nil, "ui.card: active session owner unavailable"
			end
			if after_entry_id == nil and sess.leaf then
				after_entry_id = sess:leaf()
			end
			local ok, err = pcall(sess.add_record, sess, "ui/card", {
				card_id = card_id,
				title = title,
				markdown = markdown,
				kind = kind,
				after_entry_id = after_entry_id,
				meta = spec.meta,
			}, {
				persist = true,
				sync = true,
				session_id = owner.session_id,
				session_epoch = owner.session_epoch,
			})
			if not ok then
				return nil, (type(err) == "table" and err.message) or err or "ui.card: failed to persist card"
			end
			local after = sess:info()
			if
				type(after) ~= "table"
				or after.session_id ~= owner.session_id
				or after.session_epoch ~= owner.session_epoch
			then
				return nil, "ui.card: session owner changed before card publication"
			end
		end
		local block_id
		if U.connected and U.connected() then
			block_id = U.push(kind, { title = title, markdown = markdown })
			U.finish(block_id)
		end
		ctx.bus:emit("ui/card", {
			card_id = card_id,
			block_id = block_id,
			title = title,
			markdown = markdown,
			kind = kind,
			persistent = spec.persist == true,
			after_entry_id = after_entry_id,
			meta = spec.meta,
		})
		return block_id or true
	end

	--- handler(kind, name, {order}, fn) — an ordered handler of `input/submit` ("submit") or
	--- `key/interrupt` ("interrupt"), ascending `order`, equal orders in
	--- registration order. A submit stops at the first handler that returns true; an interrupt
	--- reaches every handler (one Esc stops everything running) and is handled if any returned
	--- true. None handling, ui's own routing runs. An effect of the caller; returns its removal.
	local chains = { submit = require("mare_lib").ordered(), interrupt = require("mare_lib").ordered() }
	function service.handler(_, caller, kind, name, opts, fn)
		local chain = chains[kind]
		assert(chain, 'ui.handler: kind must be "submit" or "interrupt"')
		assert(type(name) == "string" and name ~= "", "ui.handler: name must be a non-empty string")
		assert(type(opts) == "table" and type(opts.order) == "number", "ui.handler: opts.order must be a number")
		assert(type(fn) == "function", "ui.handler: fn must be a function")
		local remove = chain.add(opts.order, { name = name, fn = fn })
		caller:effect(function()
			return remove
		end, ("ui handler %s %s"):format(kind, name))
		return remove
	end
	--- whether the registered handlers of `kind` handled `ev` (a failing handler is logged and skipped)
	local function run_handlers(kind, ev)
		local traverse = kind == "interrupt" and chains[kind].all or chains[kind].first
		return traverse(function(h)
			local ok, result = pcall(h.fn, ev)
			if not ok then
				ctx.logger:warn(("ui.handler %s %s: %s"):format(kind, h.name, tostring(result)))
				return false
			end
			return result == true
		end) and true or false
	end
	-- the chains run on every frontend, with or without a UI attached (RPC `commands.run` and
	-- headless go through input/submit too); ui's own routing (ui.input, attached UIs only) is
	-- registered after these and sees `ev.handled`
	for kind, event in pairs({ submit = "input/submit", interrupt = "key/interrupt" }) do
		ctx:on(event, function(ev)
			if type(ev) == "table" and not ev.handled and run_handlers(kind, ev) then
				ev.handled = true
			end
		end)
	end

	require("ui.images")(D, service)
	local lib = require("mare_lib")
	D.has_markdown_presenters = function()
		return false
	end
	D.invalidate_presentations = function()
		ctx.bus:emit("ui/presentation-invalidate", {})
		return {}
	end
	local ensure_presentations = lib.lazy_group(service, {
		"markdown_presenter",
		"render_markdown",
		"delete_markdown",
		"get_markdown",
		"list_markdown_presenters",
		"cancel_markdown",
	}, function()
		require("ui.presentations")(D, service)
	end, {
		extra_target = D,
		extra_fields = { "native_markdown_document" },
		fast_unloaded = {
			list_markdown_presenters = function()
				return { data = mare.json.array() }
			end,
		},
	})
	D.ensure_presentations = ensure_presentations

	-- a mare.Service: `caller` in a method is the calling plugin's ctx, so a
	-- registration is an effect of the caller's fiber
	lib.service(ctx, "ui", service)

	local function redraw_all()
		for _, d in pairs(widgets) do
			d.draw()
		end
		U.status_redraw()
	end
	ctx:on("ui/resize", redraw_all)
	ctx:on("theme/changed", redraw_all)

	local working_applied = false
	local lazy_working_proxy, base_working
	local function ensure_working(o)
		if not working_applied then
			working_applied = true
			if lazy_working_proxy and rawequal(U.Working, lazy_working_proxy) then
				U.Working = base_working
			end
			require("ui.working").apply(o or mare.o or mare.config or {})
		end
	end
	local function apply_working_shell(o)
		local raw_ui
		if rawequal(o, mare.o) then
			raw_ui = type(mare.config) == "table" and type(mare.config.ui) == "table" and mare.config.ui or nil
		else
			raw_ui = type(o.ui) == "table" and o.ui or nil
		end
		local custom = raw_ui ~= nil and (raw_ui.working ~= nil or raw_ui.tool_call ~= nil or raw_ui.stopping ~= nil)
		if working_applied or custom then
			if lazy_working_proxy and rawequal(U.Working, lazy_working_proxy) then
				U.Working = base_working
			end
			working_applied = true
			require("ui.working").apply(o)
			return
		end
		-- Default `ui.working` (`tools = "still"`): match `ui.working.apply` without loading
		-- `ui.working`, `ui.working_looks`, and `ui.working_draw` until first access or turn.
		U.ToolCall.still = true
		U.ToolCall.spinner = nil
		if lazy_working_proxy == nil or not rawequal(U.Working, lazy_working_proxy) then
			base_working = U.Working
			lazy_working_proxy = setmetatable({}, {
				__metatable = false,
				__index = function(_, k)
					ensure_working()
					return U.Working[k]
				end,
				__newindex = function(_, k, v)
					ensure_working()
					U.Working[k] = v
				end,
				__pairs = function()
					ensure_working()
					return pairs(U.Working)
				end,
			})
			U.Working = lazy_working_proxy
		end
	end

	-- interface shell options: border charset, icon set, glyphs, markdown
	local function apply_shell()
		local o = mare.o or mare.config or {}
		local color_opt = type(o.terminal) == "table" and o.terminal.color or nil
		if U.set_color_mode then
			U.set_color_mode(color_opt)
		end
		local icons = o.icons
		if icons == nil or icons == "auto" then
			icons = U.detect_icons()
		end
		local sp = o.spinner
		if sp == nil then
			sp = false
		elseif type(sp) == "table" and sp.frames == nil then
			sp = { frames = sp }
		end
		U.set_theme_data({
			border = o.border or "rounded",
			icons = icons,
			glyphs = sp,
			markdown = o.markdown or {},
		})
		local term = type(o.terminal) == "table" and o.terminal or nil
		U.set_ambiwidth((term and term.ambiwidth) or "single")
		-- no spinner animation: a static marker and static status text
		U.flag("reduce_motion", o.reduce_motion == true)
		-- the working line's style (ui.working; default the spinner),
		-- and whether running tool rows keep their own spinner
		apply_working_shell(o)
		-- stacked region layout (`mare.o.ui.layout`)
		local function plain_copy(v)
			if type(v) ~= "table" then
				return v
			end
			local out = {}
			for k, item in pairs(v) do
				out[k] = plain_copy(item)
			end
			return out
		end
		local layout = type(o.ui) == "table" and plain_copy(o.ui.layout) or nil
		local ok, err = pcall(U.set_layout, layout, { persist = false })
		if not ok then
			mare.log("error", "ui.layout: " .. tostring(err))
			U.notify("ui.layout: " .. tostring(err), "error")
		end
		redraw_all()
	end
	apply_shell()
	local SHELL_OPTS = {
		border = true,
		icons = true,
		markdown = true,
		terminal = true,
		spinner = true,
		reduce_motion = true,
		ui = true,
	}
	ctx:on("option/changed", function(ev)
		if SHELL_OPTS[ev.name] then
			apply_shell()
		end
	end)
	ctx:on("kernel/reloaded", redraw_all)
	-- the capability table arrives after the first frame: a terminal that draws emoji one
	-- cell wide (`emoji_wide = false`) gets the ASCII icons when icons = "auto"
	-- ui/caps is a waterfall (`function(caps, ev)` with `ev.next()`): ui reads the table as later listeners leave it
	ctx:on("ui/caps", function(caps, ev)
		caps = ev.next()
		local o = mare.o or mare.config or {}
		if (o.icons == nil or o.icons == "auto") and type(caps) == "table" then
			local icons = U.detect_icons(caps)
			if icons ~= U.icon_set() then
				U.set_theme_data({ icons = icons })
				redraw_all()
			end
		end
		return caps
	end)

	return {
		service = service,
		redraw_status = function()
			U.status_redraw()
		end,
	}
end
