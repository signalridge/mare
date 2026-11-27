-- tools.bash_output: spill files, output cleaning, the bash result text, input-prompt hints and
-- the bounded output accumulator; part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local MAX_LINES, MAX_BYTES = cfg.max_lines, cfg.max_bytes
	local function coded_failure(err)
		return D.pipeline.coded_failure(err)
	end

	-- spill files carry the owning pid and start time; files of processes that no longer exist are
	-- leftovers of a crash and are removed at startup (mare's own — no
	-- reference project cleans these up)
	-- the shell tool's spill files are `<state>/spill/bash-*.log` (mare.proc.spill_* take the prefix)
	local SPILL = "bash"
	local own_start = mare.proc.start_time and mare.proc.start_time()
	local function new_spill()
		local tag = own_start and ("%d-s%d"):format(mare.pid or 0, own_start) or tostring(mare.pid or 0)
		return ("%s/spill/%s-%s-%s-%s.log"):format(
			mare.paths.state,
			SPILL,
			tag,
			mare.date("%Y%m%d-%H%M%S"),
			mare.crypto.random_hex(3)
		)
	end
	local function clean_spill()
		local dir = mare.paths.state .. "/spill"
		local ok, entries = pcall(mare.fs.list, dir)
		if not ok or type(entries) ~= "table" then
			return
		end
		for _, e in ipairs(entries) do
			local name = type(e) == "table" and e.name or e
			if type(name) == "string" and name:sub(-4) == ".log" then
				local pid_s, start_s = string.match(name, "^bash%-(%d+)%-s(%d+)%-")
				local pid = tonumber(pid_s) or tonumber(string.match(name, "^bash%-(%d+)%-"))
				local start = tonumber(start_s)
				local is_self = pid == mare.pid and (not start or not own_start or start == own_start)
				if pid and not is_self and not mare.proc.alive(pid, start) then
					pcall(mare.fs.remove, dir .. "/" .. name)
				end
			end
		end
	end
	if not D.spill_cleaned then
		ctx:spawn(clean_spill)
	end

	--- result of a bash run. r = {out, dropped, code, notes={...}, failed, sandbox, program}
	-- command output → model text; binary bytes (NUL) are reported, not passed on. Returns the
	-- cleaned text and whether binary bytes were seen; bash_result reports the latter (r.binary).
	local function clean_output(raw)
		raw = raw or ""
		return mare.text.sanitize(raw), string.find(raw, "\0", 1, true) ~= nil
	end
	local function partial_note(report)
		if report and report.enforcement == "partial" then
			return "[sandbox: PARTIAL; unavailable: " .. table.concat(report.unavailable or {}, ", ") .. "]"
		end
	end
	local function bash_result(a, r, spill)
		local partial = partial_note(r.sandbox)
		if partial then
			r.notes = r.notes or {}
			r.notes[#r.notes + 1] = partial
		end
		local out = r.out
		-- Git Bash/MSYS can't initialise under the Windows sandbox's restricted token:
		-- a coded error naming the way out, not a raw runtime crash
		local msys = r.sandbox
			and mare.proc.sandbox.msys_failure({
				mechanism = r.sandbox.mechanism,
				code = r.code or r.exit_code,
				program = r.program,
				output = out,
			})
		if msys then
			pcall(mare.proc.spill_remove, SPILL, spill)
			local res = coded_failure(msys)
			if partial then
				res.content = tostring(res.content) .. "\n" .. partial
			end
			res.details = { code = r.code, output = out, job_id = r.job_id, sandbox = r.sandbox }
			return res
		end
		if r.binary then
			r.notes = r.notes or {}
			table.insert(r.notes, 1, "[the output contained binary data; non-text bytes were removed]")
		end
		local parts = { out }
		-- when the output will be truncated, point the model at the complete output file
		local _, nlines = string.gsub(out, "\n", "")
		if (r.dropped or 0) > 0 or #out > MAX_BYTES or nlines > MAX_LINES then
			parts[#parts + 1] = ("\n[full output (%d lines in memory%s): %s]"):format(
				nlines + 1,
				(r.dropped or 0) > 0 and (", first %d bytes only in the file"):format(r.dropped) or "",
				spill
			)
		elseif not r.keep_spill then
			-- mare's own file: never removed as a (sandbox-checked) tool write that would
			-- read as a denial
			pcall(mare.proc.spill_remove, SPILL, spill)
		end
		for _, n in ipairs(r.notes or {}) do
			parts[#parts + 1] = "\n" .. n
		end
		if r.code and r.code ~= 0 then
			parts[#parts + 1] = ("\n[exit code %d]"):format(r.code)
		end
		local text = table.concat(parts)
		if text == "" then
			text = "(no output)"
		end
		-- search output: cap each line
		local first = string.match(a.command or "", "^%s*(%S+)")
		local search = first == "rg"
			or first == "grep"
			or first == "ag"
			or first == "fd"
			or first == "find"
			or first == "git"
		return {
			content = text,
			max_line_chars = search and cfg.max_line_chars or nil,
			is_error = (r.code ~= nil and r.code ~= 0) or r.failed == true,
			details = {
				code = r.code,
				output = out,
				job_id = r.job_id,
				-- reliable sandbox evidence (seatbelt/bwrap refusal, proxy-denied host) for the
				-- permissions layer's targeted hints; nil for ordinary failures
				sandbox_denials = r.denial and { r.denial } or nil,
				sandbox = r.sandbox,
			},
		}
	end

	--- the output ends like an interactive question (`[y/N]`, `password:`): stdin is empty, so it
	--- will never get an answer — say so
	local PROMPTS =
		{ "%[[yY]/[nN]%]", "%([yY]es/[nN]o%)", "[Pp]assword[^\n]*:", "[Pp]assphrase[^\n]*:", "[Cc]ontinue%?" }
	local function input_prompt_hint(out)
		local tail = string.sub(string.gsub(out or "", "%s+$", ""), -200)
		local last = string.match(tail, "([^\n]*)$") or ""
		for _, p in ipairs(PROMPTS) do
			if string.find(last, p) then
				return "[the command seems to be waiting for input ("
					.. mare.text.trunc_width(last, 60)
					.. "); stdin is empty, so it won't get an answer — rerun it non-interactively (e.g. a --yes flag) or stop it]"
			end
		end
	end
	--- bounded output for the persistent shell: the first and
	--- last `cap/2` bytes stay in memory; once the output passes `cap`, all of it also streams
	--- to a spill file, so nothing is lost and memory stays bounded however much is printed.
	--- result() → {out, spill?, dropped?}
	local function new_output_acc(cap)
		local half = math.max(1, cap // 2)
		local o = { head = "", tail = "", total = 0 }
		function o:push(s)
			if s == "" then
				return
			end
			self.total = self.total + #s
			if self.spill then
				pcall(mare.proc.spill_write, SPILL, self.spill, s, true)
			end
			if #self.head < half then
				local take = string.sub(s, 1, half - #self.head)
				self.head = self.head .. take
				s = string.sub(s, #take + 1)
			end
			self.tail = self.tail .. s
			if not self.spill and self.total > cap then
				self.spill = new_spill()
				pcall(mare.proc.spill_write, SPILL, self.spill, self.head .. self.tail)
			end
			if #self.tail > cap then
				self.tail = string.sub(self.tail, -half)
			end
		end
		function o:tail_of(n)
			local all = #self.head + #self.tail
			if all == self.total then
				return string.sub(self.head .. self.tail, -n)
			end
			return string.sub(self.tail, -n)
		end
		function o:result()
			local kept = #self.head + #self.tail
			if kept == self.total then
				return { out = self.head .. self.tail, spill = self.spill }
			end
			local tail = string.sub(self.tail, -half)
			local dropped = self.total - #self.head - #tail
			return {
				out = self.head .. ("\n[… %d bytes omitted …]\n"):format(dropped) .. tail,
				spill = self.spill,
				dropped = dropped,
			}
		end
		return o
	end

	return {
		new_spill = new_spill,
		spill_prefix = SPILL,
		clean_output = clean_output,
		bash_result = bash_result,
		partial_note = partial_note,
		input_prompt_hint = input_prompt_hint,
		new_output_acc = new_output_acc,
	}
end
