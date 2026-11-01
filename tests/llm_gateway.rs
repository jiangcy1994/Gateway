//! LLM 子网关集成测试。
//!
//! 每条测试对应 `docs/sdd/llm/interfaces.md` 的某条契约，映射见 `docs/tdd/mapping.md`。
//! 测试用 axum 起一个 mock 上游，在同一进程内用真实网关上 ephemeral 端口。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;
use top_gateway::build_app;
use top_gateway::config::{self, Settings};
use top_gateway::llm::provider::{Protocol, Provider};
use top_gateway::state::AppState;

const ADMIN_KEY: &str = "test-admin-key";

// ---------------------------------------------------------------- harness

struct LlmGateway {
    addr: SocketAddr,
    _dir: TempDir,
    providers_path: PathBuf,
}

fn provider(id: &str, protocol: Protocol, base_url: &str) -> Provider {
    Provider {
        id: id.to_string(),
        name: id.to_string(),
        protocol,
        base_url: base_url.to_string(),
        api_key: format!("key-{id}"),
        rpm_limit: None,
        tpm_limit: None,
        enabled: true,
    }
}

fn llm_settings(routes_path: PathBuf, providers_path: PathBuf) -> Settings {
    Settings {
        bind: "127.0.0.1:0".to_string(),
        admin_key: Some(ADMIN_KEY.to_string()),
        routes_file: routes_path,
        providers_file: providers_path,
        health_interval_secs: 600,
        health_timeout_secs: 2,
        default_timeout_secs: 15,
    }
}

async fn spawn_llm_gateway(providers: Vec<Provider>) -> LlmGateway {
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    let providers_path = dir.path().join("providers.toml");
    config::save_routes(&routes_path, &[]).unwrap();
    config::save_providers(&providers_path, &providers).unwrap();
    let settings = llm_settings(routes_path, providers_path.clone());
    let state = AppState::new(settings).unwrap();
    state
        .routes
        .set_all(config::load_routes(state.settings.routes_file.as_path()).unwrap());
    state
        .providers
        .set_all(config::load_providers(state.settings.providers_file.as_path()).unwrap());
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    LlmGateway {
        addr,
        _dir: dir,
        providers_path,
    }
}

fn gateway_url(gw: &LlmGateway, path: &str) -> String {
    format!("http://{}{}", gw.addr, path)
}

async fn admin_get(gw: &LlmGateway, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(gateway_url(gw, path))
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
}

fn authed() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

/// 网关返回的 house 错误体：`{"error":{"code":..,"message":..}}`
async fn error_body(resp: reqwest::Response) -> Value {
    let v: Value = resp.json().await.unwrap();
    v.get("error").cloned().unwrap_or(v)
}

fn header_echo(headers: &HeaderMap) -> Value {
    let map: HashMap<String, String> = headers
        .iter()
        .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    serde_json::to_value(map).unwrap()
}

// ---------------------------------------------------------------- mock 上游

async fn spawn_mock_llm() -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .fallback(any(echo));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// openai：`model == "plain"` 时不带 usage（R1 场景）；`stream == true` 时返 SSE。
async fn chat_completions(
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    if req.get("stream") == Some(&json!(true)) {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
                   data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], sse.to_string()).into_response();
    }
    let mut resp = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "path": uri.path(),
        "echo": header_echo(&headers),
    });
    if req.get("model").and_then(|m| m.as_str()) != Some("plain") {
        resp["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
    }
    Json(resp).into_response()
}

/// anthropic：非流式单 usage；`stream == true` 时返 message_start/message_delta SSE。
async fn messages(headers: HeaderMap, OriginalUri(uri): OriginalUri, body: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    if req.get("stream") == Some(&json!(true)) {
        let sse = "event: message_start\n\
                   data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
                   event: message_delta\n\
                   data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n\
                   data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], sse.to_string()).into_response();
    }
    Json(json!({
        "id": "msg_test",
        "type": "message",
        "path": uri.path(),
        "echo": header_echo(&headers),
        "usage": {"input_tokens": 3, "output_tokens": 7},
    }))
    .into_response()
}

/// 兜底：原样回显请求，并带一个 usage 字段（用于验证 systemone 桩对 usage 视而不见）。
async fn echo(method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    Json(json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query().unwrap_or(""),
        "echo": header_echo(&headers),
        "usage": {"input_tokens": 50, "output_tokens": 50},
    }))
    .into_response()
}

