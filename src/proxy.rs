use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::json;

use crate::config::{upstream_url, Protocol, Provider};
use crate::db;
use crate::route;
use crate::state::AppState;

const ANTHROPIC_VERSION_DEFAULT: &str = "2023-06-01";
/// 上游响应回传给客户端的头（逐块透传时不破坏 SSE 语义）。
const PASSTHROUGH_RESP_HEADERS: &[&str] = &[
    "content-type",
    "x-request-id",
    "request-id",
    "retry-after",
    "anthropic-version",
    "openai-version",
];

/// 429/5xx 且还有下一候选时自动切换备用；其余 4xx 属于请求本身的问题，直接回传。
fn retryable(status: u16) -> bool {
    status == 429 || status >= 500
}

pub async fn forward(
    st: State<Arc<AppState>>,
    protocol: Protocol,
    req: Request<Body>,
) -> Response {
    let started = Instant::now();
    let cfg = st.config();
    let path = protocol.path().to_string();

    let (parts, body) = req.into_parts();
    // 客户端 UA：HeaderValue 用于转发上游；日志里存纯文本（未提供时空串）
    let inbound_ua = parts.headers.get("user-agent").cloned();
    let ua_log = inbound_ua
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 网关自身拒绝的请求同样记入日志，保证统计与曲线反映全部流量
    macro_rules! reject {
        ($status:expr, $model:expr, $msg:expr) => {{
            let status: StatusCode = $status;
            let msg: String = $msg;
            st.db.insert_log(&db::ReqLog {
                ts: chrono::Local::now(),
                protocol: protocol.as_str().into(),
                path: path.clone(),
                model: $model,
                provider: String::new(),
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis() as u64,
                stream: false,
                attempts: vec![],
                error: Some(msg.clone()),
                tokens_in: 0,
                tokens_out: 0,
                tokens_cached: 0,
                user_agent: ua_log.clone(),
            });
            return gateway_error_json(status, &msg);
        }};
    }

    let limit = (cfg.max_body_mb.saturating_mul(1024 * 1024)) as usize;
    let payload = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(e) => {
            reject!(
                StatusCode::PAYLOAD_TOO_LARGE,
                String::new(),
                format!("请求体超过上限 {}MB：{e}", cfg.max_body_mb)
            )
        }
    };

    // 解析请求体：读取 model 做选址，并按设置剥离上游不支持的参数
    let mut body_json: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(v) => v,
        Err(_) => {
            reject!(StatusCode::BAD_REQUEST, String::new(), "请求体不是合法 JSON".into())
        }
    };
    let Some(model) = body_json.get("model").and_then(|m| m.as_str()).map(str::to_string) else {
        reject!(
            StatusCode::BAD_REQUEST,
            String::new(),
            "请求体缺少 model 字段".into()
        );
    };

    // 全局模型别名（最外层）：先把请求模型转换为真实模型
    let real_model = route::apply_aliases(&cfg.aliases, &model);

    // 参数剥离（drop_params，如 reasoning_effort），避免上游直接报 UnsupportedParams
    let drop_list: Vec<String> = cfg
        .drop_params
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if !drop_list.is_empty() {
        if let Some(obj) = body_json.as_object_mut() {
            for k in &drop_list {
                obj.remove(k);
            }
        }
    }

    // 上游应收到真实模型名：改写请求体 model 字段（一次），其余内容原样
    if real_model != model {
        body_json["model"] = serde_json::json!(real_model);
    }
    // OpenAI chat 协议流式默认不返回 usage，主动要求上游附带，
    // 否则日志 token 统计全 0；客户端已带 stream_options 时不覆盖。
    if matches!(protocol, Protocol::Chat)
        && body_json
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        if let Some(obj) = body_json.as_object_mut() {
            let so = obj
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(so_obj) = so.as_object_mut() {
                so_obj
                    .entry("include_usage")
                    .or_insert(serde_json::json!(true));
            }
        }
    }
    let payload: Bytes = match serde_json::to_vec(&body_json) {
        Ok(v) => Bytes::from(v),
        Err(_) => payload,
    };

    // 健康缓存键（协议:真实模型）：先用它查上次成功的供应商
    let cache_key = format!("{}:{}", protocol.as_str(), real_model);
    let preferred = st.healthy_get(&cache_key);
    let resolved = match route::resolve(&cfg, protocol, &real_model, preferred.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            let (status, kind) = e.status();
            reject!(status, real_model.clone(), format!("{kind}：{e}"));
        }
    };
    // 以解析结果为唯一事实：后续记忆/日志统一使用解析产出的缓存键
    let cache_key = &resolved.key;
    // 候选已过滤停用与协议不匹配，且主选必然在列

    let inbound_ct = parts
        .headers
        .get("content-type")
        .cloned()
        .unwrap_or(HeaderValue::from_static("application/json"));
    let inbound_accept = parts.headers.get("accept").cloned();
    let anthropic_version = parts
        .headers
        .get("anthropic-version")
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static(ANTHROPIC_VERSION_DEFAULT));

    let mut attempts: Vec<String> = Vec::new();
    let mut last_error = String::new();

    for (i, cand) in resolved.candidates.iter().enumerate() {
        let provider = cand.provider;
        let more = i + 1 < resolved.candidates.len();
        match try_upstream(
            &st,
            provider,
            protocol,
            &payload,
            (&inbound_ct, inbound_accept.as_ref(), inbound_ua.as_ref(), &anthropic_version),
        )
        .await
        {
            Ok(up) => {
                let status = up.status().as_u16();
                attempts.push(format!("{}:{}", provider.name, status));
                return finish_response(
                    &st, up, status, &attempts, &provider.name, protocol, &path, &real_model,
                    &cache_key, started, inbound_accept.as_ref(), ua_log.clone(),
                )
                .await;
            }
            Err(UpstreamFail::Status { status, body, content_type }) => {
                attempts.push(format!("{}:{}", provider.name, status));
                last_error = format!("上游 {} 返回 {status}", provider.name);
                if retryable(status) && more {
                    tracing::info!("上游 {} 返回 {status}，切换备用（模型 {model}）", provider.name);
                    continue;
                }
                st.db.insert_log(&db::ReqLog {
                    ts: chrono::Local::now(),
                    protocol: protocol.as_str().into(),
                    path: path.clone(),
                    model: real_model.clone(),
                    provider: provider.name.clone(),
                    status,
                    latency_ms: started.elapsed().as_millis() as u64,
                    stream: is_stream_accept(inbound_accept.as_ref()),
                    attempts: attempts.clone(),
                    error: Some(last_error.clone()),
                    tokens_in: 0,
                    tokens_out: 0,
                    tokens_cached: 0,
                    user_agent: ua_log.clone(),
                });
                return error_response(status, body, content_type);
            }
            Err(UpstreamFail::Transport(msg)) => {
                attempts.push(format!("{}:err", provider.name));
                last_error = format!("上游 {} 连接失败: {msg}", provider.name);
                tracing::warn!("{last_error}（模型 {model}）");
                if more {
                    continue;
                }
            }
        }
    }

    st.db.insert_log(&db::ReqLog {
        ts: chrono::Local::now(),
        protocol: protocol.as_str().into(),
        path,
        model: real_model,
        provider: String::new(),
        status: 502,
        latency_ms: started.elapsed().as_millis() as u64,
        stream: is_stream_accept(inbound_accept.as_ref()),
        attempts,
        error: Some(last_error.clone()),
        tokens_in: 0,
        tokens_out: 0,
        tokens_cached: 0,
        user_agent: ua_log.clone(),
    });
    gateway_error_json(
        StatusCode::BAD_GATEWAY,
        &format!("所有候选供应商均失败：{last_error}"),
    )
}

