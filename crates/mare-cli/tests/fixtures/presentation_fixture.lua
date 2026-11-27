local normal_png =
	"iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAJElEQVR4nGOQ23Lp/0BihlEHjDpg1AGjDhh1wKgDRh0w0A4AALXQRkwBCanJAAAAAElFTkSuQmCC"
local variant_png =
	"iVBORw0KGgoAAAANSUhEUgAAABAAAAAgCAYAAAAbifjMAAAAIklEQVR4nGM4IWf0nxLMMGrAqAGjBowaMGrAqAGjBgw3AwAt1S49BXKV0AAAAABJRU5ErkJggg=="

return {
	name = "presentation-fixture",
	inject = { "ui", "session" },
	apply = function(ctx)
		local turn_ended = false
		local turn_tx, turn_rx = mare.task.channel()
		ctx:on("turn/completed", function()
			local p = mare.env("PRESENTATION_SESSION_REPORT")
			if p then
				mare.fs.write(p, mare.json.encode(ctx.session:info()))
			end
			turn_ended = true
			turn_tx:send(true)
		end)
		local slow_done_tx, slow_done_rx = mare.task.channel()
		local cancel_seq, cancel_barrier = 0, nil
		local pending_cancel_requests = {}
		ctx:on("ui/presentation-render-completed", function(ev)
			local req_id = ev and ev.request_id
			local entry = req_id and pending_cancel_requests[req_id]
			if
				entry
				and ev.block_id == entry.block_id
				and ev.generation == entry.generation
				and ev.revision == entry.revision
				and ev.accepted == false
			then
				pending_cancel_requests[req_id] = nil
				if next(pending_cancel_requests) == nil then
					cancel_barrier = "CANCEL-BARRIER-" .. cancel_seq
					ctx.ui:invalidate("cancel-barrier")
				end
			end
		end)
		ctx.ui:widget("cancel-barrier", {
			slot = "above-editor",
			render = function()
				return cancel_barrier and mare.ui.Span(cancel_barrier) or nil
			end,
		})
		ctx.ui:markdown_presenter("drawing", {
			render = function(r)
				if r.source:find("rpc-cancel", 1, true) then
					r.cancel:wait()
					slow_done_tx:send(true)
				elseif r.source:find("slow-cancel", 1, true) then
					cancel_seq = cancel_seq + 1
					pending_cancel_requests[r.request_id] = {
						seq = cancel_seq,
						block_id = r.block_id,
						generation = r.generation,
						revision = r.revision,
					}
					local s = mare.env("PRESENTATION_CANCEL_STARTED")
					if s then
						mare.fs.write(
							s,
							mare.json.encode({
								seq = cancel_seq,
								request_id = r.request_id,
								block_id = r.block_id,
								generation = r.generation,
								revision = r.revision,
							})
						)
					end
					r.cancel:wait()
				elseif r.source:find("fence", 1, true) then
					slow_done_rx:recv()
				elseif
					r.source:find("slow", 1, true)
					and mare.env("PRESENTATION_WAIT_AGENT_END")
					and not turn_ended
				then
					mare.task.select(function()
						turn_rx:recv()
						turn_tx:send(true)
					end, function()
						r.cancel:wait()
					end)
				end
				local b64 = r.source:find("cancel", 1, true) and variant_png or normal_png
				return mare.ui.Lines({
					mare.ui.Code("viewport:" .. r.width, "text"),
					mare.ui.image({
						bytes = mare.crypto.base64_decode(b64),
						max_rows = 8,
						max_cols = 40,
					}),
				})
			end,
		})
	end,
}
