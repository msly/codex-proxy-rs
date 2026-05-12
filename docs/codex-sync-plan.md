# Codex Sync Plan

## Scope

This batch only covers the following four items:

1. Pass through and preserve Codex client identity headers.
2. Treat Codex capacity errors as retryable / 429-like failures.
3. Support `response.append` and follow-up transcript updates in the websocket fallback path.
4. Update the default Codex client version and user-agent identifiers.

The implementation should stay narrowly scoped to:

- `src/upstream/codex.rs`
- `src/api/mod.rs`
- related existing tests

This batch should not touch:

- config
- main
- repl
- model policy/config systems
- privacy / plan_type refresh logic
- native upstream websocket proxying

## Implementation Plan

### 1. Codex Identity Header Passthrough

#### Goal

Upstream HTTP requests should no longer rely only on hard-coded header values. When the downstream client already provides compatible identity headers, preserve them. When they are absent, continue to fall back to safe defaults.

#### Files

- `src/upstream/codex.rs`
- `src/api/mod.rs`

#### Detailed Work

- Keep `CodexClient::execute` as the single upstream request path and pass optional passthrough headers through `UpstreamRequest`.
- Restrict passthrough to a whitelist:
  - `Version`
  - `Session_id`
  - `Originator`
  - `X-Codex-Turn-Metadata`
  - `X-Client-Request-Id`
- Preserve current fallback behavior:
  - generate a UUID when `Session_id` is missing
  - use default `Originator` when missing
  - use default `Version` when missing
- Do not introduce unrestricted downstream header forwarding.
- Wire the websocket fallback path so those whitelisted headers can be carried into upstream Codex requests.

### 2. Capacity Errors as Retryable

#### Goal

If the upstream response body indicates a model-capacity failure such as "selected model is at capacity", treat the failure like a retryable 429-style condition even if the HTTP status is not 429.

#### Files

- `src/upstream/codex.rs`

#### Detailed Work

- Add a helper to detect explicit capacity-error messages from the upstream response body.
- Keep the match intentionally narrow to avoid accidental over-matching.
- When a capacity condition is detected:
  - treat it as retryable
  - reuse existing 429 cooldown handling
  - allow account rotation to continue
- Keep current non-retryable handling for 400 and 403.

### 3. Websocket Fallback Support for `response.append`

#### Goal

The websocket fallback bridge should accept more than `response.create`. At minimum, it should support `response.append` so compatible Responses clients do not fail immediately on follow-up transcript updates.

#### Files

- `src/api/mod.rs`

#### Detailed Work

- Extend websocket event handling to accept:
  - `response.create`
  - `response.append`
  - existing `response.cancel`
  - existing `response.close`
- Maintain minimal per-connection state:
  - last request body
  - last effective model
  - any minimal fields needed to reissue the fallback HTTP/SSE request safely
- For `response.append`, implement a minimal compatibility merge strategy:
  - merge or replace `input`
  - preserve `model`
  - inherit the previous model when omitted
- Continue using the existing HTTP/SSE fallback path.
- Do not implement full transcript-repair caches in this batch.

### 4. Update Default Codex Client Version

#### Goal

Refresh the built-in Codex client version and user-agent constants so the fallback identity better matches newer client behavior, while still allowing passthrough values to override defaults.

#### Files

- `src/upstream/codex.rs`

#### Detailed Work

- Update:
  - `CODEX_CLIENT_VERSION`
  - `CODEX_USER_AGENT`
- Keep default constants as fallback values only.
- If downstream already provides `Version`, use the downstream value.
- Do not add dynamic platform-dependent user-agent generation in this batch.

## Acceptance Criteria

### 1. Identity Header Passthrough

Required tests:

- When passthrough headers are absent, requests still include default:
  - `Version`
  - generated `Session_id`
  - default `Originator`
- When passthrough headers are present, upstream receives the provided values instead of defaults.
- `X-Codex-Turn-Metadata` is forwarded to upstream.
- `X-Client-Request-Id` is forwarded to upstream.

Behavior constraints:

- No unrestricted header passthrough.
- No regression for existing requests that do not provide these headers.

### 2. Capacity Retry Handling

Required tests:

- A capacity-style upstream error triggers retry/account rotation.
- If another account is available, the request succeeds after retry.
- If no other account is available, the final error is returned cleanly.

Behavior constraints:

- Existing 429 handling must still work.
- Existing 400 and 403 non-retryable rules must not regress.

### 3. `response.append` Websocket Fallback

Required tests:

- `response.append` is accepted instead of returning "unsupported event type".
- If `model` is omitted on append, the previous effective model is reused.
- The merged append request still flows through the fallback HTTP/SSE bridge and returns a valid result.

Behavior constraints:

- This batch does not need to implement CLIProxyAPI-style transcript repair caches.
- This batch does not need to proxy native upstream websockets.

### 4. Client Version Update

Required tests:

- Default outgoing `Version` reflects the updated constant.
- Default outgoing `User-Agent` reflects the updated constant.
- Passthrough `Version` overrides the default.

### 5. Overall Verification

Minimum required verification before completion:

- `cargo fmt`
- newly added tests for this batch
- existing targeted upstream / websocket fallback test subsets

Completion conditions:

- all targeted tests pass
- no unrelated files are modified
- changes remain limited to the scoped files above

## Explicit Non-Goals

This batch intentionally excludes:

- websocket tool-call repair caches from CLIProxyAPI
- `sub2api` messages-model mapping and instruction-template config systems
- refresh-time plan-type refresh via backend-api
- privacy opt-out flows
- native upstream websocket support
