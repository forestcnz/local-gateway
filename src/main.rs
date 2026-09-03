mod admin;
mod config;
mod db;
mod proxy;
mod route;
mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::post;
use axum::Router;

use config::Protocol;
use state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // 数据库固定放在 exe 同级目录，承载全部数据（日志 + 供应商 + 设置）
    let db_path = exe_dir().join("gateway.db");
    let db = Arc::new(db::Db::open(&db_path).unwrap_or_else(|e| {
        eprintln!("打开数据库 {} 失败: {e}", db_path.display());
        std::process::exit(1);
    }));

    // 首次运行：写入默认设置与示例供应商
    if config::Config::ensure_defaults(&db) {
        println!("· 已写入默认配置");
    }

    let cfg = config::Config::from_db(&db);
    let removed = db.compact_logs(cfg.retention_days);
    if removed > 0 {
        println!(
            "· 日志清理：删除 {removed} 条超过 {} 天的记录",
            cfg.retention_days
        );
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建 HTTP 客户端失败");

    let st = Arc::new(AppState::new(cfg, db.clone(), client));

    tokio::spawn(retention_loop(st.clone()));

    let body_limit = (st.config().max_body_mb as usize) << 20;
    let app = build_router(st.clone()).layer(axum::extract::DefaultBodyLimit::max(body_limit));

    let listen = st.config().listen.clone();
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| {
            eprintln!("监听 {listen} 失败: {e}");
            std::process::exit(1);
        });

    let local = listener.local_addr().ok();
    let base = local.map(|a| format!("http://{a}")).unwrap_or_default();
    println!();
    println!("  LOCAL/GATEWAY v{} 已启动", env!("CARGO_PKG_VERSION"));
    println!("  ─────────────────────────────────────────────");
    println!("  控制台   {base}/");
    println!("  协议入口 POST {base}/v1/chat/completions");
    println!("           POST {base}/v1/responses");
    println!("           POST {base}/v1/messages");
    println!("  数据库   {}", db_path.display());
    println!("  ─────────────────────────────────────────────");
    println!();

    axum::serve(listener, app).await.unwrap();
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn build_router(st: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(|st, req| async move { proxy::forward(st, Protocol::Chat, req).await }),
        )
        .route(
            "/v1/responses",
            post(|st, req| async move { proxy::forward(st, Protocol::Responses, req).await }),
        )
        .route(
            "/v1/messages",
            post(|st, req| async move { proxy::forward(st, Protocol::Anthropic, req).await }),
        )
        .merge(admin::router())
        .layer(middleware::from_fn_with_state(st.clone(), cors_mw))
        .with_state(st)
}

/// 每日日志保留清理（读取当前配置的保留天数，支持热更新）。
async fn retention_loop(st: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        let days = st.config().retention_days;
        let removed = st.db.compact_logs(days);
        if removed > 0 {
            tracing::info!("日志清理：删除 {removed} 条超过 {days} 天的记录");
        }
    }
}

/// 可选 CORS：设置 cors = true 时放行任意来源（供网页端直连调试）。
async fn cors_mw(State(st): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    if !st.config().cors {
        return next.run(req).await;
    }
    if req.method() == Method::OPTIONS {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NO_CONTENT;
        add_cors_headers(&mut r);
        return r;
    }
    let mut r = next.run(req).await;
    add_cors_headers(&mut r);
    r
}

fn add_cors_headers(r: &mut Response) {
    let h = r.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    h.insert("access-control-allow-headers", HeaderValue::from_static("*"));
}

#[cfg(test)]
mod e2e {
    use super::*;
    use axum::http::HeaderMap;
    use axum::Json;
    use serde_json::{json, Value};

