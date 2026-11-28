local t = require("mare.test")
local describe = t.describe
local it = t.it
local before_each = t.before_each
local after_each = t.after_each
local expect = t.expect

--- Mount `web-fetch-http` into an isolated `mare.test.host()` backed by a contract-admitted fake
--- `web` service, capturing the registered fetch backend.
local function mount_fetch_plugin(overrides)
	local host = t.host()
	local registered
	host:provide("web", {
		register_search = function()
			return function() end
		end,
		register_fetch = function(_, definition)
			registered = definition
			return function() end
		end,
		list_providers = function()
			return { data = {} }
		end,
		search = function()
			return { data = {} }
		end,
		fetch = function()
			return { requested_url = "", content = "" }
		end,
	})
	host:load("web-fetch-http", overrides)
	return host, registered
end

describe("web-fetch-http", function()
	local closed, reads, http_calls, responses
	local chunks, headers, status, last_req
	local req_error, read_error
	local orig_http_request, orig_text_decode, orig_html_to_markdown

	local function run(body, h, s, overrides, url, cancel)
		chunks, headers, status = body, h or { ["content-type"] = "text/plain" }, s or 200
		local previous_responses, previous_closed = responses, closed
		local host, registered = mount_fetch_plugin(overrides)
		expect.settled(host)
		local ok, result = pcall(
			registered.fetch,
			{ url = url or "https://requested.test/file" },
			{ cancel = cancel or mare.task.token() }
		)
		host:dispose()
		-- A returned response must be closed exactly once, even when processing fails.
		expect.eq(closed - previous_closed, responses - previous_responses)
		return ok, result
	end

	local function expect_err(code, fn)
		local ok, err = fn()
		expect.falsy(ok, "expected " .. code)
		expect.match(tostring(err), "^" .. code .. ":")
		return tostring(err)
	end

	before_each(function()
		closed, reads, http_calls, responses = 0, 0, 0, 0
		chunks, headers, status, last_req = {}, {}, 200, nil
		req_error, read_error = nil, nil
		orig_http_request = mare.http.request
		orig_text_decode = mare.text.decode
		orig_html_to_markdown = mare.text.html_to_markdown
		if orig_text_decode == nil then
			mare.text.decode = function(bytes, charset)
				if charset == "bogus" then
					error("unknown charset: " .. charset)
				end
				assert(utf8.len(bytes), "malformed text")
				return bytes
			end
		end
		if orig_html_to_markdown == nil then
			mare.text.html_to_markdown = function(body, opts)
				if type(opts) == "table" and opts.max_depth == 1 then
					error("html depth exceeded")
				end
				return "MD:" .. body, false
			end
		end
		mare.http.request = function(req)
			http_calls = http_calls + 1
			last_req = req
			if req_error then
				error(req_error, 0)
			end
			local i = 0
			responses = responses + 1
			return {
				ok = status >= 200 and status < 300,
				status = status,
				headers = headers,
				url = "https://final.test/file",
				read = function()
					reads = reads + 1
					if read_error then
						error(read_error, 0)
					end
					i = i + 1
					return chunks[i]
				end,
				close = function()
					closed = closed + 1
				end,
			}
		end
	end)

	after_each(function()
		mare.http.request = orig_http_request
		mare.text.decode = orig_text_decode
		mare.text.html_to_markdown = orig_html_to_markdown
	end)

	it("enforces UTF-8 output limits and response size boundaries", function()
		local ok, result = run({ "a界界" }, nil, 200, { max_output_bytes = 5 })
		expect.truthy(ok)
		expect.eq(result.content, "a界")
		expect.eq(utf8.len(result.content), 2)
		expect.truthy(result.truncated)
		expect.ne(result.requested_url, result.url)

		local before = reads
		expect_err("WEB_FETCH_TOO_LARGE", function()
			return run({ "unread" }, { ["content-type"] = "text/plain", ["content-length"] = "6" }, 200, {
				max_response_bytes = 5,
			})
		end)
		expect.eq(reads, before)

		ok, result = run({ "abc", "def" }, nil, 200, { max_response_bytes = 5, max_output_bytes = 8 })
		expect.truthy(ok)
		expect.eq(result.content, "abcde")
		expect.truthy(result.truncated)

		ok, result = run({ "abcde" }, nil, 200, { max_response_bytes = 5, max_output_bytes = 5 })
		expect.truthy(ok)
		expect.eq(result.content, "abcde")
		expect.falsy(result.truncated)
	end)

	it("accepts text and structured text types and closes responses on errors", function()
		for _, mime in ipairs({
			"text/plain",
			"text/markdown",
			"text/javascript",
			"application/json",
			"application/xml",
			"application/javascript",
			"application/x-javascript",
			"application/ecmascript",
			"application/problem+json",
			"application/atom+xml",
		}) do
			local ok, result = run({ "ok" }, { ["content-type"] = mime .. "; charset=utf-8" })
			expect.truthy(ok, mime)
			expect.eq(result.content, "ok", mime)
		end
		for _, mime in ipairs({
			"application/javascript",
			"application/x-javascript",
			"application/ecmascript",
		}) do
			local ok, result = run({ "console.log(1);" }, { ["content-type"] = mime })
			expect.truthy(ok, mime .. " without charset")
			expect.eq(result.content, "console.log(1);", mime .. " without charset")
			expect.truthy(
				last_req.headers.accept:find(mime, 1, true),
				"Accept header must advertise " .. mime .. ": " .. tostring(last_req.headers.accept)
			)
		end
		expect.truthy(last_req.headers.accept:find("text/javascript", 1, true))
		for _, mime in ipairs({ "text/html", "application/xhtml+xml" }) do
			local ok, result = run({ "<p>hi</p>" }, { ["content-type"] = mime .. "; charset='UTF-8'" })
			expect.truthy(ok, mime)
			expect.match(result.content, "hi")
		end

		local before = reads
		local bin_err = expect_err("WEB_UNSUPPORTED_CONTENT_TYPE", function()
			return run({ "unread" }, { ["content-type"] = "application/octet-stream" })
		end)
		expect.match(bin_err, "supported:")
		expect.match(bin_err, "application/javascript")
		expect.match(bin_err, "application/x%-javascript")
		expect.match(bin_err, "application/ecmascript")
		local png_err = expect_err("WEB_UNSUPPORTED_CONTENT_TYPE", function()
			return run({ "\x89PNG" }, { ["content-type"] = "image/png" })
		end)
		expect.match(png_err, "supported:")
		expect_err("WEB_UNSUPPORTED_CONTENT_TYPE", function()
			return run({ "unread" }, { ["content-type"] = "text/plain; charset=bogus" })
		end)
		expect.eq(reads, before)
		expect_err("WEB_HTTP_STATUS", function()
			return run({ "unread" }, nil, 404)
		end)
		expect_err("WEB_HTTP_STATUS", function()
			return run({ "unread" }, nil, 302, { redirect = "manual" })
		end)
		expect.eq(reads, before)
		expect_err("WEB_DECODE_ERROR", function()
			return run({ "\xff" })
		end)
		expect_err("WEB_HTML_ERROR", function()
			return run({ "<div><div><p>err</p></div></div>" }, { ["content-type"] = "text/html" }, 200, {
				max_html_depth = 1,
			})
		end)
		read_error = "transport: stream timed out"
		expect_err("WEB_FETCH_TIMEOUT", function()
			return run({ "unread" })
		end)
	end)

	it("validates and normalizes URLs and forwards configuration", function()
		local calls_before = http_calls
		for _, case in ipairs({
			{ "", "WEB_INVALID_URL" },
			{ "file:///etc/passwd", "WEB_INVALID_URL" },
			{ "ftp://example.test/file", "WEB_INVALID_URL" },
			{ "https://:8080/file", "WEB_INVALID_URL" },
			{ "https://user:pass@example.test", "WEB_BLOCKED_URL" },
			{ "https://user@example.test/file", "WEB_BLOCKED_URL" },
			{ "https://example.test/with space", "WEB_INVALID_URL" },
			{ "https://example.test/\nline", "WEB_INVALID_URL" },
			{ "https://example.test/" .. string.rep("a", 30), "WEB_INVALID_URL" },
		}) do
			expect_err(case[2], function()
				return run({ "unread" }, nil, 200, { max_url_length = 40 }, case[1])
			end)
		end
		expect.eq(http_calls, calls_before)

		local cancel = mare.task.token()
		local ok, result = run({ "cfg" }, nil, 200, {
			timeout_ms = 1234,
			redirect = "manual",
			max_redirects = 0,
			user_agent = "custom-ua/1",
		}, "HTTPS://EXAMPLE.TEST:443", cancel)
		expect.truthy(ok)
		expect.eq(result.requested_url, "https://example.test/")
		expect.eq(last_req.url, "https://example.test/")
		expect.eq(last_req.method, "GET")
		expect.eq(last_req.timeout_ms, 1234)
		expect.eq(last_req.redirect, "manual")
		expect.eq(last_req.max_redirects, 0)
		expect.eq(last_req.headers["user-agent"], "custom-ua/1")
		expect.match(last_req.headers.accept, "application/xhtml%+xml")
		expect.match(last_req.headers.accept, "application/javascript")
		expect.match(last_req.headers.accept, "application/ecmascript")
		expect.eq(last_req.cancel, cancel)

		ok, result = run(
			{ "ok" },
			{ ["content-type"] = "text/plain; charset=" },
			200,
			nil,
			"http://EXAMPLE.TEST:80?a=1"
		)
		expect.truthy(ok)
		expect.eq(last_req.url, "http://example.test/?a=1")
		expect.eq(result.content, "ok")
	end)

	it("validates built-in configuration when applying the plugin", function()
		local host, registered = mount_fetch_plugin()
		expect.settled(host)
		expect.eq(registered.provider_id, "http-text")
		expect.eq(type(registered.fetch), "function")
		for _, redirect in ipairs({ "manual", "follow", "same-origin" }) do
			host:load("web-fetch-http", { redirect = redirect })
			expect.settled(host)
		end

		for _, limit in ipairs({
			{ "max_response_bytes", 1, 5000000 },
			{ "max_output_bytes", 1, 1000000 },
			{ "timeout_ms", 1, 2147483647 },
			{ "max_redirects", 0, 20 },
			{ "max_url_length", 1, 65536 },
			{ "max_html_tags", 1, 100000 },
			{ "max_html_nodes", 1, 50000 },
			{ "max_html_depth", 1, 128 },
		}) do
			for _, invalid in ipairs({ limit[2] - 1, limit[2] + 0.5, limit[3] + 1 }) do
				host:load("web-fetch-http", { [limit[1]] = invalid })
				expect.eq(host:state("web-fetch-http"), "FAILED", limit[1] .. " = " .. invalid)
				expect.truthy(host:error("web-fetch-http"), limit[1] .. " = " .. invalid)
			end
		end
		for _, invalid in ipairs({ { redirect = "always" }, { user_agent = "" }, { user_agent = "bad\nua" } }) do
			host:load("web-fetch-http", invalid)
			expect.eq(host:state("web-fetch-http"), "FAILED")
			expect.truthy(host:error("web-fetch-http"))
		end
		host:dispose()
	end)

	it("classifies transport errors, redacts secrets, and bounds details", function()
		for _, case in ipairs({
			{ "aborted", "WEB_ABORTED" },
			{ "sandbox: host denied", "WEB_BLOCKED_URL" },
			{ "local/private address denied", "WEB_BLOCKED_URL" },
			{ "offline test guard: blocked", "WEB_BLOCKED_URL" },
			{ "fake-ip address denied", "WEB_BLOCKED_URL" },
			{ "non-local proxy denied", "WEB_BLOCKED_URL" },
			{ "too many redirects", "WEB_REDIRECT_BLOCKED" },
			{ "transport: ETIMEDOUT connecting", "WEB_FETCH_CONNECTION_TIMEOUT" },
			{ "transport: dns error", "WEB_FETCH_DNS_FAILED" },
			{ "transport: ECONNREFUSED", "WEB_FETCH_CONNECTION_REFUSED" },
			{ "transport: tls handshake failed", "WEB_FETCH_CERTIFICATE_INVALID" },
			{ "transport: connection reset", "WEB_FETCH_CONNECTION_INTERRUPTED" },
			{ "transport: ENETUNREACH", "WEB_FETCH_NETWORK_UNREACHABLE" },
			{ "transport: request timed out", "WEB_FETCH_TIMEOUT" },
			{ "transport: unknown failure mode", "WEB_FETCH_NETWORK_ERROR" },
		}) do
			req_error = case[1]
			expect_err(case[2], function()
				return run({ "unread" })
			end)
		end

		for _, message in ipairs({
			"sandbox:",
			"local/private",
			"offline test guard",
			"fake-ip address",
			"non-local proxy",
		}) do
			local cancel = mare.task.token()
			cancel:cancel()
			req_error = message
			expect_err("WEB_ABORTED", function()
				return run({ "unread" }, nil, 200, nil, nil, cancel)
			end)
		end

		req_error = "proxy connect via http://alice:s3cr3t@proxy.internal:8080 failed: "
			.. ("x"):rep(600)
			.. "\nstack traceback:\n\tfoo.lua:1"
		local message = expect_err("WEB_FETCH_NETWORK_ERROR", function()
			return run({ "unread" })
		end)
		expect.falsy(message:find("alice", 1, true))
		expect.falsy(message:find("s3cr3t", 1, true))
		expect.match(message, "http://%[REDACTED%]@proxy%.internal:8080")
		expect.falsy(message:find("stack traceback", 1, true))
		local detail = message:match("^WEB_FETCH_NETWORK_ERROR: the network request failed: (.*)$")
		expect.eq(utf8.len(detail), 501)
		expect.eq(detail:sub(-#"…"), "…")

		req_error = "header Authorization: Bearer supersecret_token_123456 rejected"
		message = expect_err("WEB_FETCH_NETWORK_ERROR", function()
			return run({ "unread" })
		end)
		expect.falsy(message:find("supersecret_token_123456", 1, true))
		expect.match(message, "Authorization: %[REDACTED%]")
	end)
end)