async fn quota_view(gw: &LlmGateway, id: &str) -> Value {
    let v: Value = admin_get(gw, "/__admin/llm/quota")
        .await
        .json()
        .await
        .unwrap();
    v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap()
}

async fn metrics_view(gw: &LlmGateway, id: &str) -> Value {
    let v: Value = admin_get(gw, "/__admin/llm/metrics")
        .await
        .json()
        .await
        .unwrap();
    v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"]["id"] == id)
        .cloned()
        .unwrap()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn llm_forwards_openai_path_and_bearer_key() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .header("authorization", "Bearer client-key")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "openai-official");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/v1/chat/completions");
    assert_eq!(v["usage"]["total_tokens"], 15);
    // 客户端凭证被替换成供应商凭证
    assert_eq!(v["echo"]["authorization"], "Bearer key-openai-official");
}

#[tokio::test]
async fn llm_forwards_anthropic_key_and_version() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "anthropic-official",
        Protocol::Anthropic,
        &upstream,
    )])
    .await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/anthropic/v1/messages"))
        .json(&json!({"model": "claude-3-5-sonnet", "max_tokens": 16, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["echo"]["x-api-key"], "key-anthropic-official");
    assert_eq!(v["echo"]["anthropic-version"], "2023-06-01");

    // 客户端自带 anthropic-version 时保留原值
    let resp = authed()
        .post(gateway_url(&gw, "/llm/anthropic/v1/messages"))
        .header("anthropic-version", "2023-01-01")
        .json(&json!({"model": "claude-3-5-sonnet", "max_tokens": 16, "messages": []}))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["echo"]["anthropic-version"], "2023-01-01");
}

#[tokio::test]
async fn llm_unknown_protocol_returns_404() {
    let gw = spawn_llm_gateway(vec![]).await;
    let resp = authed()
        .post(gateway_url(&gw, "/llm/wat/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err = error_body(resp).await;
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("wat"),
        "message should name the bad segment: {msg}"
    );
}

#[tokio::test]
async fn llm_missing_protocol_returns_404() {
    let gw = spawn_llm_gateway(vec![]).await;
    let resp = authed()
        .post(gateway_url(&gw, "/llm"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err = error_body(resp).await;
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("missing protocol segment"));
}

#[tokio::test]
async fn llm_no_provider_for_protocol_returns_503() {
    let gw = spawn_llm_gateway(vec![]).await;
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let err = error_body(resp).await;
    assert_eq!(err["code"], 503);
    assert!(err["message"].as_str().unwrap().contains("openai"));
}

#[tokio::test]
async fn llm_disabled_provider_is_not_selected() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.enabled = false;
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn llm_rpm_exhaustion_returns_429() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.rpm_limit = Some(1);
    let gw = spawn_llm_gateway(vec![p]).await;

    let first = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);

    let second = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 429);
    let err = error_body(second).await;
    assert_eq!(err["code"], 429);

    // 配额视图同步：remaining = 0
    let q = quota_view(&gw, "openai-official").await;
    assert_eq!(q["rpm"]["used"], 1);
    assert_eq!(q["rpm"]["remaining"], 0);
}