enum UpstreamFail {
    Status {
        status: u16,
        body: Bytes,
        content_type: Option<HeaderValue>,
    },
    Transport(String),
}

/// 流式日志守卫：客户端中途断开导致 body 被 drop、finisher 未执行时，
/// 在 Drop 中补写日志，避免整条请求记录（含已扫到的 token）丢失。
struct StreamLogGuard {
    st: Arc<AppState>,
    usage: Arc<std::sync::Mutex<Usage>>,
    protocol: &'static str,
    path: String,
    model: String,
    provider: String,
    status: u16,
    attempts: Vec<String>,
    started: Instant,
    user_agent: String,
    done: Arc<AtomicBool>,
}

impl Drop for StreamLogGuard {
    fn drop(&mut self) {
        if self.done.load(Ordering::Relaxed) {
            return;
        }
        let u = *self.usage.lock().unwrap();
        self.st.db.insert_log(&db::ReqLog {
            ts: chrono::Local::now(),
            protocol: self.protocol.into(),
            path: self.path.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            status: self.status,
            latency_ms: self.started.elapsed().as_millis() as u64,
            stream: true,
            attempts: self.attempts.clone(),
            error: Some("客户端中断流".into()),
            tokens_in: u[0],
            tokens_out: u[1],
            tokens_cached: u[2],
            user_agent: self.user_agent.clone(),
        });
    }
}

