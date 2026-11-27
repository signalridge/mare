local LIMITS = {
	max_response_bytes = { 1, 5000000 },
	max_output_bytes = { 1, 1000000 },
	timeout_ms = { 1, 2147483647 },
	max_redirects = { 0, 20 },
	max_url_length = { 1, 65536 },
	max_html_tags = { 1, 100000 },
	max_html_nodes = { 1, 50000 },
	max_html_depth = { 1, 128 },
}

return {
	name = "web-fetch-http",
	-- it registers a fetch provider on web (a mare_lib.service)
	inject = { "web" },
	config_schema = {
		max_response_bytes = {
			type = "integer",
			default = 5000000,
			desc = "Maximum raw HTTP response body size in bytes.",
		},
		max_output_bytes = {
			type = "integer",
			default = 100000,
			desc = "Maximum extracted text/markdown output size in bytes.",
		},
		timeout_ms = {
			type = "integer",
			default = 30000,
			desc = "Overall fetch request timeout in milliseconds.",
		},
		max_redirects = {
			type = "integer",
			default = 5,
			desc = "Maximum number of HTTP redirects to follow.",
		},
		redirect = {
			type = "string",
			default = "same-origin",
			desc = "Redirect policy: same-origin, follow, or manual.",
		},
		user_agent = {
			type = "string",
			default = "mare-web-fetch/1",
			desc = "User-Agent header sent on fetch requests.",
		},
		max_url_length = {
			type = "integer",
			default = 8192,
			desc = "Maximum allowed URL length in bytes.",
		},
		max_html_tags = {
			type = "integer",
			default = 100000,
			desc = "Maximum HTML tag count parsed during conversion.",
		},
		max_html_nodes = {
			type = "integer",
			default = 50000,
			desc = "Maximum DOM node count traversed during conversion.",
		},
		max_html_depth = {
			type = "integer",
			default = 128,
			desc = "Maximum DOM nesting depth during conversion.",
		},
	},
	apply = function(ctx, cfg)
		for key, bounds in pairs(LIMITS) do
			local val = cfg[key]
			assert(type(val) == "number" and val >= bounds[1] and val % 1 == 0, key .. " must be an integer in range")
			assert(val <= bounds[2], "fetch limit exceeds safety ceiling")
		end
		assert(
			cfg.redirect == "manual" or cfg.redirect == "follow" or cfg.redirect == "same-origin",
			"invalid redirect policy"
		)
		assert(
			cfg.user_agent == nil
				or (type(cfg.user_agent) == "string" and cfg.user_agent ~= "" and not cfg.user_agent:find("[%c]")),
			"invalid user_agent"
		)
		ctx.web:register_fetch({
			provider_id = "http-text",
			fetch = function(request, call)
				return require("web-fetch-http.fetch")(request, call, cfg)
			end,
		})
	end,
}
