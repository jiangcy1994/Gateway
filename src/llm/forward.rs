use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures_util::{Stream, StreamExt};

use crate::error::ApiError;
use crate::llm::app;
use crate::llm::balancer;
use crate::llm::ledger::UsageLedger;
use crate::llm::provider::{infer_protocol, InferError, Protocol};
use crate::llm::quota::QuotaTracker;
use crate::llm::usage::{extract_tokens, extract_usage, TailBuffer};
use crate::proxy::{error_response, is_filtered};
use crate::state::AppState;

/// 保留前缀：`nest` 优先于 routes fallback（ADR-L5）。
pub const LLM_PREFIX: &str = "/llm";

/// 请求 body 的缓冲上限（ADR-L14）：为了改写 `model` 字段，请求 body 不再
/// 流式；超限直接 413——截断转发没有意义（上游只会回 400）。
pub const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;

/// `/llm` nest 的 fallback 处理器：剥前缀 → 解析 app → 缓冲 body 取 model →
/// 组合校验 → 分层选供应商 → 按需改写 model → 注入凭证 → 透传（响应流式）
/// → 尾包提取 usage：先记 TPM 配额，再记按日 × app 账本（ADR-L20）。
pub async fn forward(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let path = uri.path();
    let rest = path.strip_prefix(LLM_PREFIX).unwrap_or(path);
    let rest = rest.trim_start_matches('/');
    let (first, remainder) = match rest.split_once('/') {
        Some((seg, rem)) => (seg, rem),
        None => (rest, ""),
    };
    if first.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "missing protocol segment: use /llm/{protocol}/... with openai, anthropic or systemone",
        );
    }

    // 两种入站形态（ADR-L12）：
    //   `/llm/{protocol}/...` —— 显式协议段， remainder 原样转发；
    //   `/llm/v1/{端点}`      —— 按端点段推断协议，推断结果只影响选池与凭证注入，
    //                            转发路径保留客户端写法（`/v1/...`）。
    let (protocol, target_path) = if first.eq_ignore_ascii_case("v1") {
        let Some(endpoint) = remainder.split('/').next().filter(|s| !s.is_empty()) else {
            return error_response(
                StatusCode::NOT_FOUND,
                "missing endpoint after v1: use /llm/v1/{endpoint} (e.g. /llm/v1/chat/completions) or /llm/{protocol}/v1/...",
            );
        };
        let protocol = match infer_protocol(endpoint) {
            Ok(protocol) => protocol,
            Err(InferError::Ambiguous) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "endpoint '{endpoint}' is served by both openai and anthropic: use the explicit protocol segment /llm/{{protocol}}/v1/{endpoint}"
                    ),
                )
            }
            Err(InferError::Unknown) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "cannot infer protocol from endpoint '{endpoint}': unknown endpoint; use /llm/{{protocol}}/... (openai, anthropic or systemone)"
                    ),
                )
            }
        };
        (protocol, format!("/{first}/{remainder}"))
    } else {
        let Some(protocol) = Protocol::from_url_segment(first) else {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!(
                    "unknown protocol segment '{first}': expected openai, anthropic or systemone"
                ),
            );
        };
        let target_path = if remainder.is_empty() {
            "/".to_string()
        } else {
            format!("/{remainder}")
        };
        (protocol, target_path)
    };

    // app 解析（ADR-L15）：无凭证 = 默认策略（放行）；有凭证但不命中任何
    // app = 401 fail-closed；命中但禁用 = 403。组合校验在 model 解出之后。
    let app = match app::credential_from_headers(&headers) {
        Some(credential) => match state.apps.find_by_key(&credential) {
            Some(found) if found.enabled => Some(found),
            Some(found) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    &ApiError::AppDisabled(found.id).to_string(),
                )
            }
            None => {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    &ApiError::UnknownAppKey.to_string(),
                )
            }
        },
        None => None,
    };

    // 缓冲请求 body（上限 MAX_REQUEST_BODY）：非 JSON / 无 model 字段都算
    // 「无 model」，按协议级路由放行（盲区 R11）。
    let body = match read_request_body(request).await {
        Ok(body) => body,
        Err(BodyReadError::TooLarge) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ApiError::RequestBodyTooLarge.to_string(),
            )
        }
        Err(BodyReadError::Failed(err)) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to read request body: {err}"),
            )
        }
    };
    let (model, mut parsed) = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => {
            let model = value
                .get("model")
                .and_then(|m| m.as_str())
                .map(str::to_string);
            (model, Some(value))
        }
        Err(_) => (None, None),
    };

    // 组合校验：只有 app 与请求 model 都明确时才检查（ADR-L15/R11）。
    if let (Some(app), Some(model)) = (app.as_ref(), model.as_deref()) {
        if !app.models.is_empty() && !app.models.iter().any(|m| m == model) {
            return error_response(
                StatusCode::FORBIDDEN,
                &ApiError::ModelNotAllowedForApp(model.to_string(), app.id.clone()).to_string(),
            );
        }
    }

    let selection = match balancer::select_and_record(&state, protocol, model.as_deref()) {
        Ok(selection) => selection,
        // 其余变体（ModelNotFound → 404、NoProviderForModel → 503、
        // QuotaExhausted → 429 …）一律按 `ApiError::status` 走，两处不再各写一张表。
        Err(e) => return error_response(e.status(), &e.to_string()),
    };
    let provider = selection.provider;

    let stats = state.llm_stats_for(&provider.id);
    let _guard = stats.guard();

    let mut url = format!("{}{}", provider.base_url.trim_end_matches('/'), target_path);
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }

    // 剥离集 = hop-by-hop + content-length + host + authorization + x-api-key；
    // 客户端凭证一律替换成供应商凭证。
    let mut forward = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_filtered(name) || matches!(name.as_str(), "host" | "authorization" | "x-api-key") {
            continue;
        }
        forward.append(name.clone(), value.clone());
    }
    match protocol {
        Protocol::Openai | Protocol::Systemone => {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", provider.api_key)) {
                forward.insert("authorization", value);
            }
        }
        Protocol::Anthropic => {
            if let Ok(value) = HeaderValue::from_str(&provider.api_key) {
                forward.insert("x-api-key", value);
            }
            // 客户端已传则保留原值
            forward
                .entry("anthropic-version")
                .or_insert(HeaderValue::from_static("2023-06-01"));
        }
    }
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        if let Ok(value) = HeaderValue::from_str(host) {
            forward.insert("x-forwarded-host", value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(path) {
        forward.insert("x-forwarded-path", value);
    }

    // 改写（ADR-L14）：仅当选中条目的上游名与请求 model 不同时才重新序列化；
    // 值相同、或兜底（空模型表）供应商命中时，原始 bytes 零差异透传。
    let mut upstream_body = body;
    if let Some(upstream_model) = selection.upstream_model.as_deref() {
        if Some(upstream_model) != model.as_deref() {
            // parsed 只可能在 model 为 Some 时为 Some——逻辑上 else 分支走不到，
            // 兜底为原样转发（上游收到客户端写的 model）。
            if let Some(value) = parsed.as_mut() {
                value["model"] = serde_json::Value::String(upstream_model.to_string());
                match serde_json::to_vec(value) {
                    Ok(rewritten) => upstream_body = Bytes::from(rewritten),
                    Err(err) => {
                        return error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("failed to rewrite model field: {err}"),
                        )
                    }
                }
            }
        }
    }

    let req_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    // 按供应商选 client（ADR-L19）：无代理走共享直连 client，有代理走按代理 url
    // 复用的缓存 client。超时逻辑不变——timeout 仍包裹 `send()`。
    let client = match state
        .llm_clients
        .get(provider.proxy_url.as_deref(), &state.client)
    {
        Ok(client) => client,
        Err(e) => return error_response(e.status(), &e.to_string()),
    };
    let upstream_request = client
        .request(req_method, &url)
        .headers(forward)
        .body(upstream_body);

    // 按供应商 timeout（ADR-L17）；缺省回落全局 `GATEWAY_DEFAULT_TIMEOUT`。
    let timeout = Duration::from_secs(
        provider
            .timeout_secs
            .unwrap_or(state.settings.default_timeout_secs),
    );
    let upstream = match tokio::time::timeout(timeout, upstream_request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            stats.record_error();
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("provider '{}' unreachable: {err}", provider.id),
            );
        }
        Err(_) => {
            stats.record_error();
            return error_response(
                StatusCode::GATEWAY_TIMEOUT,
                &format!(
                    "provider '{}' timed out after {}s",
                    provider.id,
                    timeout.as_secs()
                ),
            );
        }
    };

    let status = upstream.status();
    if status.is_server_error() {
        stats.record_error();
    }

    let content_type = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let mut builder = Response::builder().status(status);
    let out_headers = builder.headers_mut().unwrap();
    for (name, value) in upstream.headers() {
        if is_filtered(name) || name.as_str() == "content-length" {
            continue;
        }
        out_headers.append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&provider.id) {
        out_headers.insert("x-llm-provider", value);
    }

    let quota = state.quota_for(&provider.id);
    let tap = UsageTap {
        upstream: Box::pin(upstream.bytes_stream()),
        protocol,
        content_type,
        quota: Some(quota),
        ledger: Some(state.llm_usage.clone()),
        app_id: app.as_ref().map(|app| app.id.clone()),
        tail: TailBuffer::new(),
        done: false,
    };
    builder.body(Body::from_stream(tap)).unwrap_or_else(|_| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build response",
        )
        .into_response()
    })
}

