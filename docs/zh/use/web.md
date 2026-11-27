# 本地 WebSocket 与 HTTP 监听

mare 内置了带鉴权的宿主 HTTP/WebSocket 监听服务（`--web` / `webserver`），用来暴露基于 WebSocket 的 mare RPC 协议 3.0（JSON-RPC 2.0 信封）、插件注册的 HTTP 路由，以及插件声明的静态资源（`web = { dir }`）。mare 本身不打包独立前端界面，页面由插件或外部客户端通过这些端点接入。

## 启动监听服务

宿主监听服务只在 RPC 模式下启动。直接运行普通 TUI `mare` 不会启动 Web 监听，即使 `init.lua` 里写了 `webserver.enabled = true`。请通过 `--web`、`--web-listen`（两者都会进入 RPC 模式）启动，或在启用 `webserver.enabled = true` 后用 `mare --rpc` 启动：

```sh
mare --web                          # 绑定 127.0.0.1:0（由系统分配端口）
mare --web-listen 127.0.0.1:7432    # 绑定指定的 HOST:PORT
mare --rpc                          # 当 webserver.enabled = true 时同时启动监听
```

```lua
-- ~/.config/mare/init.lua（配合 mare --rpc 生效）
mare.setup {
  webserver = {
    enabled = true,
    host = "127.0.0.1",
    port = 7432,
  },
}
```

启动后，mare 会打印监听地址、`/readyz` 探针地址、WebSocket RPC 地址（`ws://.../rpc` 或 `wss://.../rpc`），以及宿主令牌文件路径，绝不直接打印原始令牌或 `?token=` 链接。需要获取当前运行中监听服务的带令牌登录链接时，执行：

```sh
mare web url
```

在浏览器打开该 `?token=...` 链接会换取会话 Cookie 并重定向去掉地址栏里的 `?token=`，但宿主令牌本身并非一次性，在轮换前一直有效，请勿当作一次性链接分享。

## 内置端点

| 路径 | 需要鉴权 | 作用 |
|---|---|---|
| `/readyz`、`/healthz` | 否 | 健康检查：`GET` 返回 `{"status":"ready"}`，`HEAD` 返回 `200` 且响应体为空 |
| `/rpc`、`/`（WebSocket 升级） | 是 | 双向 mare RPC 协议 3.0 流，使用 JSON-RPC 2.0 信封（每个文本帧一个 JSON 对象；二进制帧以 `1003` 关闭）。连接关闭或断开时会自动取消未完成回调并释放该连接持有的订阅与注册项 |
| 插件路由（`webserver.register_route`） | 是 | Lua 插件或 RPC 注册的动态 HTTP 路由（支持 `GET`、`POST`、`PUT`、`PATCH`、`DELETE`、`HEAD`、`ANY`，最多 256 条） |
| 静态资源（`web = { dir }`） | 是 | 插件声明或 `webserver.register_assets` 挂载的静态目录（最多 64 个挂载，单文件上限 32 MiB） |

插件路由不能覆盖 `/readyz`、`/healthz` 和 `/rpc`。

## 鉴权与令牌轮换

除 `/readyz` 和 `/healthz` 之外的所有请求都必须通过鉴权：

1. **宿主令牌**：默认生成 32 字节 OS-CSPRNG 随机令牌，保存在 `~/.local/state/mare/host/token`（Unix 下权限 `0600`，Windows 下使用受限 ACL）。也可以在 `init.lua` 设置静态 `webserver.token` 字符串；`webserver.token = false` 会被拒绝启动。
2. **浏览器 Cookie 交换**：用浏览器打开 `GET <url>/?token=<token>` 时，服务会下发 `HttpOnly; SameSite=Strict` 会话 Cookie（启用 TLS 时附带 `Secure`），并返回 `303` 重定向以抹掉地址栏里的 `?token=`。该重定向只隐藏地址栏参数，不会使宿主令牌失效。
3. **API 与 WebSocket 客户端**：通过 `Authorization: Bearer <token>`、`X-Mare-Token: <token>` 请求头、会话 Cookie，或在 WebSocket 握手时附带 `?token=<token>` 鉴权。
4. **失败锁定**：同一 IP 连续 5 次鉴权失败将触发 30 秒锁定。

在命令行轮换运行时令牌文件：

```sh
mare token rotate
```

`mare token rotate` 会更新 `~/.local/state/mare/host/token` 并立即使旧文件令牌失效；使用旧文件令牌建立的活跃 WebSocket 连接会在撤销检查时收到关闭码 `4001` 并断开。如果监听器配置的是静态字符串 `webserver.token`，它将继续使用该静态字符串，不受文件轮换影响。

