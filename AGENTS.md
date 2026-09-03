# AGENTS.md — local-gateway

## 项目概览

单 crate Rust 应用：AI 模型 API 网关，支持 OpenAI / Anthropic / Responses 三种协议透传。基于 Axum + Tokio，配置与日志全部持久化在单一 SQLite 文件（`gateway.db`，与 exe 同级）。

## 构建与测试

```bash
cargo build                    # 构建
cargo test                     # 运行全部测试（含 e2e 模块内的集成测试）
cargo test -- --test-threads=1 # 若 e2e 测试有端口竞争时使用
```

- 没有 CI workflow、没有 lint/typecheck 脚本；依赖 `cargo build` 和 `cargo test` 即可验证。
- 无额外 dev-dependencies 或 feature flags 需要关注。
- `rusqlite` 使用 `bundled` feature，不需要系统 SQLite。

## 架构要点

| 文件 | 职责 |
|------|------|
| `main.rs` | 入口、路由构建、CORS 中间件、日志保留循环 |
| `config.rs` | `Config`/`Provider` 数据模型，DB 读写，`base_url` 归一化 |
| `state.rs` | `AppState`：配置的 `RwLock<Arc<Config>>` 缓存 + 健康缓存 |
| `route.rs` | 按模型名选址：协议过滤 → 模型匹配 → 健康缓存优先 |
| `proxy.rs` | 转发核心：参数剥离 → 别名解析 → 故障转移 → SSE 流式透传 → usage 解析 |
| `admin.rs` | 控制台 HTML（内嵌 `console.html` + `live.js`）及 REST 管理 API |
| `db.rs` | SQLite WAL 模式，单写者 Mutex；`requests`/`providers`/`settings`/`aliases` 四张表 |

**关键流程**：请求进入 → `apply_aliases`（单次，不链式）→ `resolve` 选址（大小写不敏感）→ 逐候选转发，429/5xx 自动切换 → 成功后写入健康缓存 → 日志记录（含 token 用量）。

## 注意事项

- **无 config.toml**：配置完全在 SQLite 中；`.gitignore` 排除了 `*.db`。
- **别名是单次转换**：别名目标不会再被解析为别名，不支持链式。
- **`drop_params` 默认剥离 `reasoning_effort`**：转发前从请求体移除该字段。
- **`listen` 变更需重启**：设置中修改 listen 地址后必须重启进程，其余设置即时生效。
- **SSE 流式不设超时**：`timeout_secs` 仅约束等待响应头阶段；流传输阶段无上限。
- **健康缓存键**为 `协议:真实模型`（非别名），缓存仅在内存中，重启后重置。
- **日志保留**：启动时和后台每 6 小时按 `retention_days` 清理。
