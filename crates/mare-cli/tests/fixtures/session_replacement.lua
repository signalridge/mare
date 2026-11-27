return {
  name = "session", override = true, provide = { "session" },
  apply = function(ctx)
    local all, cur, n = {}, nil, 0
    local state_file = nil
    if mare.mode ~= "child" and mare.paths and mare.paths.state then
      state_file = mare.paths.state .. "/stub_sessions.json"
    end
    local function load_state()
      if not state_file then return end
      local f = io.open(state_file, "r")
      if not f then return end
      local raw = f:read("*a")
      f:close()
      local ok, data = pcall(mare.json.decode, raw)
      if ok and type(data) == "table" then
        all = type(data.all) == "table" and data.all or {}
        n = tonumber(data.n) or #all
      end
    end
    local function save_state()
      if not state_file then return end
      local f = io.open(state_file, "w")
      if not f then return end
      f:write(mare.json.encode({ n = n, all = all }))
      f:close()
    end
    local function open(prefix)
      load_state()
      n = n + 1
      cur = { id = prefix .. n, path = "/stub/" .. prefix .. n .. ".jsonl" }
      all[#all + 1] = cur
      save_state()
    end
    if mare.mode == "child" then
      open("stub-")
      cur.id = mare.args.session_id or "missing-child-session-id"
    elseif mare.args and mare.args.fork_session then
      open("fork-")
    elseif mare.args and type(mare.args.resume) == "string" and mare.args.resume ~= "" then
      load_state()
      for _, s in ipairs(all) do
        if s.path == mare.args.resume or s.id == mare.args.resume then
          cur = s
          break
        end
      end
      if not cur then open("stub-") end
    else
      open("stub-")
    end
    local impl = {
      commit = function() return true end,
      append_compaction = function() return true end,
      append_prune = function() return true end,
      set_sandbox_mode = function(_, spec) return mare.proc.sandbox.set_current(spec) end,
      info = function() return { session_id = cur.id, session_epoch = cur.id .. "#0:0", path = cur.path, count = 0 } end,
      available = function() return true end,
      switching = function() return false end,
      child = function() return nil end,
      children = function() return {} end,
      short_id = function(_, id) return id end,
      load_transcript = function() end,
      prepare_fork = function() return { path = cur.path } end,
      get = function()
        return { session_id = cur.id, session_epoch = cur.id .. "#0:0", cwd = mare.cwd, path = cur.path, loaded = true, created_at_ms = 0, updated_at_ms = 0 }
      end,
      list = function()
        load_state()
        local out = {}
        for i, s in ipairs(all) do
          out[i] = {
            session_id = s.id,
            session_epoch = (s.id == cur.id) and (s.id .. "#0:0") or nil,
            cwd = mare.cwd,
            path = s.path,
            loaded = (s.id == cur.id),
            created_at_ms = 0,
            updated_at_ms = 0,
          }
        end
        return { data = out }
      end,
      entries = function() return { { entry_id = "e1", type = "message", from = cur.id } } end,
      list_items = function() return { data = { { item_id = "e1", entry_id = "e1", type = "assistant_message", session_id = cur.id } }, leaf_entry_id = "e1" } end,
      add_record = function() return { record_id = "r1", durable = false } end,
      list_records = function() return { data = {} } end,
      leaf = function() return "e1" end,
      entry_id = function() return nil end,
      file_changes = function() return {} end,
      path_to = function() return {} end,
      set_leaf = function() return true end,
      update = function(_, name)
        cur.name = name
        return { session_id = cur.id, session_epoch = cur.id .. "#0:0", name = cur.name, cwd = mare.cwd, path = cur.path, loaded = true, created_at_ms = 0, updated_at_ms = 0 }
      end,
      label_item = function() return { item_id = "e1" } end,
      archive = function() return {} end,
      unarchive = function() return {} end,
      delete = function() return {} end,
      rewind = function() return { session_id = cur.id, session_epoch = cur.id .. "#0:0", cwd = mare.cwd, path = cur.path, loaded = true, leaf_entry_id = "e1", created_at_ms = 0, updated_at_ms = 0 } end,
      transition = function() return { status = "rejected", error = "unsupported" } end,
      persist_message = function() return "mock-msg" end,
      record_tool_state = function() return "mock-ts" end,
      record_request = function() return "mock-req" end,
      record_request_end = function() return "mock-req-end" end,
      record_request_aborted = function() return "mock-req-abort" end,
      record_run_end = function() return true end,
      baseline_cut = function() return { parts = {} } end,
      snapshot = function() return { session_id = cur.id, path = cur.path, leaf_entry_id = "e1" } end,
      snapshot_context = function() return { entries = {}, message_count = 0 } end,
      search = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
      search_items = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
      get_item = function() return {} end,
      trace = function() return {} end,
      trace_item = function() return {} end,
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
