use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;

use crate::error::ApiError;
use crate::llm::balancer;
use crate::llm::provider::{infer_protocol, InferError, Protocol};
use crate::llm::quota::QuotaTracker;
use crate::llm::usage::{extract_tokens, TailBuffer};
use crate::proxy::{error_response, is_filtered};
use crate::state::AppState;

/// 保留前缀：`nest` 优先于 routes fallback（ADR-L5）。
pub const LLM_PREFIX: &str = "/llm";

/// `/llm` nest 的 fallback 处理器：剥前缀 → 协议段选池 → 按协议注入凭证 →
/// 透传（流式）→ 尾包提取 usage 记 TPM。
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

    let provider = match balancer::select_and_record(&state, protocol) {
        Ok(provider) => provider,
        Err(ApiError::NoProviderForProtocol(msg)) => {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, &msg)
        }
        Err(ApiError::QuotaExhausted(msg)) => {
            return error_response(StatusCode::TOO_MANY_REQUESTS, &msg)
        }
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

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

    let req_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let upstream_request = state
        .client
        .request(req_method, &url)
        .headers(forward)
        .body(reqwest::Body::wrap_stream(
            request.into_body().into_data_stream(),
        ));

    let timeout = Duration::from_secs(state.settings.default_timeout_secs);
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

/// 响应流 tap：每块向客户端转发的同时复制进 `TailBuffer`；流正常结束 /
/// 上游流出错 / 消费者断开（Drop）三种情形都触发一次
/// `extract_tokens → record_tokens`（互斥，只记一次）。
struct UsageTap<S> {
    upstream: Pin<Box<S>>,
    protocol: Protocol,
    content_type: Option<String>,
    quota: Option<std::sync::Arc<QuotaTracker>>,
    tail: TailBuffer,
    done: bool,
}

impl<S> UsageTap<S> {
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if let Some(quota) = self.quota.take() {
            let tokens = extract_tokens(
                self.protocol,
                self.content_type.as_deref(),
                self.tail.tail(),
            );
            quota.record_tokens(tokens);
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
