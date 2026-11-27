-- provider-openai.errors: Responses API error classification metadata shared by provider-openai
-- registration and provider-openai.wire without eagerly loading the full wire module at startup.
return {
	codes = {
		subscription_sharing_usage_limit_exceeded = "quota",
		subscription_sharing_workplace_individual_usage_limit_exceeded = "quota",
		model_not_found = "model_not_found",
		server_is_overloaded = "overloaded",
		rate_limit_exceeded = "rate_limit",
		slow_down = "rate_limit",
		server_error = "server",
		internal_error = "server",
		websocket_connection_limit_reached = "transport",
		previous_response_not_found = "transport",
	},
	image_limit = function(msg, code)
		local m = string.lower(tostring(msg or ""))
		return code == "image_too_large"
			or m:find("too many images", 1, true) ~= nil
			or m:find("image too large", 1, true) ~= nil
			or m:find("image is too large", 1, true) ~= nil
			or m:find("image size exceeds", 1, true) ~= nil
			or m:find("number of images", 1, true) ~= nil
	end,
}
