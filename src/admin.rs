use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{normalize_base, Protocol, Provider};
use crate::db;
use crate::state::AppState;

const CONSOLE_HTML: &str = include_str!("console.html");
const LIVE_JS: &str = include_str!("live.js");


/// 控制台页面：内嵌 HTML + 注入实时数据脚本，编译进二进制。
pub fn console_html() -> &'static str {
    static CONSOLE: OnceLock<String> = OnceLock::new();
    CONSOLE.get_or_init(|| {
        CONSOLE_HTML.replace("</body>", &format!("\n<script>\n{LIVE_JS}\n</script>\n</body>"))
    })
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

type ApiResult = Result<Response, ApiError>;

fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(console))
        .route("/admin/api/stats", get(stats))
        .route("/admin/api/logs", get(logs_handler).delete(clear_logs))
        .route("/admin/api/settings", get(get_settings).put(put_settings))
        .route("/admin/api/reload", post(reload))
        .route(
            "/admin/api/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/admin/api/providers/{name}",
            put(update_provider).patch(patch_provider).delete(delete_provider),
        )
        .route("/admin/api/providers/{name}/test", post(test_provider))
        .route("/admin/api/aliases", get(list_aliases))
        .route(
            "/admin/api/aliases/{alias}",
            put(upsert_alias).delete(delete_alias),
        )
}

// ---------- 控制台 ----------

async fn console() -> axum::response::Html<&'static str> {
    axum::response::Html(console_html())
}

// ---------- 统计 / 日志 ----------

#[derive(Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
    protocol: Option<String>,
    provider: Option<String>,
    /// 状态类别：2 / 4 / 5（2xx / 4xx / 5xx）
    status: Option<String>,
    /// 时间范围：1h / 24h / today
    since: Option<String>,
    /// model / path / provider 子串搜索
    q: Option<String>,
}

async fn stats(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.config();
    let today = chrono::Local::now().date_naive();
    let st_today = st.db.day_stats(today);
    let yesterday = st.db.day_stats(today - chrono::Duration::days(1)).requests;
    let total = cfg.providers.len();
    let active = cfg.providers.iter().filter(|p| p.enabled).count();
    ok_json(json!({
        "listen": cfg.listen,
        "version": env!("CARGO_PKG_VERSION"),
        "requests": st_today.requests,
        "success_rate": (st_today.success_rate() * 10.0).round() / 10.0,
        "avg_latency_ms": st_today.avg_latency_ms(),
        "active_providers": active,
        "total_providers": total,
        "retention_days": cfg.retention_days,
        "total_logs": st.db.total_logs(),
        "yesterday_requests": yesterday,
        "hourly": st.db.hourly_counts(chrono::Local::now()),
    }))
}

async fn logs_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<LogsQuery>,
) -> Response {
    let since_epoch = match q.since.as_deref() {
        Some("1h") => Some(chrono::Local::now().timestamp() - 3600),
        Some("24h") => Some(chrono::Local::now().timestamp() - 86_400),
        Some("today") => Some(db::day_start_epoch(chrono::Local::now().date_naive())),
        _ => None,
    };
    let filter = db::LogFilter {
        limit: q.limit.unwrap_or(100).clamp(1, 1000),
        protocol: q.protocol.filter(|s| !s.is_empty()),
        provider: q.provider.filter(|s| !s.is_empty()),
        status_class: q
            .status
            .as_deref()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|c| matches!(c, 2 | 4 | 5)),
        since_epoch,
        q: q.q
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string()),
    };
    let logs = st.db.query_logs(&filter);
    ok_json(json!({ "logs": logs, "count": logs.len() }))
}

async fn clear_logs(State(st): State<Arc<AppState>>) -> ApiResult {
    let n = st
        .db
        .clear_logs()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("清空日志失败: {e}")))?;
    Ok(ok_json(json!({ "ok": true, "deleted": n })))
}

// ---------- 设置 ----------

async fn get_settings(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.config();
    ok_json(json!({
        "listen": cfg.listen,
        "upstream_timeout_secs": cfg.upstream_timeout_secs,
        "sse_passthrough": cfg.sse_passthrough,
        "max_body_mb": cfg.max_body_mb,
        "cors": cfg.cors,
        "retention_days": cfg.retention_days,
        "drop_params": cfg.drop_params,
    }))
}