/// 成功拿到上游响应头后的收尾：非流式缓冲解析 usage，流式边透传边扫描。
#[allow(clippy::too_many_arguments)]
async fn finish_response(
    st: &Arc<AppState>,
    up: reqwest::Response,
    status: u16,
    attempts: &[String],
    provider_name: &str,
    protocol: Protocol,
    path: &str,
    model: &str,
    cache_key: &str,
    started: Instant,
    _accept: Option<&HeaderValue>,
    ua: String,
) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let ct = up.headers().get("content-type").cloned();
    let is_sse = ct
        .as_ref()
        .map(|v| v.to_str().unwrap_or("").to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false);

    // 收集需要回传的响应头
    let mut headers = HeaderMap::new();
    for h in PASSTHROUGH_RESP_HEADERS {
        if let Some(v) = up.headers().get(*h) {
            headers.insert(*h, v.clone());
        }
    }

    // 记住这次成功的供应商，后续同模型请求优先走它
    st.healthy_set(cache_key, provider_name);

    if is_sse {
        // 流式：逐块透传，同时扫描 SSE 文本中的 usage；流结束时写日志
        let usage: Arc<std::sync::Mutex<Usage>> = Arc::new(std::sync::Mutex::new([0; 3]));
        let usage_scan = usage.clone();
        let buf: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let buf_scan = buf.clone();
        let scanned = up.bytes_stream().map(move |r| {
            if let Ok(bytes) = &r {
                if let Ok(mut b) = buf_scan.lock() {
                    if let Ok(mut u) = usage_scan.lock() {
                        // 直接扫字节：多字节字符跨 chunk 被切断也不会丢整个 chunk
                        scan_usage(&mut b, bytes, &mut u);
                    }
                }
            }
            r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });

        let usage_done = usage;
        let st2 = st.clone();
        let model = model.to_string();
        let provider_name = provider_name.to_string();
        let path = path.to_string();
        let attempts = attempts.to_vec();
        let protocol_str = protocol.as_str().to_string();
        // 流结束时补写日志（带完整 token 用量）；客户端断开时由守卫兑底补写
        let guard = StreamLogGuard {
            st: st2.clone(),
            usage: usage_done.clone(),
            protocol: protocol.as_str(),
            path: path.clone(),
            model: model.clone(),
            provider: provider_name.clone(),
            status,
            attempts: attempts.clone(),
            started,
            user_agent: ua.clone(),
            done: Arc::new(AtomicBool::new(false)),
        };
        let finisher = futures::stream::once(async move {
            let u = *usage_done.lock().unwrap();
            st2.db.insert_log(&db::ReqLog {
                ts: chrono::Local::now(),
                protocol: protocol_str,
                path,
                model,
                provider: provider_name,
                status,
                latency_ms: started.elapsed().as_millis() as u64,
                stream: true,
                attempts,
                error: None,
                tokens_in: u[0],
                tokens_out: u[1],
                tokens_cached: u[2],
                user_agent: ua,
            });
            guard.done.store(true, Ordering::Relaxed); // 正常走完，无需守卫兑底
            Ok(axum::body::Bytes::new())
        });
        let body = Body::from_stream(futures::StreamExt::chain(scanned, finisher));
        response_with(status_code, Some(headers), body)
    } else {
        // 非流式：整体缓冲后解析 usage，再原样回传
        let bytes = up.bytes().await.unwrap_or_default();
        let usage = usage_from_json(&bytes);
        st.db.insert_log(&db::ReqLog {
            ts: chrono::Local::now(),
            protocol: protocol.as_str().into(),
            path: path.to_string(),
            model: model.to_string(),
            provider: provider_name.to_string(),
            status,
            latency_ms: started.elapsed().as_millis() as u64,
            stream: false,
            attempts: attempts.to_vec(),
            error: None,
            tokens_in: usage[0],
            tokens_out: usage[1],
            tokens_cached: usage[2],
            user_agent: ua,
        });
        let mut hm = HeaderMap::new();
        hm.extend(headers.drain());
        response_with(
            status_code,
            Some(hm),
            Body::from(bytes),
        )
    }
}

