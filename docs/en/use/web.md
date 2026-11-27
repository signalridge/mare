# Local WebSocket and HTTP listener

mare includes an authenticated host HTTP/WebSocket listener (`--web` / `webserver`) that exposes mare RPC protocol 3.0 (JSON-RPC 2.0 envelope) over WebSocket, plugin HTTP routes, and declarative static web assets (`web = { dir }`). It does not bundle a standalone frontend app by itself; plugins or external clients supply the UI over those endpoints.

## Starting the listener

The host listener runs only in RPC mode. Running plain `mare` opens the interactive TUI and does not start the web listener, even when `webserver.enabled = true` is set in `init.lua`. Start the listener with `--web` or `--web-listen` (which select RPC mode), or combine `webserver.enabled = true` with `mare --rpc`:

```sh
mare --web                          # binds 127.0.0.1:0 (OS-assigned ephemeral port)
mare --web-listen 127.0.0.1:7432    # binds an explicit HOST:PORT
mare --rpc                          # also starts the listener when webserver.enabled = true
```

```lua
-- ~/.config/mare/init.lua (takes effect under `mare --rpc`)
mare.setup {
  webserver = {
    enabled = true,
    host = "127.0.0.1",
    port = 7432,
  },
}
```

When the listener starts, mare prints the listening URL, the `/readyz` probe URL, the WebSocket RPC URL (`ws://.../rpc` or `wss://.../rpc`), and the host token file path — never the raw token or a `?token=` URL. To print the authenticated login link for the running listener, run:

```sh
mare web url
```

Opening that `?token=...` URL in a browser exchanges it for a session cookie and redirects to remove `?token=` from the address bar, while the host token itself stays reusable until rotated.

## Built-in endpoints

| Path | Auth required | Purpose |
|---|---|---|
| `/readyz`, `/healthz` | No | Health check: `GET` returns `{"status":"ready"}`, `HEAD` returns status `200` with an empty body |
| `/rpc`, `/` (WebSocket upgrade) | Yes | Bidirectional mare RPC protocol 3.0 stream using JSON-RPC 2.0 envelopes (`one JSON object per text frame`; binary frames close with `1003`). Closing or dropping the socket cancels pending callbacks and releases connection-owned subscriptions and registrations |
| Plugin routes (`webserver.register_route`) | Yes | Dynamic HTTP handlers registered by Lua plugins or RPC (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `ANY`; max 256 routes) |
| Static assets (`web = { dir }`) | Yes | Static files mounted by plugins or `webserver.register_assets` (max 64 mounts, 32 MiB max per file) |

Plugin routes cannot override `/readyz`, `/healthz`, or `/rpc`.

## Authentication and token rotation

Every request other than `/readyz` and `/healthz` requires authentication:

1. **Host token**: By default, mare generates a 32-byte OS-CSPRNG token stored in `~/.local/state/mare/host/token` (`0600` on Unix, restricted ACL on Windows). You can also set a static `webserver.token` string in `init.lua`; `webserver.token = false` is refused.
2. **Browser cookie exchange**: Opening `GET <url>/?token=<token>` sets an `HttpOnly; SameSite=Strict` session cookie (with `Secure` when TLS is active) and redirects (`303`) to strip `?token=` from the browser URL. This hides the token from the address bar; it does not make the token single-use.
3. **API / WebSocket clients**: Pass `Authorization: Bearer <token>`, `X-Mare-Token: <token>`, the session cookie, or `?token=<token>` on WebSocket upgrade.
4. **Lockout**: Five consecutive failed auth attempts from an IP trigger a 30-second lockout.

Rotate the runtime host token file from the shell:

```sh
mare token rotate
```

`mare token rotate` updates `~/.local/state/mare/host/token` and invalidates the previous file token immediately. Active WebSocket connections authenticated with that file token close with WebSocket close code `4001` on their next revocation check. Listeners configured with a static `webserver.token` string continue using that string and are unaffected by file rotation.

### Startup and rotation check (`fs_host_protect`)

When using the default runtime token file, both listener startup and `mare token rotate` verify that the active sandbox backend can hide the token file from sandboxed commands (`fs_host_protect`). If the backend cannot protect it (for example on Linux without `bubblewrap`, or Windows without enhanced sandbox setup), startup fails with `SANDBOX_INSUFFICIENT` before creating the token file or starting the listener.

To resolve that refusal, install `bwrap` (Linux) or run `mare sandbox setup` (Windows), switch to a static token if appropriate, or explicitly accept the missing protection:

