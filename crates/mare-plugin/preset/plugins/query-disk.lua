-- Persisted source, not a logical live observation.
-- Provider-specific coverage belongs here; the generic registry does not branch by provider.
return {
	name = "query-disk",
	-- the session service is followed below
	inject = { "query" },
	apply = function(ctx)
		local function with_coverage(result, live)
			result.coverage = { persisted = true, live = live }
			return result
		end
		local function source(execute)
			return function(args, opts)
				if args.step_bound then
					require("mare_lib").fail(
						"unavailable",
						"QUERY_CAPABILITY_UNAVAILABLE: current-session step-bound observation is unavailable"
					)
				end
				for _, availability in ipairs(args.filters and args.filters.availability or {}) do
					if availability == "live" then
						require("mare_lib").fail(
							"unavailable",
							"QUERY_CAPABILITY_UNAVAILABLE: this source observes persisted history only"
						)
					end
				end
				return with_coverage(execute(args, opts), false)
			end
		end
		ctx.query:register("persisted-sessions", {
			priority = 0,
			source = { capability = "in_process" },
			operations = {
				["session.search"] = source(mare.store.search_sessions),
				["session.events"] = source(mare.store.search_events),
				["session.read"] = source(mare.store.read_event),
				["session.trace"] = source(mare.store.trace_session),
				["session.event_trace"] = source(mare.store.trace_event),
			},
		})
		-- the live source follows the optional session service (a child fiber on the cordis tree,
		-- where session may start after this plugin; its registration goes with the service)
		ctx:inject({ "session" }, function(sub)
			if not sub.session.live_snapshot or (mare.args and mare.args.no_session) then
				return
			end
			local function live_source(execute)
				return function(args, opts)
					local call_opts = table.copy(opts or {})
					local snap = sub.session:live_snapshot()
					if snap then
						call_opts.live_sessions = { snap }
					end
					return with_coverage(execute(args, call_opts), snap ~= nil)
				end
			end
			-- through the follower: the registration is its effect, withdrawn with the session
			sub:get("query"):register("live-sessions", {
				priority = 10,
				source = { capability = "in_process" },
				operations = {
					["session.search"] = live_source(mare.store.search_sessions),
					["session.events"] = live_source(mare.store.search_events),
					["session.read"] = live_source(mare.store.read_event),
					["session.trace"] = live_source(mare.store.trace_session),
					["session.event_trace"] = live_source(mare.store.trace_event),
				},
			})
		end)
	end,
}