    async fn spawn(app: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lgw-e2e-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn provider(
        name: &str,
        base_url: &str,
        protocols: Vec<Protocol>,
        api_key: &str,
        models: Vec<&str>,
    ) -> config::Provider {
        config::Provider {
            name: name.into(),
            protocols,
            base_url: base_url.into(),
            api_key: api_key.into(),
            models: models.into_iter().map(String::from).collect(),
            timeout_secs: 120,
            enabled: true,
            extra_headers: Default::default(),
        }
    }

    async fn chat_ok(headers: HeaderMap, body: String) -> Json<Value> {
        let model = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        Json(json!({
            "auth": headers.get("authorization").and_then(|v| v.to_str().ok()),
            "model": model,
            "has_reasoning_effort": body.contains("reasoning_effort"),
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 2},
            },
        }))
    }

    async fn chat_bad() -> (StatusCode, Json<Value>) {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "boom"})))
    }

    async fn msg_ok(headers: HeaderMap, body: String) -> Json<Value> {
        let model = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        Json(json!({
            "x_api_key": headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            "version": headers.get("anthropic-version").and_then(|v| v.to_str().ok()),
            "model": model,
        }))
    }

    async fn stream_ok() -> Response {
        let body = Body::from_stream(futures::stream::iter(vec![
            Ok::<_, std::io::Error>("data: one\n\n"),
            Ok::<_, std::io::Error>("data: two\n\n"),
        ]));
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn proxy_failover_cache_stream_admin() {
        // ── 模拟上游 ──
        let base_good = spawn(Router::new().route("/v1/chat/completions", post(chat_ok))).await;
        let base_bad = spawn(Router::new().route("/v1/chat/completions", post(chat_bad))).await;
        let base_anthro = spawn(Router::new().route("/v1/messages", post(msg_ok))).await;
        let base_stream = spawn(Router::new().route("/v1/chat/completions", post(stream_ok))).await;

        let cfg = config::Config {
            cors: true,
            aliases: [("glm-5.3", "gpt-4o")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            providers: vec![
                provider("bad", &base_bad, vec![Protocol::Chat], "", vec!["gpt-4o"]),
                provider(
                    "good",
                    &base_good,
                    vec![Protocol::Chat],
                    "sk-test-123",
                    vec!["gpt-4o"],
                ),
                provider(
                    "anthro",
                    &base_anthro,
                    vec![Protocol::Anthropic],
                    "sk-ant-1",
                    vec!["claude-3"],
                ),
                provider("streamer", &base_stream, vec![Protocol::Chat], "", vec!["stream-1"]),
            ],
            ..config::Config::default_config()
        };

        let dir = temp_dir("flow");
        let db = Arc::new(db::Db::open(&dir.join("gateway.db")).unwrap());
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let st = Arc::new(AppState::new(cfg, db, client));
        let gw = spawn(build_router(st.clone())).await;
        let cli = reqwest::Client::new();

        // ── 1. 按模型名选址：gpt-4o 登记在 bad、good；bad 500 → 自动切 good ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "gpt-4o", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["auth"], "Bearer sk-test-123");
        assert_eq!(v["model"], "gpt-4o");

        // ── 2b. 参数剥离：reasoning_effort 在转发前被移除 ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "gpt-4o", "messages": [], "reasoning_effort": "high"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["has_reasoning_effort"], false);

        // ── 2c. 别名映射：glm-5.3 → 转发名 gpt-4o ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "glm-5.3", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["model"], "gpt-4o");

        // ── 2. 健康缓存：第二次请求直接命中 good，不再碰 bad ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "gpt-4o"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["auth"], "Bearer sk-test-123");

        // ── 3. 未登记的模型 → 404，附登记提示；被拒请求同样入日志 ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "no-such-model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "gateway_error");
        assert!(v["error"]["message"].as_str().unwrap().contains("未被任何启用中的供应商登记"));
        assert!(st
            .db
            .query_logs(&db::LogFilter {
                q: Some("no-such-model".into()),
                ..Default::default()
            })
            .iter()
            .any(|l| l.status == 404 && l.provider.is_empty()));

        // ── 4. Anthropic 协议透传：x-api-key + anthropic-version 注入 ──
        let r = cli
            .post(format!("{gw}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(&json!({"model": "claude-3"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["x_api_key"], "sk-ant-1");
        assert_eq!(v["version"], "2023-06-01");
        assert_eq!(v["model"], "claude-3");

        // ── 5. SSE 流式逐块透传 ──
        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .header("accept", "text/event-stream")
            .json(&json!({"model": "stream-1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers().get("content-type").unwrap(), "text/event-stream");
        let text = r.bytes().await.unwrap();
        assert_eq!(&text[..], &b"data: one\n\ndata: two\n\n"[..]);

        // ── 6. 请求日志：故障转移轨迹、缓存粘性、token 用量；只记真实模型 ──
        let logs = st.db.query_logs(&db::LogFilter {
            limit: 10,
            ..Default::default()
        });
        let gpt_logs: Vec<&db::ReqLog> = logs.iter().filter(|l| l.model == "gpt-4o").collect();
        // 3 次 gpt-4o 直连调用 + 1 次 glm-5.3 别名调用（别名转换后同模型）
        assert_eq!(gpt_logs.len(), 4);
        // recent() 新→旧：[别名调用, 参数剥离调用, 缓存直连, 故障转移]
        assert_eq!(gpt_logs[3].attempts, vec!["bad:500", "good:200"]);
        assert_eq!(gpt_logs[2].attempts, vec!["good:200"]); // 缓存粘性生效
        assert_eq!(gpt_logs[1].attempts, vec!["good:200"]);
        assert_eq!(gpt_logs[0].attempts, vec!["good:200"]); // 别名调用走缓存直达
        for l in &gpt_logs {
            assert_eq!(l.provider, "good");
            assert_eq!(l.status, 200);
            // usage 从上游响应中解析
            assert_eq!((l.tokens_in, l.tokens_out, l.tokens_cached), (10, 5, 2));
        }
        // 日志只记真实模型，不出现别名名
        assert!(logs.iter().all(|l| l.model != "glm-5.3"));

        // ── 7. 管理 API：创建供应商 + 名称查重 + 控制台可达 ──
        let r = cli
            .post(format!("{gw}/admin/api/providers"))
            .json(&json!({
                "name": "relay-x",
                "protocols": ["chat", "responses"],
                "base_url": "https://relay.example.com/v1/",
                "api_key": "sk-x",
                "models": ["deepseek-v3"]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = cli
            .post(format!("{gw}/admin/api/providers"))
            .json(&json!({"name": "relay-x", "protocols": ["chat"], "base_url": "https://x.com"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        let list: Value = cli
            .get(format!("{gw}/admin/api/providers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let relay = list["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "relay-x")
            .unwrap();
        assert_eq!(relay["api_key_set"], true);
        let page = cli
            .get(format!("{gw}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("/admin/api/stats"));
        assert!(page.contains("LOCAL-GATEWAY"));

        // ── 8. CORS 预检（config 里 cors = true） ──
        let r = cli
            .request(Method::OPTIONS, format!("{gw}/v1/chat/completions"))
            .header("origin", "http://localhost:3000")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        assert_eq!(r.headers().get("access-control-allow-origin").unwrap(), "*");

        // 供应商写入 DB（不再有 config.toml）
        let names: Vec<String> = st
            .db
            .providers_load()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(names.contains(&"relay-x".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn last_upstream_error_relayed_verbatim() {
        // 所有候选都失败时，最后一个上游的 5xx 错误体应原样透传（透明代理行为），
        // 网关自产的 502 只出现在连接失败等传输层故障。
        let base_bad = spawn(Router::new().route("/v1/chat/completions", post(chat_bad))).await;
        let cfg = config::Config {
            providers: vec![provider("bad1", &base_bad, vec![Protocol::Chat], "", vec!["any"])],
            ..config::Config::default_config()
        };
        let dir = temp_dir("fail");
        let client = reqwest::Client::builder().build().unwrap();
        let db = Arc::new(db::Db::open(&dir.join("gateway.db")).unwrap());
        let st = Arc::new(AppState::new(cfg, db, client));
        let gw = spawn(build_router(st.clone())).await;
        let cli = reqwest::Client::new();

        let r = cli
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "any"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 500);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"], "boom");

        let logs = st.db.query_logs(&db::LogFilter {
            limit: 5,
            ..Default::default()
        });
        assert_eq!(logs[0].status, 500);
        assert_eq!(logs[0].provider, "bad1");
        assert_eq!(logs[0].attempts, vec!["bad1:500"]);
        assert!(logs[0].error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