### 启动与轮换前置检查（`fs_host_protect`）

使用默认运行时令牌文件时，监听启动和 `mare token rotate` 都会检查当前沙箱后端是否支持 `fs_host_protect`（防止沙箱内命令读取宿主令牌文件）。如果后端不支持（例如 Linux 未安装 `bubblewrap`，或 Windows 未完成增强沙箱安装），服务会在创建令牌文件或启动监听前直接报 `SANDBOX_INSUFFICIENT` 并拒绝启动。

可以按需安装 `bwrap`（Linux）或执行 `mare sandbox setup`（Windows），改用静态令牌，或显式接受该缺口：

```lua
mare.setup {
  webserver = { accept_partial = { "fs_host_protect" } },
}
```

```sh
mare token rotate --accept-partial fs_host_protect
```

## Host、Origin 与 TLS 校验

鉴权前先校验 `Host` 与 `Origin` 请求头：

- 禁止通配绑定：无论是否启用 TLS，一律拒绝 `0.0.0.0`、`[::]`、`::ffff:0.0.0.0`、`*` 等未指定地址；必须绑定环回 IP（`127.0.0.1`、`[::1]`）或单个具体网卡 IP。
- 非环回 TLS / 明文 HTTP：非环回绑定默认要求 `tls_cert` 与 `tls_key`；仅在显式绑定单个具体 IPv4/IPv6 字面量（IPv6 允许 `%zone`，拒绝主机名）并设置 `plain_http = true` 时允许明文 HTTP，`localhost` 走环回分支。带 `%zone` 的 IPv6 地址无法用 `mare web url` 生成标准 `?token=` 链接，改绑非链路本地 IP 或用请求头鉴权。
- `Host` 校验：环回绑定接受环回 `Host`；非环回具体绑定自动信任所绑定的 IP 与端口，反代或自定义域名须写入 `trusted_hosts`。
- `Origin` 校验：带 `Sec-Fetch-Site: cross-site` 的请求先于 `allowed_origins` 直接返回 `403`；其余跨源浏览器请求须在 `allowed_origins` 白名单内。
- 本地管理员与 `remote_admin = false`：仅当 TCP 对端 IP、`Host` 头与监听地址三者均为环回时才拥有本地管理员权限。从本机连非环回监听仍保持 `remote_admin = false`：远程主连接不能调用管理员方法（`options.set`、`services.register`、`workspace.set_trust`、`subagent.register_type`、`hooks.subscribe`），不能认领审批/elicitation/UI 应答者租约（`capabilities.approvals` / `elicitation` / `ui` 或 `ui.attach { responder: true }`），不能按任意路径调用 `session.load { path }`，且 `session.load` / `session.fork` 恢复的预设、沙箱模式与附加目录不超过宿主上限。
- 同机反向代理：mare 不信任 `X-Forwarded-For` / `Forwarded` 等转发头。若同机反代将远端流量转发至 `127.0.0.1:<port>` 并改写为环回 `Host`，三个环回条件会同时成立，远端用户将被判为本地管理员。远端访问应使用 SSH 隧道 / Tailscale / WireGuard，或绑定非环回网卡 IP 并启用 TLS 与 `remote_admin = false`。

### 容器用法与公开部署

- **容器内绑定**：容器内同样拒绝通配绑定（`0.0.0.0` / `[::]`），请通过 `--web-listen`（或 `webserver.host`）绑定容器的单个具体网卡 IP 字面量，并配合 TLS 或 `plain_http = true`：

```sh
mare --web-listen "$(hostname -i | cut -d' ' -f1):7432" --set webserver.plain_http=true
```

- **公开部署**：不要将监听服务直接暴露到公网。请保持监听在环回地址（`127.0.0.1`）或私网网卡上，通过 SSH 隧道（例如 `ssh -L 7432:127.0.0.1:7432 user@host`）或私有 mesh 网络（如 Tailscale、WireGuard）访问，或在具体网卡绑定前配置 TLS 并显式填写 `trusted_hosts` 与 `allowed_origins`。

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

## 挂载插件资源与路由

插件可以在定义表里声明静态目录，或通过 `webserver` 服务注册动态接口：

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

静态挂载和路由由 `HostManager` 跨会话统一维护，在所属插件或会话销毁时自动撤销。

## 相关页面

- [mare RPC 协议 3.0](../integrate/rpc.md)：`/rpc` 端点使用的完整协议
- [选项](../configure/options.md)：`webserver` 全部字段与默认值
- [插件](../extend/plugins.md)：插件生命周期与 `web` 声明字段