#[tokio::test]
async fn llm_exhausted_provider_falls_back_to_remaining() {
    let upstream = spawn_mock_llm().await;
    let mut a = provider("openai-a", Protocol::Openai, &upstream);
    a.rpm_limit = Some(1);
    let mut b = provider("openai-b", Protocol::Openai, &upstream);
    b.rpm_limit = Some(1);
    let gw = spawn_llm_gateway(vec![a, b]).await;

    let mut served = HashSet::new();
    for _ in 0..2 {
        let resp = authed()
            .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
            .json(&json!({"model": "gpt-4o"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        served.insert(
            resp.headers()["x-llm-provider"]
                .to_str()
                .unwrap()
                .to_string(),
        );
    }
    // 每家限额 1：两家各被选一次（第二家必然落到未耗尽那家）
    assert_eq!(served.len(), 2);

    let third = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 429);
}

#[tokio::test]
async fn llm_rpm_accounting_is_per_provider() {
    let upstream = spawn_mock_llm().await;
    let mut a = provider("openai-a", Protocol::Openai, &upstream);
    a.rpm_limit = Some(1);
    let mut c = provider("anthropic-c", Protocol::Anthropic, &upstream);
    c.rpm_limit = Some(1);
    let gw = spawn_llm_gateway(vec![a, c]).await;

    let r1 = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);

    // openai-a 已耗尽不影响 anthropic-c
    let r2 = authed()
        .post(gateway_url(&gw, "/llm/anthropic/v1/messages"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
}

#[tokio::test]
async fn llm_counts_tokens_from_openai_usage() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.rpm_limit = Some(10);
    p.tpm_limit = Some(1000);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let q = quota_view(&gw, "openai-official").await;
    assert_eq!(q["tpm"]["used"], 15);
    assert_eq!(q["tpm"]["remaining"], 985);
    assert_eq!(q["tpm"]["limit"], 1000);
    assert_eq!(q["rpm"]["used"], 1);
    assert_eq!(q["window_secs"], 60);
}

#[tokio::test]
async fn llm_counts_tokens_from_anthropic_usage() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("anthropic-official", Protocol::Anthropic, &upstream);
    p.tpm_limit = Some(100);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/anthropic/v1/messages"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let q = quota_view(&gw, "anthropic-official").await;
    assert_eq!(q["tpm"]["used"], 10);
}

#[tokio::test]
async fn llm_streams_response_body() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"));
    assert!(body.contains("data: [DONE]"));
}

#[tokio::test]
async fn llm_counts_tokens_from_streaming_final_chunk() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.tpm_limit = Some(1000);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    let q = quota_view(&gw, "openai-official").await;
    assert_eq!(q["tpm"]["used"], 15);
}

#[tokio::test]
async fn llm_counts_tokens_from_anthropic_streaming() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("anthropic-official", Protocol::Anthropic, &upstream);
    p.tpm_limit = Some(1000);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/anthropic/v1/messages"))
        .json(&json!({"stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    // 首个 input_tokens(3) + 最后一个 output_tokens(7)
    let q = quota_view(&gw, "anthropic-official").await;
    assert_eq!(q["tpm"]["used"], 10);
}

#[tokio::test]
async fn llm_usage_missing_records_zero_tokens() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.rpm_limit = Some(10);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "plain"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let q = quota_view(&gw, "openai-official").await;
    assert_eq!(q["rpm"]["used"], 1);
    assert_eq!(q["tpm"]["used"], 0);
    assert_eq!(q["tpm"]["remaining"], serde_json::Value::Null);
}

#[tokio::test]
async fn llm_unreachable_provider_returns_502() {
    let port = portpicker_forbidden();
    let mut p = provider(
        "openai-official",
        Protocol::Openai,
        &format!("http://127.0.0.1:{port}"),
    );
    p.rpm_limit = Some(10);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    // 回环 connection refused 在本机约 2s，timeout 15s 足够不误判成 504
    assert_eq!(resp.status(), 502);
    let err = error_body(resp).await;
    assert!(err["message"].as_str().unwrap().contains("openai-official"));

    let m = metrics_view(&gw, "openai-official").await;
    assert_eq!(m["stats"]["errors"], 1);
    assert_eq!(m["stats"]["total_requests"], 1);
}

#[tokio::test]
async fn llm_systemone_stub_forwards_without_token_counting() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("systemone-a", Protocol::Systemone, &upstream);
    p.tpm_limit = Some(1000);
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/systemone/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "systemone-a");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["echo"]["authorization"], "Bearer key-systemone-a");
    // 响应里明明有 usage，桩也按 ADR-L8 记 0
    let q = quota_view(&gw, "systemone-a").await;
    assert_eq!(q["tpm"]["used"], 0);
    assert_eq!(q["rpm"]["used"], 1);
}