fn response_with(status: StatusCode, headers: Option<HeaderMap>, body: Body) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(hm) = &headers {
        for (k, v) in hm.iter() {
            builder = builder.header(k, v);
        }
    }
    builder
        .body(body)
        .unwrap_or_else(|_| gateway_error_json(StatusCode::BAD_GATEWAY, "响应构建失败"))
}

fn is_stream_accept(accept: Option<&HeaderValue>) -> bool {
    accept
        .map(|v| v.to_str().unwrap_or("").to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

// ---------- Token 用量解析 ----------

type Usage = [u64; 3]; // [输入, 输出, 缓存]

/// 合法 usage 对象至少包含这些键之一；防止流式文本中的其它 "usage": {...}
/// （如模型输出内容、上游扩展字段）被误当真实用量合并。
const USAGE_KEYS: &[&str] = &[
    "prompt_tokens",
    "input_tokens",
    "completion_tokens",
    "output_tokens",
    "cached_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
];

fn merge_usage(usage: &mut Usage, v: &serde_json::Value) {
    if !USAGE_KEYS.iter().any(|k| v.get(*k).is_some()) {
        return; // 不是真实的 usage 对象，忽略
    }
    let num = |o: &serde_json::Value, keys: &[&str]| -> u64 {
        keys.iter()
            .filter_map(|k| o.get(*k))
            .filter_map(|x| x.as_u64())
            .max()
            .unwrap_or(0)
    };
    // 输入口径：OpenAI 的 prompt_tokens 已含缓存命中；Anthropic 的 input_tokens
    // 不含缓存部分（cache_read / cache_creation 单列）。为使两种协议的 tokens_in
    // 都等于全部输入消耗，Anthropic 口径将缓存部分并入输入。
    let input = if v.get("input_tokens").is_some() {
        num(v, &["input_tokens"])
            + num(v, &["cache_read_input_tokens"])
            + num(v, &["cache_creation_input_tokens"])
    } else {
        num(v, &["prompt_tokens"])
    };
    usage[0] = usage[0].max(input);
    // 输出口径：OpenAI 的 completion_tokens 已含推理 token（reasoning 是其子集，
    // completion ≥ reasoning 恒成立）。若 completion < reasoning，说明上游（LiteLLM
    // 类网关）把推理单列、completion 只数可见内容，此时两者相加才是总输出。
    let completion = num(v, &["completion_tokens", "output_tokens"]);
    let reasoning = v
        .get("completion_tokens_details")
        .map(|d| num(d, &["reasoning_tokens"]))
        .unwrap_or(0);
    let output = if completion >= reasoning {
        completion
    } else {
        completion + reasoning
    };
    usage[1] = usage[1].max(output);
    // 缓存口径：OpenAI 的 cached_tokens（含于输入）/ Anthropic 的
    // cache_read + cache_creation。上游未上报时为 0。
    let mut cached = num(v, &["cached_tokens", "cache_read_input_tokens"]);
    if let Some(d) = v.get("prompt_tokens_details") {
        cached = cached.max(num(d, &["cached_tokens"]));
    }
    cached += num(v, &["cache_creation_input_tokens"]);
    usage[2] = usage[2].max(cached);
}

/// 非流式 JSON 响应的 usage 提取。
fn usage_from_json(bytes: &Bytes) -> Usage {
    let mut usage = [0, 0, 0];
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Some(u) = v.get("usage") {
            merge_usage(&mut usage, u);
        }
    }
    usage
}

/// 在累积字节流中查找 "usage" 对象（跨 chunk 安全，含 UTF-8 多字节字符跨 chunk），
/// 合并用量并裁剪已消费部分。
fn scan_usage(buf: &mut Vec<u8>, chunk: &[u8], usage: &mut Usage) {
    buf.extend_from_slice(chunk);
    // 只处理完整 UTF-8 前缀；被切断的多字节字符尾部留待下一个 chunk，不丢数据
    let valid_up_to = match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(e) => e.valid_up_to(),
    };
    let s = std::str::from_utf8(&buf[..valid_up_to]).unwrap_or("");
    let mut search_from = 0usize;
    loop {
        match s[search_from..].find("\"usage\"") {
            None => {
                // 没有更多 usage：仅保留尾部，防止长流内存膨胀（"usage" 最长 7 字节，留 16 兑底）
                let keep_from = s.len().saturating_sub(16);
                let cut = (0..=keep_from)
                    .rev()
                    .find(|&i| s.is_char_boundary(i))
                    .unwrap_or(0);
                buf.drain(..cut);
                return;
            }
            Some(rel) => {
                let hit = search_from + rel;
                let Some(brace_rel) = s[hit..].find("{") else { break }; // 对象未到齐，等下一个 chunk
                let start = hit + brace_rel;
                let Some(end_rel) = json_object_end(&s[start..]) else { break }; // 同上
                let end = start + end_rel;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s[start..=end]) {
                    merge_usage(usage, &v);
                }
                search_from = end + 1;
            }
        }
    }
    // 对象未到齐：仅裁剪已消费部分，保留未完成区域等待后续 chunk
    buf.drain(..search_from);
}