async fn put_settings(State(st): State<Arc<AppState>>, Json(v): Json<Value>) -> ApiResult {
    let get_u64 = |k: &str| v.get(k).and_then(|x| x.as_u64());
    let get_bool = |k: &str| v.get(k).and_then(|x| x.as_bool());
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);

    let listen = get_str("listen");
    if let Some(l) = &listen {
        if l.is_empty() || !l.contains(':') {
            return Err(err(StatusCode::BAD_REQUEST, "listen 需为 host:port 形式"));
        }
    }

    st.update_config(|cfg| {
        if let Some(x) = listen.clone() { cfg.listen = x; }
        if let Some(x) = get_u64("upstream_timeout_secs") { cfg.upstream_timeout_secs = x; }
        if let Some(x) = get_bool("sse_passthrough") { cfg.sse_passthrough = x; }
        if let Some(x) = get_u64("max_body_mb") { cfg.max_body_mb = x.max(1); }
        if let Some(x) = get_bool("cors") { cfg.cors = x; }
        if let Some(x) = get_u64("retention_days") { cfg.retention_days = x.clamp(1, 3650) as u32; }
        if let Some(x) = get_str("drop_params") { cfg.drop_params = x; }
        Ok(())
    })
    .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;

    Ok(ok_json(json!({
        "ok": true,
        "note": "listen 变更需重启进程后生效，其余即时生效"
    })))
}

async fn reload(State(st): State<Arc<AppState>>) -> ApiResult {
    st.reload_config();
    Ok(ok_json(json!({ "ok": true })))
}

// ---------- 模型别名（全局，与供应商无关） ----------

async fn list_aliases(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.config();
    let list: Vec<Value> = cfg
        .aliases
        .iter()
        .map(|(k, v)| json!({ "alias": k, "model": v }))
        .collect();
    ok_json(json!({ "aliases": list }))
}

/// upsert：PUT /admin/api/aliases/{alias}，body {"model": "真实模型名"}。
async fn upsert_alias(
    State(st): State<Arc<AppState>>,
    Path(alias): Path<String>,
    Json(v): Json<Value>,
) -> ApiResult {
    let alias = alias.trim().to_string();
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "缺少 model（真实模型名）"))?
        .to_string();
    st.update_config(|cfg| {
        cfg.aliases.insert(alias.clone(), model.clone());
        Ok(())
    })
    .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

async fn delete_alias(
    State(st): State<Arc<AppState>>,
    Path(alias): Path<String>,
) -> ApiResult {
    st.update_config(|cfg| {
        if cfg.aliases.remove(&alias).is_none() {
            return Err(format!("别名 {alias} 不存在"));
        }
        Ok(())
    })
    .map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

// ---------- 供应商 ----------

fn mask_provider(index: usize, p: &Provider) -> Value {
    json!({
        "index": index,
        "name": p.name,
        "protocols": p.protocols.iter().map(|x| x.as_str()).collect::<Vec<_>>(),
        "base_url": p.base_url,
        "api_key_set": !p.api_key.is_empty(),
        "models": p.models,
        "timeout_secs": p.timeout_secs,
        "enabled": p.enabled,
        "extra_headers": p.extra_headers,
    })
}

async fn list_providers(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.config();
    let list: Vec<Value> = cfg
        .providers
        .iter()
        .enumerate()
        .map(|(i, p)| mask_provider(i + 1, p))
        .collect();
    ok_json(json!({ "providers": list }))
}

/// 从请求体解析供应商字段。api_key 返回 None 表示「保持原值」。
fn parse_provider_input(v: &Value) -> Result<(String, Vec<Protocol>, String, Option<String>, Vec<String>, Option<u64>, Option<bool>, Option<BTreeMap<String, String>>), ApiError> {
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "缺少 name"))?
        .to_string();
    let protocols: Vec<Protocol> = v
        .get("protocols")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| serde_json::from_str::<Protocol>(&format!("\"{s}\"")))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|e: serde_json::Error| err(StatusCode::BAD_REQUEST, format!("协议字段无效: {e}")))?
        .unwrap_or_default();
    let base_url = v
        .get("base_url")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "缺少 base_url"))?
        .to_string();
    let api_key = v
        .get("api_key")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let models = v
        .get("models")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let timeout_secs = v.get("timeout_secs").and_then(|x| x.as_u64());
    let enabled = v.get("enabled").and_then(|x| x.as_bool());
    let extra_headers = v.get("extra_headers").and_then(|x| x.as_object()).map(|m| {
        m.iter()
            .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<BTreeMap<_, _>>()
    });
    Ok((
        name, protocols, base_url, api_key, models, timeout_secs, enabled, extra_headers,
    ))
}

