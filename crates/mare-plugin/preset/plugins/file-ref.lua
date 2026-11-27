-- file-ref: `@path` completion in the editor; referenced files are attached to the prompt.
-- `@"path with spaces"` quotes a path. A reference that can't be
-- attached (missing, too large, binary, denied, over the total budget) is never dropped silently:
-- a one-line note says why and points at the read tool.
return {
	name = "file-ref",
	-- `policy` decides whether a referenced file may be read
	inject = { "policy" },
	config_schema = {
		max_bytes = { type = "integer", default = 100 * 1024, desc = "Byte cap per referenced file." },
		max_total_bytes = {
			type = "integer",
			default = 256 * 1024,
			desc = "Byte cap of all files of one message together.",
		},
		limit = { type = "integer", default = 30, desc = "Completion items." },
		refresh_ms = {
			type = "integer",
			default = 1000,
			desc = "The file index checks for changes at most this often.",
		},
	},
	apply = function(ctx, cfg)
		-- the optional permissions service, read on each call
		local svc = require("mare_lib").followers(ctx, { "permissions" })
		-- built on the first `@`, then kept current incrementally (mare.fs.index: one stat per
		-- known directory, changed directories re-listed —), not rescanned
		local indexes, files = nil, nil -- { {idx, prefix, gen} }, the combined list

		local function all_files()
			if not indexes then
				indexes = { { idx = mare.fs.index(".", { limit = 50000, dirs = true }), prefix = "" } }
				-- extra workspace directories (/add-dir): absolute paths
				if mare.workspace then
					for _, d in ipairs(mare.workspace.dirs()) do
						if d ~= mare.cwd then
							indexes[#indexes + 1] =
								{ idx = mare.fs.index(d, { limit = 20000, dirs = true }), prefix = d .. "/" }
						end
					end
				end
			end
			local changed = files == nil
			for _, x in ipairs(indexes) do
				local gen = x.idx:refresh({ min_interval_ms = cfg.refresh_ms })
				changed = changed or gen ~= x.gen
				x.gen = gen
			end
			if changed then
				files = {}
				for _, x in ipairs(indexes) do
					for _, p in ipairs(x.idx:files()) do
						files[#files + 1] = x.prefix .. p
					end
				end
			end
			return files
		end

		ctx:on("workspace/changed", function()
			indexes, files = nil, nil
		end)

		ctx:inject({ "ui" }, function(sub)
			sub.ui:completion_register({
				name = "files",
				triggers = { "@" },
				min_prefix = 0,
				score_offset = 5,
				enabled = function(ev)
					return (not ev.shell_mode or ev.force) and string.sub(ev.token or "", 1, 1) == "@"
				end,
				complete = function(ev)
					local tok = ev.token or ""
					-- `@"my dir/fi`: the quote only opens a path with spaces
					local q = string.gsub(string.sub(tok, 2), '^"', "")
					local list = all_files() or {}
					local matched
					if q == "" then
						matched = {}
						local limit = math.min(#list, cfg.limit)
						for i = 1, limit do
							matched[i] = list[i]
						end
					else
						matched = mare.text.fuzzy(q, list, cfg.limit)
					end
					local items = {}
					for _, p in ipairs(matched) do
						local ref = string.find(p, "%s") and ('@"' .. p .. '"') or ("@" .. p)
						items[#items + 1] = {
							label = ref,
							insert = ref .. (string.sub(p, -1) == "/" and "" or " "),
							icon = ui.file_icon(p),
							style = ui.filetype_style(p),
						}
					end
					return items
				end,
			})
		end)

		--- the `@` references of `text`, in order: @"quoted path", @'quoted path', @plain
		local function refs_of(text)
			local out, i = {}, 1
			text = " " .. text
			while true do
				local s, e = string.find(text, "%s@", i)
				if not s then
					break
				end
				local q = string.sub(text, e + 1, e + 1)
				local close = (q == '"' or q == "'") and string.find(text, q, e + 2, true) or nil
				if close then
					out[#out + 1] = string.sub(text, e + 2, close - 1)
					i = close + 1
				else
					local word = string.match(text, "^[^%s]+", e + 1)
					if word then
						out[#out + 1] = (string.gsub(word, "[,.;:!?)]+$", ""))
					end
					i = e + 1 + #(word or "")
				end
			end
			return out
		end

		local function check_read_policy(target)
			local function sandbox_allows(spec)
				local okc, allowed_sb, reason = pcall(mare.proc.sandbox.check, "read", target, spec, mare.cwd)
				if not okc then
					return false, tostring(allowed_sb)
				end
				if not allowed_sb then
					return false, tostring(reason or "denied by sandbox policy")
				end
				return true
			end
			local okp, verdict = pcall(function()
				return ctx.policy:decide(
					"tool/before",
					table.pack({
						id = "file-ref",
						name = "read",
						arguments = { path = target },
						cwd = mare.cwd,
					}, { side_effect = "read", read_only = true, unconfined = false })
				)
			end)
			if not okp or type(verdict) ~= "table" then
				return false, tostring(verdict)
			end
			if verdict.decision ~= "allow" then
				return false, verdict.reason or "denied by permissions policy"
			end
			if svc.permissions and not verdict.authority_approved then
				return false, "permissions provider did not adjudicate read access"
			end
			local policies = {}
			local cur_spec, cur_snapshot = mare.proc.sandbox.snapshot_current()
			local cur = (cur_snapshot and cur_snapshot ~= false) and cur_snapshot or nil
			local policy = verdict.sandbox_grant or cur
			if policy ~= nil then
				local okp_sb, err = sandbox_allows(policy)
				if not okp_sb then
					return false, err
				end
				policies[#policies + 1] = policy
			end
			if
				verdict.sandbox_grant ~= nil
				and cur ~= nil
				and type(cur_spec) == "table"
				and (cur_spec.restrict_read or (cur_spec.deny_read and #cur_spec.deny_read > 0))
			then
				local okc_sb, err = sandbox_allows(cur)
				if not okc_sb then
					return false, err
				end
				policies[#policies + 1] = cur
			end
			for _, sb in ipairs(verdict.sandboxes or {}) do
				local oksb, err = sandbox_allows(sb)
				if not oksb then
					return false, err
				end
				policies[#policies + 1] = sb
			end
			return true, nil, policies
		end

		--- (block, bytes) for one reference; a note instead of the contents when it can't be attached
		local function attach(p, left)
			local function skip(why)
				return ("[@%s not attached: %s — use the read tool if it is needed]"):format(p, why), 0
			end
			local allowed, deny_why, policies = check_read_policy(p)
			if not allowed then
				return skip(deny_why)
			end
			local ok, st = pcall(mare.fs.stat, p)
			if not ok then
				return skip(tostring(st))
			end
			if not st then
				return skip("no such file or directory")
			end
			local ok_real, real = pcall(mare.fs.realpath, p)
			local target = (ok_real and type(real) == "string" and real ~= "") and real or p
			if target ~= p then
				local r_allowed, r_why, r_policies = check_read_policy(target)
				if not r_allowed then
					return skip(r_why)
				end
				policies = r_policies
			end
			if st.kind == "dir" then
				local budget = math.min(cfg.max_bytes, left)
				local header = ('<directory path="%s">\n'):format(p)
				local footer = "\n</directory>"
				if #header + #footer > left then
					return skip(("the attachments of this message would exceed %d bytes"):format(cfg.max_total_bytes))
				end
				local okl, list, list_err = pcall(mare.fs.list, target, {
					no_follow = true,
					policies = policies,
					cwd = mare.cwd,
					max_entries = 1000,
				})
				if not okl or not list then
					return skip(tostring(okl and list_err or list))
				end
				local names, used = {}, #header + #footer
				local trunc_note = "\n[... truncated]"
				local truncated = list.truncated == true
				for _, e in ipairs(list) do
					local line = e.name .. (e.kind == "dir" and "/" or "")
					local add = (#names > 0 and 1 or 0) + #line
					local reserve = truncated and #trunc_note or 0
					if used + add + reserve > budget then
						truncated = true
						break
					end
					names[#names + 1] = line
					used = used + add
				end
				local body = table.concat(names, "\n")
				if truncated and used + #trunc_note <= budget then
					body = body .. trunc_note
				end
				local block = require("mare_lib").data_block("directory", { path = p }, body)
				return block, #block
			end
			if st.kind ~= "file" then
				return skip("not a regular file")
			end
			if st.size > cfg.max_bytes then
				return skip(("%d bytes, over the %d-byte limit per file"):format(st.size, cfg.max_bytes))
			end
			if st.size > left then
				return skip(("the attachments of this message would exceed %d bytes"):format(cfg.max_total_bytes))
			end
			-- a read the policy denies raises: one bad reference never aborts the message
			local limit = math.min(cfg.max_bytes, left)
			local okr, data, err = pcall(mare.fs.read, target, {
				max_bytes = limit,
				no_follow = true,
				policies = policies,
				cwd = mare.cwd,
			})
			if not okr or not data then
				return skip(tostring(okr and err or data))
			end
			if string.find(string.sub(data, 1, 4000), "\0", 1, true) then
				return skip("binary file")
			end
			if not utf8.len(data) then
				return skip("invalid UTF-8")
			end
			if string.sub(data, 1, 3) == "\239\187\191" then
				data = string.sub(data, 4)
			end
			return require("mare_lib").data_block("file", { path = p }, data), #data
		end

		-- attach referenced files' contents to the user message
		local function transform(msg)
			local seen, attached = {}, {}
			local unescape_peer = { quot = '"', apos = "'", lt = "<", gt = ">", amp = "&" }
			local mention_only = msg.source == "peer"
				or msg.source == "untrusted"
				or (msg.source ~= nil and not svc.permissions)
			if mention_only then
				for _, block in ipairs(msg.content or {}) do
					if block.type == "text" and type(block.text) == "string" then
						local texts = {}
						if msg.source == "peer" and msg.peer_message == true then
							for body in string.gmatch(block.text, "<peer%-message [^>]*>(.-)</peer%-message>") do
								texts[#texts + 1] = string.gsub(body, "&([a-z]+);", unescape_peer)
							end
							local outside = string.gsub(block.text, "<peer%-message [^>]*>.-</peer%-message>", " ")
							texts[#texts + 1] = outside
						else
							texts[#texts + 1] = block.text
						end
						for _, t in ipairs(texts) do
							for _, p in ipairs(refs_of(t)) do
								if p ~= "" and not seen[p] then
									seen[p] = true
									attached[#attached + 1] = { type = "mention", path = p }
								end
							end
						end
					end
				end
				for _, m in ipairs(attached) do
					table.insert(msg.content, m)
				end
				return msg
			end
			local text = msg.content[1] and msg.content[1].text or ""
			local left = cfg.max_total_bytes
			for _, p in ipairs(refs_of(text)) do
				if p ~= "" and not seen[p] then
					seen[p] = true
					local block, n = attach(p, left)
					left = left - n
					attached[#attached + 1] = block
				end
			end
			if #attached > 0 then
				table.insert(msg.content, { type = "text", text = table.concat(attached, "\n\n") })
			end
			return msg
		end
		-- input/transform is a bus waterfall (`function(msg, ev)` with `ev.next(msg)`)
		ctx:on("input/transform", function(msg, ev)
			msg = transform(msg)
			return ev.next(msg)
		end)
	end,
}