/// 返回字符串中第一个平衡的 `{...}` 对象的结束下标（含），未闭合返回 None。
fn json_object_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

async fn try_upstream(
    st: &Arc<AppState>,
    provider: &Provider,
    protocol: Protocol,
    payload: &[u8],
    headers: (&HeaderValue, Option<&HeaderValue>, Option<&HeaderValue>, &HeaderValue),
) -> Result<reqwest::Response, UpstreamFail> {
    let (content_type, accept, user_agent, anthropic_version) = headers;
    let url = upstream_url(provider, protocol);
    let mut rb = st
        .client
        .post(&url)
        .header("content-type", content_type);
    if let Some(a) = accept {
        rb = rb.header("accept", a);
    }
    if let Some(ua) = user_agent {
        rb = rb.header("user-agent", ua);
    }
    match protocol {
        Protocol::Anthropic => {
            if !provider.api_key.is_empty() {
                rb = rb.header("x-api-key", &provider.api_key);
            }
            rb = rb.header("anthropic-version", anthropic_version);
        }
        Protocol::Chat | Protocol::Responses => {
            if !provider.api_key.is_empty() {
                rb = rb.header("authorization", format!("Bearer {}", provider.api_key));
            }
        }
    }
    for (k, v) in &provider.extra_headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    rb = rb.body(payload.to_vec());

    // 超时只约束「等待响应头」阶段（近似首字延迟），流式传输阶段不设上限
    let exec = st.client.execute(rb.build().map_err(|e| UpstreamFail::Transport(e.to_string()))?);
    let resp = tokio::time::timeout(Duration::from_secs(provider.timeout_secs), exec)
        .await
        .map_err(|_| UpstreamFail::Transport(format!("等待响应超时（{}s）", provider.timeout_secs)))?
        .map_err(|e| UpstreamFail::Transport(transport_msg(e)))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let ct = resp.headers().get("content-type").cloned();
        let body = resp.bytes().await.unwrap_or_default();
        return Err(UpstreamFail::Status { status, body, content_type: ct });
    }
    Ok(resp)
}

fn transport_msg(e: reqwest::Error) -> String {
    if e.is_connect() {
        format!("无法连接（{e}）")
    } else if e.is_timeout() {
        format!("请求超时（{e}）")
    } else {
        e.to_string()
    }
}

/// 上游返回的 4xx/5xx 错误体按原样回传（换状态行即可），客户端 SDK 正常解析。
fn error_response(status: u16, body: Bytes, content_type: Option<HeaderValue>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    Response::builder()
        .status(status)
        .header(
            "content-type",
            content_type.unwrap_or(HeaderValue::from_static("application/json")),
        )
        .body(Body::from(body))
        .unwrap_or_else(|_| gateway_error_json(StatusCode::BAD_GATEWAY, "响应构建失败"))
}

