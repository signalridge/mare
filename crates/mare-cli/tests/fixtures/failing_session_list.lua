-- A replacement `session` service whose listing fails with a secret-looking message (peers.rs).
-- MARKER (a path) is prepended by the test.
return {
	name = "session",
	override = true,
	provide = { "session" },
	apply = function(ctx)
		local all = {
			{
				id = "11111111-0000-0000-0000-000000000001",
				path = "/stub/first.jsonl",
				title = "22222222-0000-0000-0000-000000000002",
			},
			{ id = "22222222-0000-0000-0000-000000000002", path = "/stub/second.jsonl", title = "shared-name" },
			{ id = "11111111-9999-0000-0000-000000000003", path = "/stub/third.jsonl", title = "other" },
		}
		local cur = all[1]
		local impl = {
			commit = function()
				return true
			end,
			append_compaction = function()
				return true
			end,
			append_prune = function()
				return true
			end,
			set_sandbox_mode = function(_, spec)
				return mare.proc.sandbox.set_current(spec)
			end,
			info = function()
				return { session_id = cur.id, session_epoch = cur.id .. "#0:0", path = cur.path, count = 0 }
			end,
			available = function()
				return true
			end,
			switching = function()
				return false
			end,
			child = function()
				return nil
			end,
			children = function()
				return {}
			end,
			short_id = function(_, id)
				return id
			end,
			load_transcript = function() end,
			get = function()
				return {
					session_id = cur.id,
					cwd = mare.cwd,
					path = cur.path,
					loaded = true,
					created_at_ms = 0,
					updated_at_ms = 0,
				}
			end,
			list = function()
				mare.fs.write(MARKER, "hit")
				error("SECRET-MARKER-7f3 at /secret/index/path", 0)
			end,
			entries = function()
				return {}
			end,
			list_items = function()
				return { data = {} }
			end,
			add_record = function()
				return { record_id = "r1", durable = false }
			end,
			list_records = function()
				return { data = {} }
			end,
			leaf = function()
				return nil
			end,
			entry_id = function()
				return nil
			end,
			file_changes = function()
				return {}
			end,
			path_to = function()
				return {}
			end,
			set_leaf = function()
				return true
			end,
			update = function()
				return { session_id = cur.id, cwd = mare.cwd, loaded = true, created_at_ms = 0, updated_at_ms = 0 }
			end,
			label_item = function()
				return { item_id = "e1" }
			end,
			archive = function()
				return {}
			end,
			unarchive = function()
				return {}
			end,
			delete = function()
				return {}
			end,
			rewind = function()
				return cur
			end,
			transition = function()
				return { status = "rejected", error = "unsupported" }
			end,
			persist_message = function()
				return "mock-msg"
			end,
			record_tool_state = function()
				return "mock-ts"
			end,
			record_request = function()
				return "mock-req"
			end,
			record_request_end = function()
				return "mock-req-end"
			end,
			record_request_aborted = function()
				return "mock-req-abort"
			end,
			record_run_end = function()
				return true
			end,
			baseline_cut = function()
				return { parts = {} }
			end,
			snapshot = function()
				return {}
			end,
			snapshot_context = function()
				return { entries = {}, message_count = 0 }
			end,
			search = function()
				return { data = {}, corpus_revision = "g0", incomplete = false }
			end,
			search_items = function()
				return { data = {}, corpus_revision = "g0", incomplete = false }
			end,
			get_item = function()
				return {}
			end,
			trace = function()
				return {}
			end,
			trace_item = function()
				return {}
			end,
		}
		local Session = require("cordis").class("Session", mare.Service)
		function Session:constructor(c)
			Session.super.constructor(self, c, "session")
		end
		-- every contract method on the class, `new` included (cordis-lua constructs a class whose
		-- method is named `new`, b74525e9)
		for name, fn in pairs(impl) do
			Session[name] = fn
		end
		ctx:plugin(Session)
	end,
}