```lua
mare.setup {
  webserver = { accept_partial = { "fs_host_protect" } },
}
```

```sh
mare token rotate --accept-partial fs_host_protect
```

## Host, Origin, and TLS checks

`Host` and `Origin` headers are checked before authentication:

- No wildcard binds: `0.0.0.0`, `[::]`, `::ffff:0.0.0.0`, `*`, and unspecified addresses are refused even with TLS. Bind a loopback IP (`127.0.0.1`, `[::1]`) or one concrete interface IP.
- Non-loopback TLS / plain HTTP: Non-loopback binds require `tls_cert` and `tls_key` unless you bind one concrete IPv4/IPv6 literal (IPv6 may include `%zone`; hostnames are rejected) with `plain_http = true`. `localhost` uses the loopback path. For `%zone` IPv6 addresses, `mare web url` cannot emit a browser `?token=` URL; bind a non-link-local IP or authenticate via headers.
- `Host` check: Loopback binds accept loopback `Host` headers. Concrete non-loopback binds trust their bound IP and port automatically; list reverse-proxied or custom hostnames in `trusted_hosts`.
- `Origin` check: `Sec-Fetch-Site: cross-site` returns `403` before `allowed_origins` is checked. Other cross-origin browser requests require an entry in `allowed_origins`.
- Local admin vs `remote_admin = false`: Local admin WebSocket rights require all three to be loopback: TCP peer IP, `Host` header, and listener bind address. Connecting from localhost to a non-loopback bind keeps `remote_admin = false`. While `remote_admin = false`, remote owners cannot call admin methods (`options.set`, `services.register`, `workspace.set_trust`, `subagent.register_type`, `hooks.subscribe`), cannot claim responder leases (`capabilities.approvals` / `elicitation` / `ui` or `ui.attach { responder: true }`), cannot call `session.load { path }`, and have restored presets, sandbox modes, and extra directories capped to the host ceiling.
- Same-host reverse proxy: Because mare ignores forwarded headers (`X-Forwarded-For`, `Forwarded`), a reverse proxy on the same host forwarding remote traffic to `127.0.0.1:<port>` with a loopback `Host` header satisfies all three loopback checks and grants remote callers local admin rights. Reach a loopback listener over SSH tunnel / Tailscale / WireGuard, or bind a concrete non-loopback IP with TLS and `remote_admin = false`.

### Containers and public deployment

- **Containers**: Wildcard binds (`0.0.0.0` / `[::]`) are refused inside containers as well. Pass one concrete container interface IP literal via `--web-listen` (or `webserver.host`) together with TLS or `plain_http = true`:

```sh
mare --web-listen "$(hostname -i | cut -d' ' -f1):7432" --set webserver.plain_http=true
```

- **Public deployment**: Do not expose the listener directly to the public internet. Keep mare bound to loopback (`127.0.0.1`) or a private interface and reach it through an SSH tunnel (for example `ssh -L 7432:127.0.0.1:7432 user@host`) or a private mesh network (such as Tailscale or WireGuard), or place TLS in front of a concrete interface bind with explicit `trusted_hosts` and `allowed_origins`.

```lua
mare.setup {
  webserver = {
    enabled = true,
    host = "192.168.1.20",
    port = 8443,
    tls_cert = "/etc/mare/cert.pem",
    tls_key = "/etc/mare/key.pem",
    trusted_hosts = { "mare.internal:8443" },
    allowed_origins = { "https://mare.internal:8443" },
    remote_admin = false,
  },
}
```

## Serving plugin assets and routes

Plugins can mount static directories declaratively on their plugin definition table or register handlers through the `webserver` service:

```lua
return {
  name = "my-panel",
  web = { dir = "dist", prefix = "/panel", index = "index.html", fallback = "index.html" },
  inject = { "webserver" },
  apply = function(ctx)
    ctx.webserver:register_route("/api/ping", {
      method = "GET",
      handle = function()
        return { status = 200, headers = { ["content-type"] = "application/json" }, body = '{"ok":true}' }
      end,
    })
  end,
}
```

Active mounts and routes stay registered across the `HostManager` and are cleaned up automatically when the owning plugin or session disposes.

## Related

- [mare RPC protocol 3.0](../integrate/rpc.md) for the protocol spoken over `/rpc`
- [Options](../configure/options.md) for every `webserver` field and default
- [Plugins](../extend/plugins.md) for plugin lifecycle and `web` definitions
