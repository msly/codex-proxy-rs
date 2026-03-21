# Network config notes (Go parity)

本文件说明 `codex-proxy-rs` 如何把 Go 版 `codex-proxy` 的网络/连接池配置映射到 `reqwest`/`hyper`，以及当前的限制。

## 生效范围

以下 client 会使用同一套“后端 client builder”（支持 `backend-resolve-address`）：

- Codex 上游（`{base-url}/responses`、`{base-url}/responses/compact`）
- 额度查询（`/backend-api/wham/usage`）
- health checker
- keepalive ping

OAuth refresh（`https://auth.openai.com/oauth/token`）使用“通用 client builder”（不会应用 `backend-resolve-address`，因为 host 不同）。

## 配置项映射

### `enable-http2`

- Go：`Transport.ForceAttemptHTTP2`
- Rust：`enable-http2: false` 时启用 `reqwest::ClientBuilder::http1_only()`
- 说明：默认 **true**，与当前 Go 版默认值一致；如遇上游 `GOAWAY ENHANCE_YOUR_CALM`，可按 Go 的建议调低连接池参数或关闭 HTTP/2。
- 范围：该配置只影响出站到 Codex / wham / health checker / keepalive 的 HTTP client，不控制当前代理进程的入站监听协议。

### `backend-resolve-address`

- Go：DialContext 里把 `backend-domain` 的连接重定向到 `backend-resolve-address`（若不带端口，则沿用原请求端口）
- Rust：使用 `reqwest::ClientBuilder::resolve(backend-domain, SocketAddr)`
  - 支持 `ip` / `ip:port` / `hostname` / `hostname:port`
  - 当端口为 `0` 时，`reqwest` 会使用 scheme 的默认端口（如 `https=443` / `http=80`）；如果 URL 自带端口，则以 URL 端口为准

### 连接池与超时

- `max-idle-conns-per-host` → `pool_max_idle_per_host`
- `upstream-timeout-sec`：作为上游首包/响应头等待超时，包裹 `req.send()`；不会额外截断已建立的 SSE body 读取
- `keepalive-interval`：映射到 keepalive ping 循环间隔
- 固定值（对齐 Go）：
  - `pool_idle_timeout = 120s`
  - `tcp_keepalive = 60s`
  - `connect_timeout = 10s`

Rust 版当前只保留与 `reqwest` 实际能力直接对应的连接池参数，不再暴露无效或误导性的 Go 对齐配置项。