/// `read_request_body` 的失败区分：超上限 → 413；其他读错误 → 500。
enum BodyReadError {
    TooLarge,
    Failed(axum::Error),
}

/// 把请求 body 全量读进 `Bytes`，超过 [`MAX_REQUEST_BODY`] 即停止并报
/// `TooLarge`（不截断、不转发——ADR-L14）。无 body 的请求（GET 等）得到
/// 空的 `Bytes`，随后的 model 解析自然回落到「无 model」。
async fn read_request_body(request: Request) -> Result<Bytes, BodyReadError> {
    let mut stream = request.into_body().into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyReadError::Failed)?;
        if buf.len() + chunk.len() > MAX_REQUEST_BODY {
            return Err(BodyReadError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// 响应流 tap：每块向客户端转发的同时复制进 `TailBuffer`；流正常结束 /
/// 上游流出错 / 消费者断开（Drop）三种情形都互斥地触发一次记账（`done`
/// 保证只记一次）：先 `extract_tokens → record_tokens`（TPM 配额），再
/// `extract_usage → ledger.record(app_id, 当日 UTC, 分桶)`（ADR-L20）。
struct UsageTap<S> {
    upstream: Pin<Box<S>>,
    protocol: Protocol,
    content_type: Option<String>,
    quota: Option<std::sync::Arc<QuotaTracker>>,
    ledger: Option<UsageLedger>,
    app_id: Option<String>,
    tail: TailBuffer,
    done: bool,
}

impl<S> UsageTap<S> {
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let tail = self.tail.tail();
        if let Some(quota) = self.quota.take() {
            let tokens = extract_tokens(self.protocol, self.content_type.as_deref(), tail);
            quota.record_tokens(tokens);
        }
        // 账本路径与配额路径完全独立（ADR-L20）：`record` 不可失败，也不持
        // 任何 `QuotaTracker` 的锁——两路径 worst case 互不影响。
        if let Some(ledger) = self.ledger.take() {
            let usage = extract_usage(self.protocol, self.content_type.as_deref(), tail);
            ledger.record(self.app_id.as_deref(), Utc::now().date_naive(), usage);
        }
    }
}

impl<S> Drop for UsageTap<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<S, E> Stream for UsageTap<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.upstream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.tail.push(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finish();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                self.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
