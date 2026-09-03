# local-gateway

单二进制本地 LLM 网关（Rust edition 2024 / axum 0.8）：透明转发 OpenAI Chat、OpenAI Responses 与 Anthropic 三种协议，**不做协议转换**（入站什么协议，出站就用同一协议）。仓库尚无提交、无 CI、无 README；代码注释与控制台文案全部为中文，新代码保持该约定。

## 常用命令

- `cargo run` — 启动，默认监听 `127.0.0.1:8787`，控制台在 `/`
- `cargo test` — 全部测试（单元测试 + e2e，均在本地随机端口起真实 axum 服务，秒级完成，无外部依赖）
- `cargo build --release` — 启用 LTO + strip，明显较慢；日常验证用 debug
- 日志级别走 `RUST_LOG`（EnvFilter，默认 `info`）

## 关键事实（易踩坑）

- 配置全部持久化在 SQLite（`settings` / `providers` / `aliases` 表），**不存在 config.toml**——.gitignore 里的 `config.toml` 是历史遗留，Cargo.toml 里的 `toml` 依赖实际已不使用。
- 修改配置的唯一入口是 `state::AppState::update_config`（校验 → 写库 → 原子替换内存缓存），不要绕过它直接改缓存或写库。`listen` 变更需重启进程，其余设置即时生效。
- 数据库文件 `gateway.db` 固定放在 **exe 同级目录**：`cargo run` 时即 `target/debug/gateway.db`，跨次运行保留数据；重置状态就删它。
- `console.html` 与 `live.js` 通过 `include_str!` 编译进二进制（live.js 注入到 console.html 的 `</body>` 前），改前端必须重新编译才生效。
- 供应商 `base_url` 会先归一化（去尾部 `/` 和 `/v1`，转发时补回协议路径），见 `config::normalize_base`。
- 故障转移只在 429/5xx（`proxy::retryable`）且还有候选时发生；其余 4xx 原样透传；全部候选失败时最后一个上游错误体原样回传，网关自产 502 仅限传输层故障。
- 模型名匹配忽略大小写；别名仅单跳解析（不链式）；请求日志只记真实模型名，不记别名。
- SSE 流式逐块透传；上游响应仅透传白名单头（`PASSTHROUGH_RESP_HEADERS`）。

## 结构

- `main.rs` — 入口 + 路由（`/v1/*` → `proxy::forward`；`/admin/api/*` 与 `/` 控制台 → `admin::router`）+ e2e 测试（`mod e2e`）
- `proxy.rs` — 转发管线：别名解析 → drop_params 剥离 → 选址 → 故障转移循环 → token 用量统计 → 请求日志落库
- `route.rs` — 模型→供应商选址（models 登记者为候选，健康缓存里的上次成功者优先）
- `db.rs` — SQLite schema、迁移与查询；`admin.rs` — 管理 API + 内嵌控制台

## 测试约定

- 单元测试内联在各文件的 `#[cfg(test)]` 模块；核心 e2e 流程测试在 `main.rs` 的 `mod e2e`，使用临时目录 `lgw-e2e-*-{pid}`，并断言请求日志落库内容（attempts、缓存粘性、用量）。
- 修改转发、选址或日志逻辑时，需同步更新 `main.rs` e2e 中的断言（如 attempts 序列、日志条数）。