#[tokio::test]
async fn llm_admin_provider_crud_and_persistence() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![]).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", gw.addr);

    // POST 创建（id 省略时生成 UUID）
    let created: Value = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({
            "name": "OpenAI", "protocol": "openai",
            "base_url": upstream, "api_key": "sk-test",
            "rpm_limit": 60, "tpm_limit": 10000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["protocol"], "openai");
    assert_eq!(created["rpm_limit"], 60);
    let id = created["id"].as_str().unwrap().to_string();

    // GET 单个
    let one: Value = admin_get(&gw, &format!("/__admin/llm/providers/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(one["id"], id);

    // PUT 合并：缺省字段保留；显式 null = 不限
    let merged: Value = client
        .put(format!("{base}/__admin/llm/providers/{id}"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"rpm_limit": serde_json::Value::Null, "enabled": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(merged["tpm_limit"], 10000);
    assert_eq!(merged["rpm_limit"], serde_json::Value::Null);
    assert_eq!(merged["enabled"], false);

    // 落盘了
    let on_disk = std::fs::read_to_string(&gw.providers_path).unwrap();
    assert!(on_disk.contains(&id));
    assert!(on_disk.contains("managed file"));

    // DELETE
    let deleted: Value = client
        .delete(format!("{base}/__admin/llm/providers/{id}"))
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], id);

    let gone = admin_get(&gw, &format!("/__admin/llm/providers/{id}")).await;
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn llm_admin_llm_endpoints_require_admin_key() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let no_key = reqwest::Client::new()
        .get(gateway_url(&gw, "/__admin/llm/providers"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_key.status(), 401);

    let bad_key = reqwest::Client::new()
        .get(gateway_url(&gw, "/__admin/llm/providers"))
        .header("X-Admin-Key", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_key.status(), 401);

    let bearer = reqwest::Client::new()
        .get(gateway_url(&gw, "/__admin/llm/providers"))
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), 200);
}

#[tokio::test]
async fn llm_admin_rejects_invalid_provider() {
    let gw = spawn_llm_gateway(vec![]).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", gw.addr);

    // scheme 非法
    let resp = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"protocol": "openai", "base_url": "ftp://x", "api_key": "k"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // api_key 缺失（必填字段反序列化失败 → axum 标准 422，与路由 API 同行为）
    let resp = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"protocol": "openai", "base_url": "http://up"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);

    // 重复 id
    let created: Value = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"id": "dup", "protocol": "openai", "base_url": "http://up", "api_key": "k"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["id"], "dup");
    let resp = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"id": "dup", "protocol": "openai", "base_url": "http://up", "api_key": "k"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn llm_metrics_view_reports_per_provider_stats() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    for _ in 0..2 {
        let resp = authed()
            .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
            .json(&json!({"model": "gpt-4o"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    let m = metrics_view(&gw, "openai-official").await;
    assert_eq!(m["provider"]["id"], "openai-official");
    assert_eq!(m["stats"]["total_requests"], 2);
    assert_eq!(m["stats"]["errors"], 0);
    assert_eq!(m["stats"]["in_flight"], 0);
    let lat = m["stats"]["avg_latency_ms"].as_f64().unwrap();
    assert!(lat >= 0.0);
}

#[tokio::test]
async fn llm_coexists_with_route_table() {
    let upstream = spawn_mock_llm().await;
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    let providers_path = dir.path().join("providers.toml");
    config::save_providers(
        &providers_path,
        &[provider("openai-official", Protocol::Openai, &upstream)],
    )
    .unwrap();
    config::save_routes(
        &routes_path,
        &[top_gateway::router::Route {
            id: "echo".into(),
            prefix: "/echo".into(),
            upstream: upstream.clone(),
            strip_prefix: true,
            enabled: true,
            description: String::new(),
            headers: Default::default(),
            timeout_secs: None,
            health_path: "/".into(),
        }],
    )
    .unwrap();
    let settings = llm_settings(routes_path, providers_path.clone());
    let state = AppState::new(settings).unwrap();
    state
        .routes
        .set_all(config::load_routes(state.settings.routes_file.as_path()).unwrap());
    state
        .providers
        .set_all(config::load_providers(state.settings.providers_file.as_path()).unwrap());
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let gw = LlmGateway {
        addr,
        _dir: dir,
        providers_path,
    };

    // routes.toml 的前缀转发照常
    let resp = authed()
        .get(gateway_url(&gw, "/echo/v1/anything"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/v1/anything");

    // /llm 保留前缀归 LLM 子网关
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "openai-official");
}

#[tokio::test]
async fn llm_query_string_is_preserved() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let resp = authed()
        .get(gateway_url(&gw, "/llm/openai/v1/models?limit=5&order=desc"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/v1/models");
    assert_eq!(v["query"], "limit=5&order=desc");
    assert!(v["echo"]
        .as_object()
        .unwrap()
        .contains_key("x-forwarded-host"));
}

/// 找一个未被监听的端口用作「不可达上游」。
fn portpicker_forbidden() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}
