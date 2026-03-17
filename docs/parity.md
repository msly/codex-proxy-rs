# Parity checklist (vs Go `codex-proxy`)

本文件用于跟踪 Rust 重写版 `codex-proxy-rs` 与 Go 版 `codex-proxy` 的行为对齐进度。

## HTTP 端点

| 端点 | Rust | 说明 |
|---|---:|---|
| `GET /health` | ✅ | 不鉴权；返回 `status` + `accounts` |
| `POST /check-quota` | ✅ | SSE；查询 wham/usage 并缓存 quota raw JSON |
| `GET /stats` | ✅ | summary + accounts + quota raw JSON cache |
| `GET /v1/models` | ✅ | base models + thinking suffix + `-fast` 变体（fast no-op） |
| `POST /v1/responses` | ✅ | stream/non-stream passthrough（含内部重试 SSE gate） |
| `POST /v1/chat/completions` | ✅ | stream/non-stream（上游 Responses → Chat Completions 转换） |
| `POST /refresh` | ❌ | Go 支持；Rust 未实现管理端点 |
| `POST /v1/messages` | ❌ | Go 支持 Claude Messages API；Rust 未实现 |
| `POST /v1/responses/compact` | ❌ | Go 支持；Rust 未实现 |
| `/v1/responses` websocket upgrade | ❌ | Go 支持 fallback；Rust 未实现 |

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
| 内部重试切换账号 | ✅ | 400/403 不重试，其它可重试 |
| SSE gate（2xx 前不写下游） | ✅ | integration test 已锁定 |

## Token 刷新 / 配额 / 健康检查

| 项 | Rust | 说明 |
|---|---:|---|
| OAuth refresh（/oauth/token） | ✅ | mock tests 覆盖 429/重试/不可重试 |
| refresh loop（定时并发刷新） | ⚠️ | 刷新能力已实现，但未完全对齐 Go 的 loop/热加载策略 |
| wham/usage quota cache | ✅ | raw JSON 缓存并提取 used_percent |
| health checker loop | ✅ | mock tests 覆盖 401 移除 + cancel |
| keepalive ping | ✅ | mock tests 覆盖 HEAD ping + cancel |

## 语义差异（已知）

- `-fast`：Rust 当前为 no-op（仅解析/剥离，不透传 `service_tier=fast`）。
- Chat Completions 响应转换：已实现基础 text/tool_calls/usage 映射，但可能与 Go 在边界事件顺序/细节上仍有差异（需要更多 fixture 回归）。

