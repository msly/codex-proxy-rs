# codex-proxy-rs

Rust 重写版 `codex-proxy`（参考同级目录 `../codex-proxy` 的 Go 实现）。

提供 OpenAI 兼容的 HTTP 端点，将请求转换为 ChatGPT Codex 后端 `/backend-api/codex/responses` 请求并在多账号之间做内部重试。

## 功能概览

- OpenAI 兼容端点：
  - `POST /v1/responses`（流式/非流式，透传上游 Responses SSE 或 response 对象）
  - `POST /v1/chat/completions`（流式/非流式，上游 Responses SSE → Chat Completions 转换）
  - `GET /v1/models`（生成 thinking 后缀与 `-fast` 变体）
- 管理端点：
  - `GET /stats`（账号 summary + 统计 + quota raw JSON cache）
  - `POST /check-quota`（SSE，批量查询 `/backend-api/wham/usage`）
  - `GET /health`（不鉴权）
- 多账号池 + 内部重试：
  - 从 `auth-dir` 读取 `*.json`（access_token/refresh_token/account_id/email/expired）
  - 账号切换重试（400/403 不重试，其它可重试）
  - **SSE gate**：仅在拿到上游 **2xx** 后才向下游返回流式响应（客户端“无感重试”）
- 后台任务（可取消）：
  - health checker：探测 `/responses` 并对 401/403/429 做移除/冷却处理
  - keepalive：周期性 HEAD ping 上游，保持连接池热度

## 运行

```bash
cd codex-proxy-rs
cargo run -- --config config.yaml
```

## Docker

```bash
cd codex-proxy-rs
docker build -t codex-proxy-rs .
docker run --rm -p 8080:8080 \
  -v $(pwd)/config.yaml:/app/config.yaml:ro \
  -v $(pwd)/auths:/app/auths \
  codex-proxy-rs
```

如果构建阶段出现 DNS/解析超时（如 `crates.io` / `deb.debian.org`），可尝试：

```bash
docker build --network=host -t codex-proxy-rs .
```

也提供 `docker-compose.yml`：

```bash
docker compose up --build
```

### 配置示例（`config.yaml`）

```yaml
# 监听地址（Go 兼容写法）
listen: ":8080"

# 账号目录（里面放 *.json token 文件）
auth-dir: "./auths"

# 上游 Codex base url（不配则按 backend-domain 拼）
base-url: "https://chatgpt.com/backend-api/codex"
backend-domain: "chatgpt.com"

# 可选：全局代理（同时用于 upstream + quota + health + keepalive）
proxy-url: ""

# 日志级别：debug|info|warn|error
log-level: "info"

# 内部重试次数（0 表示不重试；总尝试次数 = max-retry + 1）
max-retry: 2

# API key 鉴权（为空则不鉴权；支持 Authorization Bearer / x-api-key / api-key）
api-keys:
  - "your-api-key"

# health checker（0 禁用）
health-check-interval: 300
health-check-max-failures: 3
health-check-concurrency: 5
health-check-start-delay: 45
health-check-batch-size: 20
health-check-request-timeout: 8

# 其他配置项仍保留以对齐 Go（部分未完全接入）
refresh-interval: 3000
refresh-concurrency: 50
enable-http2: true
startup-async-load: true
```

### auth 文件示例（`auths/a.json`）

```json
{
  "access_token": "at-xxx",
  "refresh_token": "rt-xxx",
  "account_id": "acc-xxx",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}
```

## API key 鉴权

当 `api-keys` 配置非空时：

- 受保护：`/v1/*`、`/stats`、`/check-quota`
- 不鉴权：`/health`

支持以下 header 任一方式：

```bash
Authorization: Bearer <api-key>
x-api-key: <api-key>
api-key: <api-key>
```

## cURL 示例

### 1) 模型列表

```bash
curl -sS http://127.0.0.1:8080/v1/models \
  -H 'Authorization: Bearer your-api-key'
```

### 2) Responses API（非流式）

```bash
curl -sS http://127.0.0.1:8080/v1/responses \
  -H 'Authorization: Bearer your-api-key' \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.4","stream":false,"input":"hello"}'
```

### 3) Responses API（流式）

```bash
curl -N http://127.0.0.1:8080/v1/responses \
  -H 'Authorization: Bearer your-api-key' \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.4","stream":true,"input":"hello"}'
```

### 4) Chat Completions（非流式）

```bash
curl -sS http://127.0.0.1:8080/v1/chat/completions \
  -H 'Authorization: Bearer your-api-key' \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.4","stream":false,"messages":[{"role":"user","content":"hello"}]}'
```

### 5) Chat Completions（流式）

```bash
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'Authorization: Bearer your-api-key' \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.4","stream":true,"messages":[{"role":"user","content":"hello"}]}'
```

### 6) 查询剩余额度（SSE）

```bash
curl -N -X POST http://127.0.0.1:8080/check-quota \
  -H 'Authorization: Bearer your-api-key'
```

### 7) 查看账号统计 + quota cache

```bash
curl -sS http://127.0.0.1:8080/stats \
  -H 'Authorization: Bearer your-api-key'
```

## 模型后缀：thinking / fast

之所以用 `model` 名后缀表达这些开关，是为了兼容更多“只允许选择 model、不方便改请求体字段”的客户端；代理在内部会剥离后缀并映射到上游请求参数。

### thinking（通过模型名后缀控制）

客户端可用 `model` 后缀表达“思考等级”，代理会将其写入上游请求的 `reasoning.effort`，并将去后缀的 base model 作为真实模型名：

- `gpt-5.4-low`
- `gpt-5.4-medium`
- `gpt-5.4-high`
- `gpt-5.4-xhigh`
- `gpt-5.4-max`
- `gpt-5.4-none`
- `gpt-5.4-auto`（等价映射到 `medium`）
- `model-<budget>`（数字预算 > 100，会映射为等级）

### fast（`-fast` 后缀，当前为 no-op）

`-fast` 会被解析/剥离以保持兼容，但当前 **不影响** 上游请求（不会透传 `service_tier=fast`）。

因此：

- `gpt-5.4` 与 `gpt-5.4-fast` 行为一致
- `/v1/models` 仍会列出 `*-fast` 变体用于兼容旧客户端

## 流程图

### /v1/responses（内部重试 + SSE gate）

```mermaid
sequenceDiagram
  participant C as Client
  participant P as codex-proxy-rs
  participant M as Manager/Selector
  participant U as Upstream Codex (/responses)

  C->>P: POST /v1/responses
  P->>P: api-key auth (optional)
  P->>P: apply_thinking(model suffix)
  P->>P: translate OpenAI→Codex request
  loop internal retry
    P->>M: pick_excluding(model)
    P->>U: POST /backend-api/codex/responses (Bearer access_token)
    U-->>P: status (2xx or retryable)
  end
  P-->>C: (only after upstream 2xx) SSE passthrough / JSON
```

### /check-quota（wham/usage cache）

```mermaid
flowchart TD
  A[POST /check-quota] --> B[iterate accounts snapshot]
  B --> C[GET /backend-api/wham/usage]
  C --> D[store quota raw JSON per account]
  D --> E[emit SSE progress]
  E --> F[done]
```

## 现状与差异（相对 Go）

- 已实现：`/v1/responses`、`/v1/chat/completions`、`/v1/models`、`/stats`、`/check-quota`、health checker、keepalive
- 未实现（待补齐）：`/v1/messages`、`/v1/responses/compact`、websocket、`/refresh` 管理端点、后台 refresh loop 的完整对齐等

更多对齐清单见 `docs/parity.md`。
