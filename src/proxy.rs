use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::AppState;

/// Hop-by-hop headers must not be forwarded by a proxy; the rest of the client
/// request is passed through verbatim, including `Authorization` (mounted
/// sub-gateways do their own client auth).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The fallback handler: anything that is not `/health` or `/__admin/*` is matched
/// against the route table and proxied. Request and response bodies stream through,
/// so a sub-gateway can serve large or chunked payloads without buffering.
pub async fn proxy(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let path = uri.path().to_string();
    let Some(route) = state.routes.match_route(&path) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no route mounted for path {path}"),
        );
    };
    if !route.enabled {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("route '{}' is disabled", route.id),
        );
    }

    let stats = state.stats_for(&route.id);
    let _guard = stats.guard();

    let target_path = match route.strip_prefix {
        true => {
            let rest = &path[route.prefix.len()..];
            if rest.is_empty() || !rest.starts_with('/') {
                format!("/{rest}")
            } else {
                rest.to_string()
            }
        }
        false => path.clone(),
    };
    let mut url = format!("{}{}", route.upstream.trim_end_matches('/'), target_path);
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }

    let mut forward = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_filtered(name) || name.as_str() == "host" {
            continue;
        }
        forward.append(name.clone(), value.clone());
    }
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        if let Ok(value) = HeaderValue::from_str(host) {
            forward.insert("x-forwarded-host", value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&path) {
        forward.insert("x-forwarded-path", value);
    }
    for (name, value) in &route.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                forward.insert(name, value);
            }
            _ => {
                tracing::warn!(route = %route.id, "skipping invalid header {name}");
            }
        }
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

    let timeout = Duration::from_secs(
        route
            .timeout_secs
            .unwrap_or(state.settings.default_timeout_secs),
    );
    let upstream = match tokio::time::timeout(timeout, upstream_request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            stats.record_error();
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream '{}' unreachable: {err}", route.id),
            );
        }
        Err(_) => {
            stats.record_error();
            return error_response(
                StatusCode::GATEWAY_TIMEOUT,
                &format!(
                    "upstream '{}' timed out after {}s",
                    route.id,
                    timeout.as_secs()
                ),
            );
        }
    };

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

pub(crate) fn is_filtered(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str()) || name.as_str() == "content-length"
}

pub fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": status.as_u16(), "message": message } })),
    )
        .into_response()
}
