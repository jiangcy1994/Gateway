use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::proxy::{error_response, is_filtered};
use crate::state::AppState;

/// `/qqbot` nest 的 fallback 处理器：`/qqbot/{app_id}/...` 透传到平台
/// OpenAPI，注入 `Authorization: QQBot <access_token>`。body 原样字节流式
/// 转发，无缓冲、无 usage tap（QQ Bot 的 OpenAPI 无 usage 语义，与 LLM
/// 转发不同）。
pub async fn forward(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let path = uri.path();

    // 1. 剥掉 /qqbot 前缀，取 app_id 段与其后的剩余路径。
    let rest = path.strip_prefix("/qqbot").unwrap_or(path);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let (app_id, mut target_path) = match rest.split_once('/') {
        Some((app_id, tail)) => (app_id, format!("/{tail}")),
        None => (rest, String::new()),
    };
    if app_id.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "no QQ bot for path: missing app_id segment after /qqbot",
        );
    }
    if target_path.is_empty() {
        target_path.push('/');
    }

    // 2. 查 bot：未找到 404，禁用 503（与路由的禁用语义一致）。
    let Some(bot) = state.bots.find(app_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no QQ bot registered for app_id {app_id}"),
        );
    };
    if !bot.enabled {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("QQ bot '{}' (app_id {}) is disabled", bot.id, bot.app_id),
        );
    }

    // 3. 取 token：失败 502。
    let timeout = Duration::from_secs(state.settings.default_timeout_secs);
    let token = match state
        .bot_tokens
        .get_or_fetch(&state.client, &bot, timeout)
        .await
    {
        Ok(token) => token,
        Err(err) => {
            state.bot_stats_for(&bot.id).record_error();
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "failed to obtain access token for bot '{}' (app_id {}): {err}",
                    bot.id, bot.app_id
                ),
            );
        }
    };

    // 4. 计费 guard（Drop 记账）。
    let stats = state.bot_stats_for(&bot.id);
    let _guard = stats.guard();

    // 5. 上游 URL。
    let mut url = format!(
        "{}{target_path}",
        bot.resolve_api_base().trim_end_matches('/')
    );
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }

    // 6/7. 请求头：剥离集 + 注入 QQBot 鉴权 + x-forwarded-*。
    let mut forward_headers = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_filtered(name) || name.as_str() == "host" || name.as_str() == "authorization" {
            continue;
        }
        forward_headers.append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&format!("QQBot {token}")) {
        forward_headers.insert("authorization", value);
    }
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        if let Ok(value) = HeaderValue::from_str(host) {
            forward_headers.insert("x-forwarded-host", value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(path) {
        forward_headers.insert("x-forwarded-path", value);
    }

    let req_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let upstream_request = state
        .client
        .request(req_method, &url)
        .headers(forward_headers)
        .body(reqwest::Body::wrap_stream(
            request.into_body().into_data_stream(),
        ));

    // 8. 超时包裹：传输错误 502、超时 504，均计错误。
    let upstream = match tokio::time::timeout(timeout, upstream_request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            stats.record_error();
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("QQ bot '{}' upstream unreachable: {err}", bot.id),
            );
        }
        Err(_) => {
            stats.record_error();
            return error_response(
                StatusCode::GATEWAY_TIMEOUT,
                &format!(
                    "QQ bot '{}' upstream timed out after {}s",
                    bot.id,
                    timeout.as_secs()
                ),
            );
        }
    };

    // 9/10/11. 响应头剥离 + 注入 bot id；body 流式；5xx 透传并计错误。
    let status = upstream.status();
    if status.is_server_error() {
        stats.record_error();
    }

    let mut builder = Response::builder().status(status);
    let out_headers = builder.headers_mut().unwrap();
    for (name, value) in upstream.headers() {
        if is_filtered(name) || name.as_str() == "content-length" {
            continue;
        }
        out_headers.append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&bot.id) {
        out_headers.insert("x-qqbot-bot", value);
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response",
            )
            .into_response()
        })
}