async fn create_provider(State(st): State<Arc<AppState>>, Json(v): Json<Value>) -> ApiResult {
    let (name, protocols, base_url, api_key, models, timeout_secs, enabled, extra_headers) =
        parse_provider_input(&v)?;
    if protocols.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "至少勾选一种协议"));
    }
    st.update_config(|cfg| {
        if cfg.providers.iter().any(|p| p.name == name) {
            return Err(format!("供应商 {name} 已存在"));
        }
        cfg.providers.push(Provider {
            name,
            protocols,
            base_url,
            api_key: api_key.unwrap_or_default(),
            models,
            timeout_secs: timeout_secs.unwrap_or(120),
            enabled: enabled.unwrap_or(true),
            extra_headers: extra_headers.unwrap_or_default(),
        });
        Ok(())
    })
    .map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

async fn update_provider(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(v): Json<Value>,
) -> ApiResult {
    let (new_name, protocols, base_url, api_key, models, timeout_secs, enabled, extra_headers) =
        parse_provider_input(&v)?;
    if protocols.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "至少勾选一种协议"));
    }
    st.update_config(|cfg| {
        if new_name != name && cfg.providers.iter().any(|p| p.name == new_name) {
            return Err(format!("供应商 {new_name} 已存在"));
        }
        let p = cfg
            .providers
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("供应商 {name} 不存在"))?;
        p.name = new_name;
        p.protocols = protocols;
        p.base_url = base_url;
        if let Some(k) = api_key {
            p.api_key = k; // 留空表示保持原 Key
        }
        p.models = models;
        if let Some(t) = timeout_secs { p.timeout_secs = t; }
        if let Some(e) = enabled { p.enabled = e; }
        if let Some(h) = extra_headers { p.extra_headers = h; }
        Ok(())
    })
    .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

async fn patch_provider(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(v): Json<Value>,
) -> ApiResult {
    let enabled = v
        .get("enabled")
        .and_then(|x| x.as_bool())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "缺少 enabled 字段"))?;
    st.update_config(|cfg| {
        let p = cfg
            .providers
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("供应商 {name} 不存在"))?;
        p.enabled = enabled;
        Ok(())
    })
    .map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

async fn delete_provider(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult {
    st.update_config(|cfg| {
        let before = cfg.providers.len();
        cfg.providers.retain(|p| p.name != name);
        if cfg.providers.len() == before {
            return Err(format!("供应商 {name} 不存在"));
        }
        Ok(())
    })
    .map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok(ok_json(json!({ "ok": true })))
}

/// 连通性测试：GET {base}/v1/models（OpenAI 与 Anthropic 均提供该端点）。
async fn test_provider(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    let cfg = st.config();
    let p = cfg
        .find_provider(&name)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("供应商 {name} 不存在")))?;

    let url = format!("{}/v1/models", normalize_base(&p.base_url));
    let mut rb = st.client.get(&url);
    let openai_style = p
        .protocols
        .iter()
        .any(|x| matches!(x, Protocol::Chat | Protocol::Responses));
    if openai_style && !p.api_key.is_empty() {
        rb = rb.header("authorization", format!("Bearer {}", p.api_key));
    }
    if p.protocols.contains(&Protocol::Anthropic) && !p.api_key.is_empty() {
        rb = rb
            .header("x-api-key", &p.api_key)
            .header("anthropic-version", "2023-06-01");
    }
    for (k, v) in &p.extra_headers {
        rb = rb.header(k.as_str(), v.as_str());
    }

    let started = std::time::Instant::now();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(15), st.client.execute(rb.build().map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?))
        .await
        .map_err(|_| err(StatusCode::GATEWAY_TIMEOUT, "测试请求超时（15s）"))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("连接失败: {e}")));
    let latency = started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let ok = r.status().is_success();
            let snippet = r
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            Ok(ok_json(json!({
                "ok": ok,
                "status": status,
                "latency_ms": latency,
                "error": if ok { Value::Null } else { Value::String(snippet) },
            })))
        }
        Err(e) => Err(e),
    }
}