/// 网关自身产生的错误统一走 OpenAI 风格 JSON，客户端 SDK 能直接显示 message。
pub fn gateway_error_json(status: StatusCode, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "error": {
                "message": msg,
                "type": "gateway_error",
                "code": status.as_u16(),
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_openai_json() {
        let body = br#"{"id":"x","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":64}}}"#;
        let u = usage_from_json(&Bytes::from_static(body));
        assert_eq!(u, [120, 30, 64]);
    }

    #[test]
    fn usage_from_anthropic_json() {
        let body = br#"{"usage":{"input_tokens":2095,"output_tokens":503,"cache_creation_input_tokens":2085,"cache_read_input_tokens":100}}"#;
        let u = usage_from_json(&Bytes::from_static(body));
        // tokens_in = input_tokens + cache_read + cache_creation（与 OpenAI 口径对齐）
        // 缓存 = cache_read + cache_creation
        assert_eq!(u, [4280, 503, 2185]);
    }

    #[test]
    fn usage_ignores_object_without_known_keys() {
        // 模型输出文本中恰好有 "usage": {...} 但不含已知键，不应计入
        let mut usage = [0u64; 3];
        merge_usage(&mut usage, &serde_json::json!({"count": 999, "total": 1}));
        assert_eq!(usage, [0, 0, 0]);
        merge_usage(&mut usage, &serde_json::json!({"prompt_tokens": 5}));
        assert_eq!(usage, [5, 0, 0]);
    }

    #[test]
    fn usage_litellm_reasoning_style() {
        // LiteLLM 转发推理模型：completion_tokens=0，实际生成量在 reasoning_tokens
        let body = br#"{"usage":{"completion_tokens":0,"prompt_tokens":16,"total_tokens":16,"completion_tokens_details":{"reasoning_tokens":30}}}"#;
        let u = usage_from_json(&Bytes::from_static(body));
        assert_eq!(u, [16, 30, 0]);
    }

    #[test]
    fn usage_reasoning_reported_disjoint_from_completion() {
        // 上游把推理单列（completion 只数可见内容）：completion < reasoning，
        // 违反子集语义（completion ≥ reasoning），总输出应为两者之和
        let body = br#"{"usage":{"prompt_tokens":18,"completion_tokens":9,"total_tokens":27,"completion_tokens_details":{"reasoning_tokens":80}}}"#;
        let u = usage_from_json(&Bytes::from_static(body));
        assert_eq!(u, [18, 89, 0]);
    }

    #[test]
    fn usage_reasoning_included_in_completion() {
        // 标准 OpenAI 口径：completion 已含推理，直接取 completion 不重复相加
        let body = br#"{"usage":{"prompt_tokens":20,"completion_tokens":50,"completion_tokens_details":{"reasoning_tokens":49}}}"#;
        let u = usage_from_json(&Bytes::from_static(body));
        assert_eq!(u, [20, 50, 0]);
    }

    #[test]
    fn usage_without_usage_key() {
        let u = usage_from_json(&Bytes::from_static(br#"{"ok":true}"#));
        assert_eq!(u, [0, 0, 0]);
    }

    #[test]
    fn scan_usage_across_chunk_boundary() {
        let mut buf = Vec::new();
        let mut usage = [0u64; 3];
        scan_usage(&mut buf, b"data: {\"usage\": {\"prompt_tok", &mut usage);
        assert_eq!(usage, [0, 0, 0]); // 对象未到齐
        scan_usage(&mut buf, b"ens\": 7, \"completion_tokens\": 3}}\n\n", &mut usage);
        assert_eq!(usage, [7, 3, 0]);
    }

    #[test]
    fn scan_usage_survives_multibyte_split_across_chunks() {
        // 中文多字节字符在 chunk 边界被切断，usage 不应丢失
        let mut buf = Vec::new();
        let mut usage = [0u64; 3];
        let t = "你好".as_bytes(); // "你" 共 3 字节，从中间切开
        scan_usage(&mut buf, b"data: {\"delta\":{\"text\":\"", &mut usage);
        scan_usage(&mut buf, &t[..2], &mut usage);
        scan_usage(&mut buf, &t[2..], &mut usage);
        scan_usage(&mut buf, b"\"},\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n", &mut usage);
        assert_eq!(usage, [7, 3, 0]);
    }

    #[test]
    fn scan_usage_anthropic_message_start_and_delta() {
        let mut buf = Vec::new();
        let mut usage = [0u64; 3];
        scan_usage(
            &mut buf,
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2095,\"cache_read_input_tokens\":100}}}\n\n",
            &mut usage,
        );
        scan_usage(
            &mut buf,
            b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":503}}\n\n",
            &mut usage,
        );
        // tokens_in = input_tokens + cache_read_input_tokens
        assert_eq!(usage, [2195, 503, 100]);
    }

    #[test]
    fn scan_usage_ignores_usage_word_in_content() {
        let mut buf = Vec::new();
        let mut usage = [0u64; 3];
        // 内容里出现 "usage" 字样但没有 JSON 对象，不应 panic 也不应误配
        scan_usage(&mut buf, b"data: {\"delta\":{\"text\":\"my usage report\"}}\n\n", &mut usage);
        assert_eq!(usage, [0, 0, 0]);
    }
}
