# Parity checklist (vs Go `codex-proxy`)

本文件用于跟踪 Rust 重写版 `codex-proxy-rs` 与 Go 版 `codex-proxy` 的行为对齐进度。

## HTTP 端点

| 端点 | Rust | 说明 |
|---|---:|---|
| `GET /` | ✅ | embed `assets/index.html`（统计/展示首页） |
| `GET /health` | ✅ | 不鉴权；返回 `status` + `accounts` |
| `POST /check-quota` | ✅ | SSE；查询 wham/usage 并缓存 quota raw JSON |
| `GET /stats` | ✅ | summary + rpm + token totals + accounts + quota raw JSON cache |
| `GET /v1/models` | ✅ | Go 同款 per-model suffix matrix + `gpt-5.4-mini` + `-fast` |
| `POST /v1/responses` | ✅ | stream/non-stream passthrough（含内部重试 SSE gate） |
| `POST /v1/chat/completions` | ✅ | stream/non-stream（上游 Responses → Chat Completions 转换） |
| `POST /refresh` | ✅ | SSE；强制刷新所有账号 Token（成功后会查询 quota） |
| `POST /v1/messages` | ✅ | stream/non-stream（Codex Responses SSE → Claude Messages 格式） |
| `POST /v1/responses/compact` | ✅ | stream/non-stream passthrough（上游 `/responses/compact`） |
| `/v1/responses` websocket upgrade | ✅ | 支持 fallback：`response.create` → HTTP/SSE 转发，并将 SSE payload 作为 WS text frame 透传 |

## 中间件与基础行为

| 项 | Rust | 说明 |
|---|---:|---|
| OPTIONS 预检直通 | ✅ | 全局 OPTIONS 返回 204；绕过 api-key 鉴权 |
| CORS headers | ✅ | `Access-Control-Allow-Origin` + `Vary: Origin` |
| gzip | ✅ | 仅 non-`/v1/*`；SSE 不压缩 |

## 鉴权

| 项 | Rust | 说明 |
|---|---:|---|
| API key middleware | ✅ | `Authorization: Bearer` / `x-api-key` / `api-key` |
| 鉴权覆盖范围 | ✅ | `/v1/*` + 管理端点；`/health` 不鉴权 |

## 账号池与重试

| 项 | Rust | 说明 |
|---|---:|---|
| `auth-dir` 加载 `*.json` | ✅ | 跳过非法文件；缺 refresh_token 的文件会被跳过 |
| Round-robin 选择器 | ✅ | used_percent 排序（unknown 排最后） |
| Quota-first 选择器 | ✅ | `selector: quota-first` 按 used_percent 最低优先 |
| 内部重试切换账号 | ✅ | 400/403 不重试，其它可重试 |
| non-stream 空响应换号重试 | ✅ | `empty-retry-max` 仅用于 Chat Completions 非流式 |
| SSE gate（2xx 前不写下游） | ✅ | integration test 已锁定 |

## 上游请求头（Codex）

| 项 | Rust | 说明 |
|---|---:|---|
| `Version` / `User-Agent` / `Origin` / `Referer` / `Originator` | ✅ | 与 Go 保持一致 |
| `Session_id` | ✅ | 每次请求生成 UUID（对齐 Go） |

## 上游失败副作用（账号状态）

| 项 | Rust | 说明 |
|---|---:|---|
| 401 | ✅ | `cooldown-401-sec` + 后台 refresh（失败按 `auth_401` 移除） |
| 429 | ✅ | 解析 `resets_at`/`resets_in_seconds`，默认回退 `cooldown-429-sec` |
| 403 | ✅ | 5min 冷却；403 不重试 |

## `/stats` schema

| 项 | Rust | 说明 |
|---|---:|---|
| `summary.rpm` | ✅ | 每个 `AppState` 独立统计最近一分钟成功请求数 |
| `summary.total_input_tokens` / `summary.total_output_tokens` | ✅ | 汇总账号 usage 统计 |
| `plan_type` / `last_used_at` / `last_refreshed_at` / `cooldown_until` | ✅ | UI 关键字段补齐 |
| `quota_exhausted` / `quota_resets_at` | ✅ | 429 冷却对齐 |
| `usage.*` | ✅ | completions/input/output/total tokens 统计 |

## Token 刷新 / 配额 / 健康检查

| 项 | Rust | 说明 |
|---|---:|---|
| OAuth refresh（/oauth/token） | ✅ | mock tests 覆盖 429/重试/不可重试 |
| refresh loop（定时并发刷新） | ✅ | `auth-scan-interval`、`refresh-batch-size`、`cooldown-429-sec` 已接线；支持 shutdown 取消 |
| wham/usage quota cache | ✅ | raw JSON 缓存并提取 used_percent |
| health checker loop | ✅ | mock tests 覆盖 401 移除 + cancel |
| keepalive ping | ✅ | `keepalive-interval` 已接线；mock tests 覆盖 HEAD ping + cancel |

## 网络配置（transport knobs）

对齐说明见 `docs/network.md`。

## 语义差异（已知）

- `/check-quota`：Rust 会更新 used_percent 排序缓存；Go 当前实现只更新 quota raw_data（不刷新 used_percent cache）。如需严格对齐，可单独调整为 Go 行为并补测试。
- Chat Completions 响应转换：已实现基础 text/tool_calls/usage 映射，但可能与 Go 在边界事件顺序/细节上仍有差异（需要更多 fixture 回归）。
