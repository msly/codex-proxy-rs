  基础配置
  | 配置项 | 默认值 | 含义 |
  |---|---:|---|
  | listen | :8080 | 监听地址；":8080" 会实际绑定到 0.0.0.0:8080 |
  | auth-dir | ./auths | 账号 JSON 文件目录；运行时状态文件也会放这里 |
  | base-url | https://chatgpt.com/backend-api/codex | 上游 Codex 基础地址；优先级高于 backend-domain |
  | backend-domain | chatgpt.com | 当 base-url 为空时用于拼接默认上游地址；如果配了 base-url，会自动从其 host 提取 |
  | backend-resolve-address | 空 | 强制把 backend-domain 解析到指定 IP/host:port |
  | proxy-url | 空 | 出站代理；上游、quota、health、keepalive、refresh 都会走它 |
  | log-level | info | 日志级别：debug/info/warn/error，非法值会回退到 info |
  | api-keys | [] | API key 鉴权列表；空则关闭鉴权 |

  刷新与运行时
  | 配置项 | 默认值 | 含义 |
  |---|---:|---|
  | refresh-interval | 3000 | 定时 refresh 间隔，单位秒 |
  | max-retry | 2 | 上游请求重试次数；总尝试次数 = max-retry + 1 |
  | refresh-concurrency | 50 | refresh 并发数 |
  | startup-async-load | true | 启动时后台加载账号；false 时启动阶段同步加载，没账号会直接报错退出 |
  | startup-load-retry-interval | 10 | 启动后台加载失败后的重试间隔，秒 |
  | shutdown-timeout | 5 | 优雅退出等待时长，秒；最大会被限制到 60 |
  | auth-scan-interval | 30 | 扫描新账号文件的间隔，秒；实际不会大于 refresh-interval |
  | save-workers | 4 | refresh 后写回账号文件的并发 worker 数；最大 32 |
  | cooldown-401-sec | 30 | 上游返回 401 后，该账号冷却时长 |
  | cooldown-429-sec | 60 | 上游返回 429 后，默认冷却时长 |
  | refresh-single-timeout-sec | 30 | 单个 refresh 请求超时，秒 |
  | quota-check-concurrency | 跟随 refresh-concurrency | quota 查询并发；默认配置下最终是 50 |
  | keepalive-interval | 60 | keepalive ping 间隔，秒 |
  | upstream-timeout-sec | 0 | 等待上游首包/响应头超时；0 表示不启用这个超时 |
  | empty-retry-max | 2 | /v1/chat/completions 非流式遇到空响应时，换账号重试次数 |
  | selector | round-robin | 账号选择策略：round-robin 或 quota-first |
  | refresh-batch-size | 0 | refresh 分批大小；0 表示本轮待刷新的账号一次性全处理 |

  健康检查
  | 配置项 | 默认值 | 含义 |
  |---|---:|---|
  | health-check-interval | 300 | 健康检查间隔，秒；0 可关闭 health checker |
  | health-check-max-failures | 3 | 健康检查失败阈值 |
  | health-check-concurrency | 5 | 健康检查并发；最大 128 |
  | health-check-start-delay | 45 | 启动后多久开始首轮健康检查，秒 |
  | health-check-batch-size | 20 | 每轮抽样检查多少账号；0 表示每轮检查全部账号 |
  | health-check-request-timeout | 8 | 单次健康检查请求超时，秒 |

  连接池与网络
  | 配置项 | 默认值 | 含义 |
  |---|---:|---|
  | enable-http2 | true | 出站 HTTP client 是否启用 HTTP/2 |
  | max-idle-conns-per-host | 10 | 每个 host 的空闲连接数上限 |
