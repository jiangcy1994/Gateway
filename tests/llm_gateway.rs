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
use top_gateway::llm::ledger::{UsageFilter, UsageLedger};
use top_gateway::llm::provider::{Model, Protocol, Provider};
use top_gateway::state::AppState;

const ADMIN_KEY: &str = "test-admin-key";

// ---------------------------------------------------------------- harness

struct LlmGateway {
    addr: SocketAddr,
    /// 网关 state 的克隆：usage 测试直接调 `state.llm_usage.flush_if_dirty`
    /// 钉死落盘，不依赖周期任务的时间窗口（测试里 `usage_flush_secs = 0`）。
    state: AppState,
    _dir: TempDir,
    providers_path: PathBuf,
    apps_path: PathBuf,
    usage_path: PathBuf,
}

fn provider(id: &str, protocol: Protocol, base_url: &str) -> Provider {
    Provider {
        id: id.to_string(),
        name: id.to_string(),
        protocols: vec![protocol],
        base_url: base_url.to_string(),
        api_key: format!("key-{id}"),
        rpm_limit: None,
        tpm_limit: None,
        models: vec![],
        timeout_secs: None,
        proxy_url: None,
        enabled: true,
    }
}

/// 多协议供应商（ADR-L13）：说 openai + anthropic 两套端点。
fn multi_protocol_provider(id: &str, base_url: &str) -> Provider {
    Provider {
        id: id.to_string(),
        name: id.to_string(),
        protocols: vec![Protocol::Openai, Protocol::Anthropic],
        base_url: base_url.to_string(),
        api_key: format!("key-{id}"),
        rpm_limit: None,
        tpm_limit: None,
        models: vec![],
        timeout_secs: None,
        proxy_url: None,
        enabled: true,
    }
}

fn llm_settings(
    routes_path: PathBuf,
    providers_path: PathBuf,
    apps_path: PathBuf,
    usage_path: PathBuf,
) -> Settings {
    Settings {
        bind: "127.0.0.1:0".to_string(),
        admin_key: Some(ADMIN_KEY.to_string()),
        routes_file: routes_path,
        providers_file: providers_path,
        bots_file: std::path::PathBuf::from("bots.toml"),
        apps_file: apps_path,
        usage_file: usage_path,
        // 0 = 不起周期 flush 任务；测试落盘走 state.llm_usage.flush_if_dirty
        usage_flush_secs: 0,
        health_interval_secs: 600,
        health_timeout_secs: 2,
        default_timeout_secs: 15,
    }
}

async fn spawn_llm_gateway(providers: Vec<Provider>) -> LlmGateway {
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    let providers_path = dir.path().join("providers.toml");
    let apps_path = dir.path().join("apps.toml");
    let usage_path = dir.path().join("usage.toml");
    config::save_routes(&routes_path, &[]).unwrap();
    config::save_providers(&providers_path, &providers).unwrap();
    let settings = llm_settings(
        routes_path,
        providers_path.clone(),
        apps_path.clone(),
        usage_path.clone(),
    );
    let state = AppState::new(settings).unwrap();
    state
        .routes
        .set_all(config::load_routes(state.settings.routes_file.as_path()).unwrap());
    state
        .providers
        .set_all(config::load_providers(state.settings.providers_file.as_path()).unwrap());
    let app = build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    LlmGateway {
        addr,
        state,
        _dir: dir,
        providers_path,
        apps_path,
        usage_path,
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
/// usage 带 `prompt_tokens_details.cached_tokens`，钉住账本的 input 归一（ADR-L20：
/// `input = prompt_tokens - cached`，TPM 仍读 `total_tokens`）。
async fn chat_completions(
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    if req.get("stream") == Some(&json!(true)) {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n\
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
        resp["usage"] = json!({
            "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 4}
        });
    }
    Json(resp).into_response()
}

/// anthropic：非流式单 usage；`stream == true` 时返 message_start/message_delta
/// SSE。usage 带 cache 字段，钉住账本的四桶映射（ADR-L20；TPM 仍只读
/// `input_tokens` + `output_tokens`）。
async fn messages(headers: HeaderMap, OriginalUri(uri): OriginalUri, body: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    if req.get("stream") == Some(&json!(true)) {
        let sse = "event: message_start\n\
                   data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\n\n\
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
        "usage": {
            "input_tokens": 3, "output_tokens": 7,
            "cache_read_input_tokens": 2, "cache_creation_input_tokens": 1
        },
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

/// `/llm/v1/{端点}`：按端点段推断协议（ADR-L12）——同一上游、两个各自单协议的
/// 供应商，请求该落在哪个池里由端点决定，凭证注入也按推断结果走。
#[tokio::test]
async fn llm_v1_inference_routes_by_endpoint() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![
        provider("openai-official", Protocol::Openai, &upstream),
        provider("anthropic-official", Protocol::Anthropic, &upstream),
    ])
    .await;

    // chat/completions → openai 池：Bearer 注入、路径以 /v1 开头
    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "openai-official");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/v1/chat/completions");
    assert_eq!(v["echo"]["authorization"], "Bearer key-openai-official");

    // messages → anthropic 池：x-api-key + 默认 anthropic-version
    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/messages"))
        .json(&json!({"model": "claude-3-5-sonnet", "max_tokens": 16, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "anthropic-official");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["echo"]["x-api-key"], "key-anthropic-official");
    assert_eq!(v["echo"]["anthropic-version"], "2023-06-01");

    // 推断只影响选池与凭证，转发路径保留客户端写法（大小写原样）
    let resp = authed()
        .post(gateway_url(&gw, "/llm/V1/chat/completions?top_k=1"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "openai-official");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/V1/chat/completions");
    assert_eq!(v["query"], "top_k=1");
}

/// 一个供应商说 openai + anthropic 两套端点（ADR-L13）：两个协议池都选它，
/// RPM/TPM 只记一份——同一分钟内两协议共用配额。
#[tokio::test]
async fn llm_v1_shared_provider_serves_both_protocols() {
    let upstream = spawn_mock_llm().await;
    let mut shared = multi_protocol_provider("shared", &upstream);
    shared.rpm_limit = Some(3);
    let gw = spawn_llm_gateway(vec![shared]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "shared");
    assert_eq!(
        resp.json::<Value>().await.unwrap()["echo"]["authorization"],
        "Bearer key-shared"
    );

    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/messages"))
        .json(&json!({"model": "claude", "max_tokens": 16, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["echo"]["x-api-key"],
        "key-shared"
    );

    // 显式协议段同样落进这个供应商（两个池都是它）
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "shared");

    // 三次请求后 rpm 已用满（跨两个协议共用 3 的限额）-> 第 4 次 429
    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let err = error_body(resp).await;
    assert_eq!(err["code"], 429);

    let q = quota_view(&gw, "shared").await;
    assert_eq!(q["protocols"], json!(["openai", "anthropic"]));
    assert_eq!(q["rpm"]["used"], 3);
    assert_eq!(q["rpm"]["remaining"], 0);
}

#[tokio::test]
async fn llm_v1_ambiguous_endpoint_returns_404() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![multi_protocol_provider("shared", &upstream)]).await;

    // models 两协议族都有：无法无歧义判定，要求显式协议段
    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/models"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err = error_body(resp).await;
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("models"),
        "message should name the endpoint: {msg}"
    );
    assert!(
        msg.contains("explicit protocol segment"),
        "message should hint at the explicit form: {msg}"
    );

    // 显式段下同一端点可正常路由（剥歧义）
    let resp = authed()
        .get(gateway_url(&gw, "/llm/openai/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn llm_v1_unknown_endpoint_returns_404() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![multi_protocol_provider("shared", &upstream)]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/wat"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err = error_body(resp).await;
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("wat"),
        "message should name the endpoint: {msg}"
    );
    assert!(
        msg.contains("cannot infer protocol"),
        "message should say inference failed: {msg}"
    );
}

#[tokio::test]
async fn llm_v1_missing_endpoint_returns_404() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![multi_protocol_provider("shared", &upstream)]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err = error_body(resp).await;
    assert!(err["message"]
        .as_str()
        .unwrap()
        .contains("missing endpoint"));
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

/// ADR-L24：RPM 耗尽的 429 携带 `Retry-After`（delta-seconds 整数 ∈ [1,60]），
/// 消息体与加头前逐字节一致；同测试内非 429 错误（503 池空）不带该头。
#[tokio::test]
async fn llm_429_response_carries_retry_after() {
    let upstream = spawn_mock_llm().await;
    let mut p = provider("openai-official", Protocol::Openai, &upstream);
    p.rpm_limit = Some(1);
    let gw = spawn_llm_gateway(vec![p]).await;

    // 首请求消费唯一 RPM 槽位
    let first = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);

    // 次请求：全池耗尽 → 429 + Retry-After；值只按域断言（Instant 不可注入，
    // 恢复时刻 ≈ 记录 + 60s，故不钉具体秒数）
    let second = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 429);
    let secs: u64 = second
        .headers()
        .get("retry-after")
        .expect("429 carries Retry-After (ADR-L24)")
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After is integer delta-seconds");
    assert!((1..=60).contains(&secs));

    let err = error_body(second).await;
    assert_eq!(err["code"], 429);
    // 消息体钉成实际复合文案的逐字节快照（exhausted() 的文案经 ApiError 的
    // thiserror Display 包装——`{0}` 内嵌一层 `quota exhausted for protocol
    // '...': ...`，故比 exhausted() 的字面量长一圈）
    assert_eq!(
        err["message"].as_str().unwrap(),
        "quota exhausted for protocol 'model 'gpt-4o': all providers are at their rpm or \
         tpm limit (protocol openai)': all providers are at their rpm or tpm limit"
    );

    // 配额视图同步：remaining = 0（既有快照行为不回归）
    let q = quota_view(&gw, "openai-official").await;
    assert_eq!(q["rpm"]["used"], 1);
    assert_eq!(q["rpm"]["remaining"], 0);

    // 非 429 错误（模型有声明但池空 → 503）绝不携带 Retry-After（CK-5）
    let mut disabled = provider_with_models(
        "off",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 0)],
    );
    disabled.enabled = false;
    let gw_off = spawn_llm_gateway(vec![disabled]).await;
    let off = authed()
        .post(gateway_url(&gw_off, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(off.status(), 503);
    assert!(!off.headers().contains_key("retry-after"));
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

// ---------------------------------------------------- 按供应商代理（ADR-L19）

/// 代理端 mock：回显收到的请求行 URI。经 plain-http 代理转发时 reqwest/hyper
/// 发 **absolute-form**（`GET http://host/path HTTP/1.1`），直连发 origin-form
/// （`GET /path`）——代理端是否读到 scheme+host 即区分两种路径
/// （见 hyper-util `client.rs` 的 `absolute_form`）。
async fn spawn_mock_proxy() -> String {
    async fn proxy_echo(method: Method, OriginalUri(uri): OriginalUri) -> Response {
        Json(json!({
            "method": method.as_str(),
            "request_uri": uri.to_string(),
        }))
        .into_response()
    }
    let app = Router::new().fallback(proxy_echo);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// 配了 `proxy_url` 的供应商经代理转发（ADR-L19）：base_url 指向不可达端口，
/// 请求却成功 → 必是经代理抵达；且代理端读到的是 absolute-form URI（带
/// scheme 与目标 host），与直连的 origin-form 相区分。
#[tokio::test]
async fn llm_provider_proxy_routes_through_proxy() {
    let proxy = spawn_mock_proxy().await;
    let port = portpicker_forbidden();
    let mut p = provider(
        "proxied",
        Protocol::Openai,
        &format!("http://127.0.0.1:{port}"),
    );
    p.proxy_url = Some(proxy.clone());
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "chat-pro", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "proxied");
    let v: Value = resp.json().await.unwrap();
    let request_uri = v["request_uri"].as_str().unwrap();
    assert!(
        request_uri.starts_with("http://127.0.0.1:"),
        "代理端应读到 absolute-form URI，实际：{request_uri}"
    );
    // 代理收到的目标是 base_url（那个不可达端口），不是代理自己
    assert!(request_uri.ends_with(&format!(":{port}/v1/chat/completions")));
    // 去掉代理的同名请求直连不可达端口 → 502，衬托上面的 200 确实走了代理
    let plain = provider(
        "plain",
        Protocol::Openai,
        &format!("http://127.0.0.1:{port}"),
    );
    let gw2 = spawn_llm_gateway(vec![plain]).await;
    let resp2 = authed()
        .post(gateway_url(&gw2, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "chat-pro", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 502);
}

/// 代理不可达与上游不可达同语义（ADR-L19 / R13）：502，消息带供应商 id，
/// 供应商错误数 +1。
#[tokio::test]
async fn llm_unreachable_proxy_returns_502() {
    let port = portpicker_forbidden();
    let mut p = provider("proxied", Protocol::Openai, "http://127.0.0.1:1");
    p.proxy_url = Some(format!("http://127.0.0.1:{port}"));
    let gw = spawn_llm_gateway(vec![p]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    // 回环 connection refused 在本机约 2s；authed 客户端 30s 超时足够不误判
    assert_eq!(resp.status(), 502);
    let err = error_body(resp).await;
    assert!(err["message"].as_str().unwrap().contains("proxied"));

    let m = metrics_view(&gw, "proxied").await;
    assert_eq!(m["stats"]["errors"], 1);
    assert_eq!(m["stats"]["total_requests"], 1);
}

/// `proxy_url` 非法形态在配置时就拒绝（P14）：socks5 / 非 http(s) scheme /
/// 无 scheme / 无 host → 400，而不是等请求时才 500。
#[tokio::test]
async fn llm_admin_rejects_invalid_proxy_url() {
    let gw = spawn_llm_gateway(vec![]).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", gw.addr);

    // 合法代理可通过
    let created: Value = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({
            "id": "proxied", "protocols": ["openai"],
            "base_url": "http://up", "api_key": "k",
            "proxy_url": "http://127.0.0.1:7890"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["proxy_url"], "http://127.0.0.1:7890");

    for (i, bad) in [
        "socks5://127.0.0.1:7890",
        "ftp://127.0.0.1:7890",
        "127.0.0.1:7890",
        "http://",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("bad-{i}");
        let resp = client
            .post(format!("{base}/__admin/llm/providers"))
            .header("X-Admin-Key", ADMIN_KEY)
            .json(&json!({
                "id": id, "protocols": ["openai"],
                "base_url": "http://up", "api_key": "k",
                "proxy_url": bad
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "proxy_url {bad} 应被拒绝");
    }
}

/// `proxy_url` 的 PUT 合并语义（ADR-L19）：缺省 = 保留原值；显式 `null` =
/// 清除代理（回到共享直连 client）。清除后落盘的 TOML 不再带该键
/// （Option None 字段在序列化时被跳过），重载等价于不配代理。
#[tokio::test]
async fn llm_admin_provider_proxy_merge_semantics() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![]).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", gw.addr);

    let created: Value = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({
            "id": "proxied", "protocols": ["openai"], "base_url": upstream,
            "api_key": "k", "proxy_url": "http://127.0.0.1:7890"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["proxy_url"], "http://127.0.0.1:7890");

    // 缺省 = 保留原代理
    let merged: Value = client
        .put(format!("{base}/__admin/llm/providers/proxied"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"rpm_limit": 10}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(merged["proxy_url"], "http://127.0.0.1:7890");
    assert_eq!(merged["rpm_limit"], 10);

    // 落盘带着代理键（TOML round-trip 保留 Some 值）
    let on_disk = std::fs::read_to_string(&gw.providers_path).unwrap();
    assert!(on_disk.contains("proxy_url = \"http://127.0.0.1:7890\""));

    // 显式 null = 清除代理，随即可直连上游（选回了共享直连 client）
    let cleared: Value = client
        .put(format!("{base}/__admin/llm/providers/proxied"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"proxy_url": serde_json::Value::Null}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["proxy_url"], serde_json::Value::Null);

    // GET（单项与列表）的供应商 JSON 总是带 proxy_url 键，未配置即 null——
    // 对 0.2.2 之前的消费者是纯新增键（与 rpm_limit/tpm_limit/timeout_secs 同构）
    let got: Value = client
        .get(format!("{base}/__admin/llm/providers/proxied"))
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["proxy_url"], serde_json::Value::Null);
    let listed: Value = admin_get(&gw, "/__admin/llm/providers")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed["providers"][0]["proxy_url"], serde_json::Value::Null);

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "chat-pro", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let on_disk = std::fs::read_to_string(&gw.providers_path).unwrap();
    assert!(!on_disk.contains("proxy_url"));
    // 重载等价：ConfigLoad 回到无代理形态
    let reloaded = config::load_providers(&gw.providers_path).unwrap();
    assert_eq!(reloaded[0].proxy_url, None);
}

/// 0.2.1 时期的生产形态 providers.toml（单值 `protocol`、空模型表的兜底
/// 供应商、带模型表的禁用供应商、注释、无 `proxy_url` 键）在 0.2.2 下加载，
/// 且重写后**不多出代理键**——未配代理的既有部署落盘形态与旧版一致，只有
/// 已知的 `protocol` → `protocols` 迁移（ADR-L13/L14）。
#[tokio::test]
async fn llm_legacy_production_file_round_trips_without_proxy_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.toml");
    std::fs::write(
        &path,
        [
            "# top-gateway providers - managed file, rewrites drop comments",
            "[[providers]]",
            "id = 'inkstone'",
            "name = 'inkstone'",
            "protocol = 'openai'",
            "base_url = 'https://discovery-api.example.com'",
            "api_key = 'sk-legacy'",
            "models = []",
            "",
            "[[providers]]",
            "id = 'nvidia'",
            "name = 'nvidia'",
            "protocols = ['openai']",
            "base_url = 'https://integrate.example.com'",
            "api_key = 'nvapi-legacy'",
            "enabled = false",
            "",
            "  [[providers.models]]",
            "  name = 'chat-pro'",
            "  upstream = 'vendor/chat-pro'",
            "  priority = 10",
        ]
        .join("\n"),
    )
    .unwrap();

    let loaded = config::load_providers(&path).unwrap();
    assert_eq!(loaded.len(), 2);
    // 空模型表 = 协议级兜底（ADR-L14）；单值 protocol 已按 ADR-L13 读入
    assert!(loaded[0].models.is_empty());
    assert_eq!(loaded[0].protocols, vec![Protocol::Openai]);
    assert_eq!(loaded[0].proxy_url, None);
    assert!(!loaded[1].enabled);
    assert_eq!(loaded[1].models.len(), 1);
    assert_eq!(loaded[1].models[0].priority, 10);
    assert_eq!(loaded[1].proxy_url, None);

    // 重写：无代理供应商不落 proxy_url 键（与未加该字段的旧版逐字节等价）
    config::save_providers(&path, &loaded).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("proxy_url"));
    assert!(!text.contains("protocol ="));
    assert!(text.contains("protocols = ["));

    let reloaded = config::load_providers(&path).unwrap();
    assert!(reloaded[0].models.is_empty());
    assert_eq!(reloaded[1].models.len(), 1);
    assert_eq!(reloaded[0].proxy_url, None);
    assert_eq!(reloaded[1].proxy_url, None);
    assert!(!reloaded[1].enabled);
}

/// 文件态端到端（ADR-L19）：providers.toml 里直接写 `proxy_url` → 启动加载
/// 后请求**实际走该代理**——钉住「磁盘配置 → 启动加载 → 转发」整条链路，
/// 而非只有管理 API 一条配置路径。base_url 指向无人监听的端口，请求若能
/// 抵达必是经代理；代理端读到 absolute-form URI。
#[tokio::test]
async fn llm_file_level_proxy_url_routes_request_through_proxy() {
    let proxy = spawn_mock_proxy().await;
    let port = portpicker_forbidden();
    let dir = tempfile::tempdir().unwrap();
    let providers_path = dir.path().join("providers.toml");
    std::fs::write(
        &providers_path,
        [
            "[[providers]]",
            "id = 'via-file'",
            "name = 'via-file'",
            "protocols = ['openai']",
            &format!("base_url = 'http://127.0.0.1:{port}'"),
            "api_key = 'k'",
            &format!("proxy_url = '{proxy}'"),
            "enabled = true",
        ]
        .join("\n"),
    )
    .unwrap();
    let routes_path = dir.path().join("routes.toml");
    config::save_routes(&routes_path, &[]).unwrap();
    let settings = llm_settings(
        routes_path,
        providers_path.clone(),
        dir.path().join("apps.toml"),
        dir.path().join("usage.toml"),
    );
    let state = AppState::new(settings).unwrap();
    state
        .providers
        .set_all(config::load_providers(providers_path.as_path()).unwrap());
    let app = build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let resp = authed()
        .post(format!("http://{addr}/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "chat-pro", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "via-file");
    let v: Value = resp.json().await.unwrap();
    let request_uri = v["request_uri"].as_str().unwrap();
    assert!(
        request_uri.starts_with("http://127.0.0.1:"),
        "代理端应读到 absolute-form URI，实际：{request_uri}"
    );
    assert!(request_uri.ends_with(&format!(":{port}/v1/chat/completions")));
}

/// 启动加载路径**不跑 `validate`**（与 base_url 同构，`load_providers` 后置）：
/// 手工往 providers.toml 写非法 `proxy_url`（socks5）能加载、启动不失败，
/// 但 `validate` 的 400 闸门只在管理 API 路径上；文件态的首个请求把
/// socks5 代理当作传输错误暴露为 502「provider ... unreachable」，运维上
/// 与「代理挂了」不可区分——钉住 R13/R14 这条边界。
#[tokio::test]
async fn llm_hand_edited_invalid_proxy_loads_but_fails_at_request_time() {
    let dir = tempfile::tempdir().unwrap();
    let providers_path = dir.path().join("providers.toml");
    let proxy_port = portpicker_forbidden();
    let port = portpicker_forbidden();
    std::fs::write(
        &providers_path,
        [
            "[[providers]]",
            "id = 'bad-proxy'",
            "name = 'bad-proxy'",
            "protocols = ['openai']",
            &format!("base_url = 'http://127.0.0.1:{port}'"),
            "api_key = 'k'",
            &format!("proxy_url = 'socks5://127.0.0.1:{proxy_port}'"),
            "enabled = true",
        ]
        .join("\n"),
    )
    .unwrap();
    let routes_path = dir.path().join("routes.toml");
    config::save_routes(&routes_path, &[]).unwrap();
    let settings = llm_settings(
        routes_path,
        providers_path.clone(),
        dir.path().join("apps.toml"),
        dir.path().join("usage.toml"),
    );
    let state = AppState::new(settings).unwrap();
    // 启动加载接受 socks5 值（文件态优先于校验）
    let loaded = config::load_providers(providers_path.as_path()).unwrap();
    let expected = format!("socks5://127.0.0.1:{proxy_port}");
    assert_eq!(loaded[0].proxy_url.as_deref(), Some(expected.as_str()));
    state.providers.set_all(loaded);
    let app = build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let resp = authed()
        .post(format!("http://{addr}/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "chat-pro", "messages": []}))
        .send()
        .await
        .unwrap();
    // 回环 connection refused 在本机约 2s；authed 客户端 30s 超时足够不误判
    assert_eq!(resp.status(), 502);
    let err = error_body(resp).await;
    let msg = err["message"].as_str().unwrap();
    assert!(
        msg.contains("bad-proxy") && msg.contains("unreachable"),
        "应归属到供应商的传输错误，实际：{msg}"
    );
    // 请求行里的目标 URL 是 base_url，而不是 socks5 代理 url
    assert!(msg.contains(&format!("127.0.0.1:{port}")));
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
            "name": "OpenAI", "protocols": ["openai"],
            "base_url": upstream, "api_key": "sk-test",
            "rpm_limit": 60, "tpm_limit": 10000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["protocols"], json!(["openai"]));
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
        .json(&json!({"protocols": ["openai"], "base_url": "ftp://x", "api_key": "k"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // api_key 缺失（必填字段反序列化失败 → axum 标准 422，与路由 API 同行为）
    let resp = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(&json!({"protocols": ["openai"], "base_url": "http://up"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);

    // protocols 空列表 / 重复协议 → 400
    for bad in [
        json!({"id": "bad", "protocols": [], "base_url": "http://up", "api_key": "k"}),
        json!({"id": "bad", "protocols": ["openai", "openai"], "base_url": "http://up", "api_key": "k"}),
    ] {
        let resp = client
            .post(format!("{base}/__admin/llm/providers"))
            .header("X-Admin-Key", ADMIN_KEY)
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "payload {bad} should be rejected");
    }

    // 重复 id
    let created: Value = client
        .post(format!("{base}/__admin/llm/providers"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(
            &json!({"id": "dup", "protocols": ["openai"], "base_url": "http://up", "api_key": "k"}),
        )
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
        .json(
            &json!({"id": "dup", "protocols": ["openai"], "base_url": "http://up", "api_key": "k"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// 旧形态 `protocol = "openai"`（单值）仍能读入，免得字段改名让已部署的
/// providers.toml 启动失败（ADR-L13）。两个键都没有则报错，与改名前缺字段一致。
#[tokio::test]
async fn llm_legacy_single_protocol_field_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.toml");
    std::fs::write(
        &path,
        [
            "# top-gateway providers - managed file, rewrites drop comments",
            "[[providers]]",
            "id = 'legacy'",
            "name = 'Legacy'",
            "protocol = 'openai'",
            "base_url = 'http://127.0.0.1:9'",
            "api_key = 'k'",
        ]
        .join("\n"),
    )
    .unwrap();

    let loaded = config::load_providers(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, "legacy");
    assert_eq!(loaded[0].protocols, vec![Protocol::Openai]);

    // 一次原子写后即迁移为新形态：列表形式
    config::save_providers(&path, &loaded).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("protocols = ["));
    assert!(!text.contains("protocol ="));

    // 两个键都没有 → 启动失败（保持缺字段的大错行为，不悄悄给空列表）
    let broken = dir.path().join("broken.toml");
    std::fs::write(
        &broken,
        [
            "[[providers]]",
            "id = 'x'",
            "base_url = 'http://u'",
            "api_key = 'k'",
        ]
        .join("\n"),
    )
    .unwrap();
    let err = config::load_providers(&broken).unwrap_err();
    assert!(matches!(err, top_gateway::error::ApiError::ConfigLoad(_)));
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
    let apps_path = dir.path().join("apps.toml");
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
    let usage_path = dir.path().join("usage.toml");
    let settings = llm_settings(
        routes_path,
        providers_path.clone(),
        apps_path.clone(),
        usage_path.clone(),
    );
    let state = AppState::new(settings).unwrap();
    state
        .routes
        .set_all(config::load_routes(state.settings.routes_file.as_path()).unwrap());
    state
        .providers
        .set_all(config::load_providers(state.settings.providers_file.as_path()).unwrap());
    let app = build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let gw = LlmGateway {
        addr,
        state,
        _dir: dir,
        providers_path,
        apps_path,
        usage_path,
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

// ---------------------------------------------------- 模型映射 / 应用组合新增件

/// 声明模型表的供应商：`models` 至少一条。
fn provider_with_models(
    id: &str,
    protocol: Protocol,
    base_url: &str,
    models: Vec<Model>,
) -> Provider {
    let mut p = provider(id, protocol, base_url);
    p.models = models;
    p
}

fn model(name: &str, upstream: Option<&str>, priority: i32) -> Model {
    Model {
        name: name.to_string(),
        upstream: upstream.map(str::to_string),
        priority,
    }
}

/// 逐字节回显请求 body 的 mock 上游（断言 model 改写 / 零差异透传）。
async fn spawn_body_echo() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: Bytes| async move { ([("content-type", "application/json")], body) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// 睡 `secs` 秒再回 200 的 mock 上游（断言按供应商超时）。
async fn spawn_slow_llm(secs: u64) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            Json(json!({"id": "chatcmpl-slow"}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// 通过管理 API 注册一个 app，返回创建后的 app JSON（含生成的 key）。
async fn create_app(gw: &LlmGateway, body: &Value) -> Value {
    let resp = reqwest::Client::new()
        .post(gateway_url(gw, "/__admin/llm/apps"))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    resp.json().await.unwrap()
}

async fn update_app(gw: &LlmGateway, id: &str, body: &Value) -> Value {
    reqwest::Client::new()
        .put(gateway_url(gw, &format!("/__admin/llm/apps/{id}")))
        .header("X-Admin-Key", ADMIN_KEY)
        .json(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn app_list(gw: &LlmGateway) -> Vec<Value> {
    admin_get(gw, "/__admin/llm/apps")
        .await
        .json::<Value>()
        .await
        .unwrap()["apps"]
        .as_array()
        .cloned()
        .unwrap()
}

fn chat_request(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

/// 向网关发一次 chat/completions，返回 (状态码, 响应 JSON 或 house 错误体)。
async fn post_chat(gw: &LlmGateway, path: &str, body: &Value) -> (u16, Value) {
    let resp = authed()
        .post(gateway_url(gw, path))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    (status, v)
}

async fn post_chat_with_key(gw: &LlmGateway, path: &str, body: &Value, key: &str) -> (u16, Value) {
    let resp = authed()
        .post(gateway_url(gw, path))
        .header("authorization", format!("Bearer {key}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    (status, v)
}

// ------------------------------------------- ADR-L14/L16 模型映射与分层选择

/// ADR-L14：两供应商声明同一网关 model、不同 upstream 名与 priority，
/// 高优先级者被选中，且上游 body 的 model 被改写为该供应商的 upstream 名。
#[tokio::test]
async fn llm_rewrites_model_to_upstream_name() {
    let upstream = spawn_body_echo().await;
    let gw = spawn_llm_gateway(vec![
        provider_with_models(
            "preferred",
            Protocol::Openai,
            &upstream,
            vec![model("gpt-4o", Some("gpt-4o-2024-08-06"), 10)],
        ),
        provider_with_models(
            "fallback",
            Protocol::Openai,
            &upstream,
            vec![model("gpt-4o", Some("gpt-4o-mini-2024"), 1)],
        ),
    ])
    .await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "preferred");
    let echoed: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(echoed["model"], "gpt-4o-2024-08-06");
}

/// ADR-L14：upstream 名与请求 model 相同时不重序列化——上游收到的 body
/// 与客户端发送的逐字节一致（键序保持原样，证明没有经过 Value 往返）。
#[tokio::test]
async fn llm_model_rewrite_zero_diff_passthrough() {
    let upstream = spawn_body_echo().await;
    let gw = spawn_llm_gateway(vec![provider_with_models(
        "openai-official",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 0)],
    )])
    .await;

    // 手工构造键序非字典序的 body：经 Value 重序列化后键会被排序，逐字节比较即可分辨
    let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(&raw[..])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), &raw[..]);
}

/// ADR-L16：高优先级供应商 `rpm_limit = 1` 耗尽后，下一次请求降层到低优先级供应商。
#[tokio::test]
async fn llm_priority_tier_demotes_on_exhaustion() {
    let upstream = spawn_mock_llm().await;
    let mut top = provider_with_models(
        "top",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 10)],
    );
    top.rpm_limit = Some(1);
    let low = provider_with_models(
        "low",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 1)],
    );
    let gw = spawn_llm_gateway(vec![top, low]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "top");

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "low");
}

/// 无任何供应商声明该 model → 404，消息提示模型表位置。
/// 注意：空模型表的兜底供应商参与任意 model 请求（ADR-L14），所以本用例
/// 的供应商都带非空模型表——否则未知模型会被兜底接走。
#[tokio::test]
async fn llm_unknown_model_returns_404() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![
        provider_with_models(
            "a",
            Protocol::Openai,
            &upstream,
            vec![model("gpt-4o", None, 0)],
        ),
        provider_with_models(
            "b",
            Protocol::Openai,
            &upstream,
            vec![model("gpt-4o-mini", None, 0)],
        ),
    ])
    .await;

    let (status, err) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("nope-2x"),
    )
    .await;
    assert_eq!(status, 404);
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("nope-2x"),
        "message should name the model: {msg}"
    );
    assert!(
        msg.contains("/__admin/llm/providers"),
        "message should hint where models are declared: {msg}"
    );
}

/// 模型有声明但池空（供应商 disabled）→ 503，不是 404 或 500。
#[tokio::test]
async fn llm_model_all_disabled_returns_503() {
    let upstream = spawn_mock_llm().await;
    let mut disabled = provider_with_models(
        "off",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 0)],
    );
    disabled.enabled = false;
    let gw = spawn_llm_gateway(vec![disabled]).await;

    let (status, err) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
    )
    .await;
    assert_eq!(status, 503);
    assert!(err["error"]["message"].as_str().unwrap().contains("gpt-4o"));
}

/// 所有层 weight 全 0（rpm 耗尽）→ 429，house 错误体含 model。
#[tokio::test]
async fn llm_all_tiers_exhausted_returns_429() {
    let upstream = spawn_mock_llm().await;
    let mut only = provider_with_models(
        "only",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 0)],
    );
    only.rpm_limit = Some(1);
    let gw = spawn_llm_gateway(vec![only]).await;

    let (status, _) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
    )
    .await;
    assert_eq!(status, 200);
    let (status, err) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
    )
    .await;
    assert_eq!(status, 429);
    assert!(err["error"]["message"].as_str().unwrap().contains("gpt-4o"));
}

/// ADR-L14：显式声明该 model 的供应商优先于空模型表的兜底供应商。
#[tokio::test]
async fn llm_model_request_skips_empty_models_provider() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![
        provider("legacy-catchall", Protocol::Openai, &upstream),
        provider_with_models(
            "declared",
            Protocol::Openai,
            &upstream,
            vec![model("gpt-4o", None, 0)],
        ),
    ])
    .await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "declared");
}

/// ADR-L14 空表兜底：只有无模型表的供应商时，带 model 的请求照常路由、
/// model 原样透传（不改写）。
#[tokio::test]
async fn llm_catchall_provider_passes_model_through() {
    let upstream = spawn_body_echo().await;
    let gw = spawn_llm_gateway(vec![provider("legacy", Protocol::Openai, &upstream)]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "legacy");
    let echoed: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(echoed["model"], "gpt-4o");
}

/// ADR-L14 兜底兼容：历史无模型表 provider 配置在带 model 的请求下仍可路由。
#[tokio::test]
async fn llm_legacy_provider_without_models_still_works() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-llm-provider"], "openai-official");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["path"], "/v1/chat/completions");
}

// ------------------------------------------------------- ADR-L15 应用组合

/// ADR-L15 默认策略：无凭证请求按全部已映射模型放行（历史 `Bearer client-key`
/// 站点去头后的回归基线）。
#[tokio::test]
async fn llm_request_without_credential_uses_default_policy() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider_with_models(
        "openai-official",
        Protocol::Openai,
        &upstream,
        vec![model("gpt-4o", None, 0)],
    )])
    .await;

    let (status, _) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
    )
    .await;
    assert_eq!(status, 200);
}

/// 组合校验盲区（R11）：app 组合非空但请求 body 无 `model` 字段 → 放行，不拦截。
#[tokio::test]
async fn llm_app_combo_allows_models_without_model_field() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    let app = create_app(
        &gw,
        &json!({"id": "cli", "key": "sk-gw-cli-test", "models": ["gpt-4o"]}),
    )
    .await;
    assert_eq!(app["models"][0], "gpt-4o");

    let (status, v) = post_chat_with_key(
        &gw,
        "/llm/openai/v1/chat/completions",
        &json!({"messages": []}),
        "sk-gw-cli-test",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(v["path"], "/v1/chat/completions");
}

/// 组合外 model → 403，消息含 model 与 app id。
#[tokio::test]
async fn llm_app_rejects_model_outside_combo() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    create_app(
        &gw,
        &json!({"id": "cli", "key": "sk-gw-cli-test", "models": ["gpt-4o"]}),
    )
    .await;

    let (status, err) = post_chat_with_key(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-3.5-turbo"),
        "sk-gw-cli-test",
    )
    .await;
    assert_eq!(status, 403);
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("gpt-3.5-turbo"),
        "message should name the model: {msg}"
    );
    assert!(msg.contains("cli"), "message should name the app id: {msg}");
}

/// 凭证存在但不命中任何 app → 401（fail-closed）；去掉凭证后同一请求放行。
#[tokio::test]
async fn llm_unknown_app_key_returns_401() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let (status, err) = post_chat_with_key(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
        "sk-gw-made-up",
    )
    .await;
    assert_eq!(status, 401);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("app key"));

    // 去掉凭证 → 默认策略放行
    let (status, _) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
    )
    .await;
    assert_eq!(status, 200);
}

/// 命中但 `enabled = false` 的 app key → 403。
#[tokio::test]
async fn llm_disabled_app_key_returns_403() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    create_app(&gw, &json!({"id": "cli", "key": "sk-gw-cli-test"})).await;
    update_app(&gw, "cli", &json!({"enabled": false})).await;

    let (status, err) = post_chat_with_key(
        &gw,
        "/llm/openai/v1/chat/completions",
        &chat_request("gpt-4o"),
        "sk-gw-cli-test",
    )
    .await;
    assert_eq!(status, 403);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("disabled"));
}

// ------------------------------------------- ADR-L14 缓冲上限与 ADR-L17 超时

/// 超过 16 MiB 的请求 body → 413，不转发（R12）。
#[tokio::test]
async fn llm_oversized_body_returns_413() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let too_big = " ".repeat(16 * 1024 * 1024 + 1);
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(too_big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

/// ADR-L17：provider `timeout_secs = 1` 指向 sleep 3s 的慢上游 → 504；
/// 不设 timeout 的 provider 走全局默认（15s），同一慢上游正常 200。
#[tokio::test]
async fn llm_provider_timeout_overrides_global() {
    let slow = spawn_slow_llm(3).await;
    let mut quick = provider("quick", Protocol::Openai, &slow);
    quick.timeout_secs = Some(1);
    let gw = spawn_llm_gateway(vec![quick]).await;

    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);

    // 不设 timeout → 全局默认 15s，3s 上游来得及响应
    let gw = spawn_llm_gateway(vec![provider("default", Protocol::Openai, &slow)]).await;
    let resp = authed()
        .post(gateway_url(&gw, "/llm/openai/v1/chat/completions"))
        .json(&chat_request("gpt-4o"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// --------------------------------------------------- ADR-L18 apps 管理面

/// apps CRUD、合并式 PUT（models 列表替换）、key 自动生成 `sk-gw-...`、
/// `apps.toml` 原子写回。
#[tokio::test]
async fn llm_admin_app_crud_and_persistence() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    let client = reqwest::Client::new();
    let base = format!("http://{}/__admin/llm/apps", gw.addr);

    // 空表起步
    assert!(app_list(&gw).await.is_empty());

    // `{}` 也能创建：启用、空组合、自动生成 id 与 sk-gw- key
    let generated = create_app(&gw, &json!({})).await;
    assert_eq!(generated["enabled"], true);
    assert_eq!(generated["models"].as_array().unwrap().len(), 0);
    let gen_key = generated["key"].as_str().unwrap();
    assert!(
        gen_key.starts_with("sk-gw-") && gen_key.len() > "sk-gw-".len() + 8,
        "generated key should look like sk-gw-<uuid>: {gen_key}"
    );
    let generated_id = generated["id"].as_str().unwrap().to_string();

    // 显式指定 id / key / models
    let created = create_app(
        &gw,
        &json!({"id": "cli-code", "name": "Claude Code CLI", "key": "sk-gw-cli", "models": ["chat-pro", "chat-max"]}),
    )
    .await;
    assert_eq!(created["id"], "cli-code");
    assert_eq!(created["name"], "Claude Code CLI");

    // 重复 id / 重复 key → 400
    for body in [
        json!({"id": "cli-code", "key": "other"}),
        json!({"id": "other", "key": "sk-gw-cli"}),
    ] {
        assert_eq!(
            client
                .post(&base)
                .header("X-Admin-Key", ADMIN_KEY)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    // 非法（显式空 id，区别于省略）→ 400
    assert_eq!(
        client
            .post(&base)
            .header("X-Admin-Key", ADMIN_KEY)
            .json(&json!({"id": "", "key": "k"}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    // 空白 key 同样非法；纯空白 model 名不卡（组合语义里无意义）
    assert_eq!(
        client
            .post(&base)
            .header("X-Admin-Key", ADMIN_KEY)
            .json(&json!({"id": "x", "key": "   "}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    // JSON 体不合法 → 400（axum JsonRejection；app 无必填字段，422 不可达）
    assert_eq!(
        client
            .post(&base)
            .header("X-Admin-Key", ADMIN_KEY)
            .header("content-type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );

    // GET 单个；未知 → 404
    let one: Value = client
        .get(format!("{base}/cli-code"))
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["id"], "cli-code");
    assert_eq!(
        client
            .get(format!("{base}/ghost"))
            .header("X-Admin-Key", ADMIN_KEY)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // PUT 合并：未给字段保留；models 给列表 = 整体替换；key 不可改
    let updated = update_app(
        &gw,
        "cli-code",
        &json!({"models": ["chat-mini"], "enabled": false}),
    )
    .await;
    assert_eq!(updated["name"], "Claude Code CLI"); // 未给 → 保留
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["models"].as_array().unwrap().len(), 1);
    assert_eq!(updated["models"][0], "chat-mini");
    assert_eq!(updated["key"], "sk-gw-cli");

    // 落盘：重读 apps.toml 断言
    let on_disk = std::fs::read_to_string(&gw.apps_path).unwrap();
    assert!(on_disk.contains("id = \"cli-code\""));
    assert!(on_disk.contains("chat-mini"));
    assert!(on_disk.contains("sk-gw-cli"));
    assert!(!on_disk.contains("chat-pro")); // 已被整体替换

    // DELETE：未知 → 404；成功 → 200 且持久化后消失
    assert_eq!(
        client
            .delete(format!("{base}/ghost"))
            .header("X-Admin-Key", ADMIN_KEY)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let deleted: Value = client
        .delete(format!("{base}/cli-code"))
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], "cli-code");
    let on_disk = std::fs::read_to_string(&gw.apps_path).unwrap();
    assert!(!on_disk.contains("cli-code"));

    // 只剩自动生成的那个 app
    let remaining: Vec<String> = app_list(&gw)
        .await
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(remaining, vec![generated_id]);
}

// ------------------------------------------- ADR-L20 按日 × 应用用量账本

async fn usage_view(gw: &LlmGateway, query: &str) -> Vec<Value> {
    let v: Value = admin_get(gw, &format!("/__admin/llm/usage{query}"))
        .await
        .json()
        .await
        .unwrap();
    v["usage"].as_array().cloned().unwrap()
}

fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

fn shift_days(days: i64) -> String {
    (chrono::Utc::now().date_naive() + chrono::Duration::days(days)).to_string()
}

fn usage_row(rows: &[Value], app: Option<&str>) -> Value {
    rows.iter()
        .find(|row| match app {
            Some(app) => row["app"].as_str() == Some(app),
            None => row["app"].is_null(),
        })
        .cloned()
        .unwrap_or_else(|| panic!("usage row for app {app:?} not found in {rows:?}"))
}

/// 四桶 + total 一起取，便于逐条断言。
fn buckets(row: &Value) -> (u64, u64, u64, u64, u64) {
    (
        row["input"].as_u64().unwrap(),
        row["output"].as_u64().unwrap(),
        row["cache_read"].as_u64().unwrap(),
        row["cache_write"].as_u64().unwrap(),
        row["total"].as_u64().unwrap(),
    )
}

/// openai 非流式：`input = prompt_tokens - cached_tokens` 归一、`total` = 四桶之和；
/// app 维度独立累加（两 app 各自翻倍 / 单次，互不串）；无凭证流量单列
/// `app: null` 条目（ADR-L20）。
#[tokio::test]
async fn llm_usage_records_per_app_buckets() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    create_app(&gw, &json!({"id": "app-a", "key": "key-a"})).await;
    create_app(&gw, &json!({"id": "app-b", "key": "key-b"})).await;

    let body = json!({"model": "gpt-4o"});
    let path = "/llm/openai/v1/chat/completions";
    for _ in 0..2 {
        let (status, _) = post_chat_with_key(&gw, path, &body, "key-a").await;
        assert_eq!(status, 200);
    }
    let (status, _) = post_chat_with_key(&gw, path, &body, "key-b").await;
    assert_eq!(status, 200);
    let (status, _) = post_chat(&gw, path, &body).await;
    assert_eq!(status, 200);

    let rows = usage_view(&gw, "").await;
    assert_eq!(rows.len(), 3, "应有 3 行：app-a / app-b / 无凭证");
    // 无凭证条目：单列、app 为 null
    let anon = usage_row(&rows, None);
    assert_eq!(anon["date"], today());
    assert_eq!(buckets(&anon), (6, 5, 4, 0, 15));
    // app 维度互不串：a 两次累加，b 一次
    assert_eq!(
        buckets(&usage_row(&rows, Some("app-a"))),
        (12, 10, 8, 0, 30)
    );
    assert_eq!(buckets(&usage_row(&rows, Some("app-b"))), (6, 5, 4, 0, 15));
}

/// anthropic 非流式：四个原生字段一一落到四个桶（ADR-L20 跨协议桶对齐）。
#[tokio::test]
async fn llm_usage_records_anthropic_cache_buckets() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "anthropic-official",
        Protocol::Anthropic,
        &upstream,
    )])
    .await;

    let (status, _) = post_chat(
        &gw,
        "/llm/anthropic/v1/messages",
        &json!({"model": "claude-3-5-sonnet"}),
    )
    .await;
    assert_eq!(status, 200);

    let rows = usage_view(&gw, "").await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(row["app"].is_null());
    assert_eq!(row["date"], today());
    // input 3 / output 7 / cache_read 2 / cache_write 1 = total 13
    assert_eq!(buckets(row), (3, 7, 2, 1, 13));
}

/// 流式也记账：openai SSE 末块 usage 聚合（含 cached 归一）；anthropic SSE
/// 取首个 input/cache 值 + 末个累计 output（`extract_usage` SSE 契约）。
#[tokio::test]
async fn llm_usage_records_from_streaming_final_chunk() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![
        provider("openai-official", Protocol::Openai, &upstream),
        provider("anthropic-official", Protocol::Anthropic, &upstream),
    ])
    .await;
    // 同日两笔流量天然合并：用 app 凭证把两协议拆成两行
    create_app(&gw, &json!({"id": "app-a", "key": "key-a"})).await;

    let (status, _) = post_chat_with_key(
        &gw,
        "/llm/openai/v1/chat/completions",
        &json!({"model": "gpt-4o", "stream": true}),
        "key-a",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = post_chat(
        &gw,
        "/llm/anthropic/v1/messages",
        &json!({"model": "claude-3-5-sonnet", "stream": true}),
    )
    .await;
    assert_eq!(status, 200);

    let rows = usage_view(&gw, "").await;
    assert_eq!(rows.len(), 2);
    // openai SSE：prompt 10 - cached 4 = input 6
    assert_eq!(buckets(&usage_row(&rows, Some("app-a"))), (6, 5, 4, 0, 15));
    // anthropic SSE：message_start 的 input/cache + message_delta 的累计 output
    assert_eq!(buckets(&usage_row(&rows, None)), (3, 7, 2, 1, 13));
}

/// `/__admin/llm/usage` 过滤：`app` / `date` / `from`-`to`（闭区间）AND 语义；
/// 结果按 `(date, app)` 升序、无凭证条目同日最前；非法日期参数 → 400。
#[tokio::test]
async fn llm_usage_view_filters_by_app_date_range() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;
    create_app(&gw, &json!({"id": "app-a", "key": "key-a"})).await;
    create_app(&gw, &json!({"id": "app-b", "key": "key-b"})).await;

    let body = json!({"model": "gpt-4o"});
    let path = "/llm/openai/v1/chat/completions";
    let (status, _) = post_chat_with_key(&gw, path, &body, "key-a").await;
    assert_eq!(status, 200);
    let (status, _) = post_chat_with_key(&gw, path, &body, "key-b").await;
    assert_eq!(status, 200);
    let (status, _) = post_chat(&gw, path, &body).await;
    assert_eq!(status, 200);

    // app 过滤
    let rows = usage_view(&gw, "?app=app-a").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["app"], json!("app-a"));

    // date 过滤：今天全中、昨天空
    assert_eq!(
        usage_view(&gw, &format!("?date={}", today())).await.len(),
        3
    );
    assert_eq!(
        usage_view(&gw, &format!("?date={}", shift_days(-1)))
            .await
            .len(),
        0
    );

    // 闭区间 from/to：昨天到明天全中、明天起空
    assert_eq!(
        usage_view(
            &gw,
            &format!("?from={}&to={}", shift_days(-1), shift_days(1))
        )
        .await
        .len(),
        3
    );
    assert_eq!(
        usage_view(&gw, &format!("?from={}", shift_days(1)))
            .await
            .len(),
        0
    );

    // 空串 = 未传，四个参数同一约定：不报错、不过滤
    assert_eq!(usage_view(&gw, "?date=").await.len(), 3);
    assert_eq!(usage_view(&gw, "?from=&to=").await.len(), 3);

    // 结构非法的 query 走 ApiError 的 400 JSON 信封（非 axum 纯文本拒绝）
    let resp = admin_get(&gw, "/__admin/llm/usage?date=a&date=b").await;
    assert_eq!(resp.status(), 400);
    let err = error_body(resp).await;
    assert_eq!(err["code"].as_u64().unwrap_or(0), 400);
    assert!(err["message"].is_string());

    // 升序：无凭证条目（app = null）同日最前
    let rows = usage_view(&gw, "").await;
    assert!(rows[0]["app"].is_null());
    assert_eq!(rows[1]["app"], json!("app-a"));
    assert_eq!(rows[2]["app"], json!("app-b"));

    // 非法日期 → 400（错误体带参数名与原值）
    for bad in ["not-a-date", "26-11-06"] {
        let resp = admin_get(&gw, &format!("/__admin/llm/usage?date={bad}")).await;
        assert_eq!(resp.status(), 400, "date={bad} 应被拒绝");
        let err = error_body(resp).await;
        let msg = err["message"].as_str().unwrap();
        assert!(msg.contains(&format!("date='{bad}'")), "消息: {msg}");
    }
    let resp = admin_get(&gw, "/__admin/llm/usage?from=2026-13-01").await;
    assert_eq!(resp.status(), 400);
}

/// 账本写盘与重启加载（ADR-L20/L21）：flush 落 `usage.toml` 且 round-trip；
/// 另一网关（新 state）从文件加载后历史可见，同日同 app 行接着累计。
#[tokio::test]
async fn llm_usage_ledger_flush_writes_and_reloads() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    let (status, _) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &json!({"model": "gpt-4o"}),
    )
    .await;
    assert_eq!(status, 200);

    // flush：dirty → 全量写盘（harness 直接调，测试里 usage_flush_secs = 0）
    gw.state.llm_usage.flush_if_dirty(&gw.usage_path).unwrap();
    assert!(gw.usage_path.exists(), "usage.toml 应被创建");

    // 落盘行与查询快照一致（round-trip）。盘上无凭证条目写作 `app = ""`
    // （无 app id 可落），经 set_all → snapshot 边界再转回 null，与端点形态对齐。
    let rows = usage_view(&gw, "").await;
    let entries = config::load_usage(&gw.usage_path).unwrap();
    let reloaded = UsageLedger::new();
    reloaded.set_all(entries);
    assert_eq!(
        serde_json::to_value(reloaded.snapshot(&UsageFilter::default())).unwrap(),
        Value::Array(rows.clone())
    );
    let on_disk = std::fs::read_to_string(&gw.usage_path).unwrap();
    assert!(
        on_disk.contains("app = \"\""),
        "盘上无凭证条目应为 app = \"\": {on_disk}"
    );

    // 「重启」：新网关加载同一份 usage.toml
    let dir2 = tempfile::tempdir().unwrap();
    let providers2 = dir2.path().join("providers.toml");
    config::save_providers(
        &providers2,
        &[provider("openai-official", Protocol::Openai, &upstream)],
    )
    .unwrap();
    let usage2 = dir2.path().join("usage.toml");
    std::fs::copy(&gw.usage_path, &usage2).unwrap();
    let state2 = AppState::new(llm_settings(
        dir2.path().join("routes.toml"),
        providers2.clone(),
        dir2.path().join("apps.toml"),
        usage2.clone(),
    ))
    .unwrap();
    state2
        .providers
        .set_all(config::load_providers(&providers2).unwrap());
    state2
        .llm_usage
        .set_all(config::load_usage(&usage2).unwrap());
    let app2 = build_app(state2.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app2).await;
    });
    let usage_url = format!("http://{addr2}/__admin/llm/usage");

    // 历史可见：reload 后行与 flush 前一致（不新建条目）
    let hist: Value = reqwest::Client::new()
        .get(&usage_url)
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hist["usage"].as_array().unwrap(), &rows);

    // 新请求在已加载的历史上接着累计（同日同 app 行合并）
    let resp = authed()
        .post(format!("http://{addr2}/llm/openai/v1/chat/completions"))
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let after: Value = reqwest::Client::new()
        .get(&usage_url)
        .header("X-Admin-Key", ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let after_rows = after["usage"].as_array().unwrap();
    assert_eq!(after_rows.len(), 1);
    assert_eq!(buckets(&after_rows[0]), (12, 10, 8, 0, 30));

    // 累计后的账本再次 round-trip 落盘
    state2.llm_usage.flush_if_dirty(&usage2).unwrap();
    let reloaded = config::load_usage(&usage2).unwrap();
    assert_eq!(reloaded.len(), 1);
    let row = &reloaded[0];
    assert_eq!(
        (row.input, row.output, row.cache_read, row.total),
        (12, 10, 8, 30)
    );
}

/// flush 不是无脑周期写：干净账本（无流量 / 刚加载）既不创建文件也不改文件。
#[tokio::test]
async fn llm_usage_ledger_clean_no_op() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "openai-official",
        Protocol::Openai,
        &upstream,
    )])
    .await;

    // 无流量：不创建文件
    assert!(!gw.usage_path.exists());
    gw.state.llm_usage.flush_if_dirty(&gw.usage_path).unwrap();
    assert!(!gw.usage_path.exists(), "干净账本不应创建 usage.toml");

    // 有流量：flush 一次后 dirty 已清，再 flush 内容逐字节不变
    let (status, _) = post_chat(
        &gw,
        "/llm/openai/v1/chat/completions",
        &json!({"model": "gpt-4o"}),
    )
    .await;
    assert_eq!(status, 200);
    gw.state.llm_usage.flush_if_dirty(&gw.usage_path).unwrap();
    let first = std::fs::read_to_string(&gw.usage_path).unwrap();
    gw.state.llm_usage.flush_if_dirty(&gw.usage_path).unwrap();
    assert_eq!(std::fs::read_to_string(&gw.usage_path).unwrap(), first);
}

/// systemone 桩（ADR-L8/L20）：上游响应里明明有 usage，账本条目存在但四桶恒 0
/// ——账本不因桩产生虚假用量，同时保留可审计的条目。
#[tokio::test]
async fn llm_usage_records_systemone_zero_buckets() {
    let upstream = spawn_mock_llm().await;
    let gw = spawn_llm_gateway(vec![provider(
        "systemone-a",
        Protocol::Systemone,
        &upstream,
    )])
    .await;

    let (status, v) = post_chat(
        &gw,
        "/llm/systemone/v1/chat/completions",
        &json!({"model": "gpt-4o"}),
    )
    .await;
    assert_eq!(status, 200);
    // 上游确实回了 usage（桩对账本「视而不见」的前提）
    assert_eq!(v["usage"]["total_tokens"], 15);

    let rows = usage_view(&gw, "").await;
    assert_eq!(rows.len(), 1, "条目存在但桶为 0");
    assert!(rows[0]["app"].is_null());
    assert_eq!(buckets(&rows[0]), (0, 0, 0, 0, 0));
}

// ---------------------------------------------------------------- examples

/// `examples/` 下的示例 TOML 必须能被真实加载器解析并通过 `validate`，且示例
/// 演示的分层 / 超时 / 组合不变式成立——这是示例文档不腐坏的机械保证（文档
/// 侧质量门，无对应 SDD 契约；`mapping.md` 标为 examples 维护条目）。
#[test]
fn llm_example_configs_parse_and_validate() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let providers_path = root.join("examples").join("providers.example.toml");
    let apps_path = root.join("examples").join("apps.example.toml");
    assert!(
        providers_path.exists(),
        "missing {}",
        providers_path.display()
    );
    assert!(apps_path.exists(), "missing {}", apps_path.display());

    let providers = config::load_providers(&providers_path)
        .unwrap_or_else(|e| panic!("parse {}: {e}", providers_path.display()));
    let apps = config::load_apps(&apps_path)
        .unwrap_or_else(|e| panic!("parse {}: {e}", apps_path.display()));

    assert_eq!(providers.len(), 3, "示例应含 3 个供应商");
    for provider in &providers {
        provider
            .validate()
            .unwrap_or_else(|e| panic!("provider '{}' 无效: {e}", provider.id));
    }
    assert_eq!(apps.len(), 3, "示例应含 3 个 app");
    for app in &apps {
        app.validate()
            .unwrap_or_else(|e| panic!("app '{}' 无效: {e}", app.id));
    }
    // id / key 全表唯一（load_* 只告警不拒绝，示例必须自己干净）
    let ids: HashSet<_> = apps.iter().map(|a| a.id.clone()).collect();
    let keys: HashSet<_> = apps.iter().map(|a| a.key.clone()).collect();
    assert_eq!(ids.len(), apps.len());
    assert_eq!(keys.len(), apps.len());

    // 示例演示的分层（ADR-L16）：同一网关模型被两家声明，优先级 10 与 1
    let priorities = providers
        .iter()
        .flat_map(|p| {
            p.models
                .iter()
                .filter(|m| m.name == "chat-pro")
                .map(move |m| (p.id.clone(), m.priority))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        priorities,
        vec![
            ("shared-primary".to_string(), 10),
            ("shared-fallback".to_string(), 1),
        ]
    );
    // 示例演示的按供应商超时（ADR-L17）与协议级改写（upstream 与 name 不同）
    let open_models = providers
        .iter()
        .find(|p| p.id == "open-models")
        .expect("示例应含 open-models 供应商");
    assert_eq!(open_models.timeout_secs, Some(60));
    // 共享上游两条故意不写 timeout_secs：与 open-models 的显式覆盖成对演示
    // 「缺省继承全局默认」（ADR-L17）
    for provider in &providers {
        if provider.id.starts_with("shared-") {
            assert!(
                provider.timeout_secs.is_none(),
                "provider '{}' 应演示缺省继承超时",
                provider.id
            );
        }
    }
    // 示例演示的按供应商代理（ADR-L19）：只有 open-models 需要经本地代理可达，
    // 共享上游两条故意不写 proxy_url，与显式配置成对演示「缺省 = 共享直连」
    assert_eq!(
        open_models.proxy_url.as_deref(),
        Some("http://127.0.0.1:7890")
    );
    for provider in &providers {
        if provider.id.starts_with("shared-") {
            assert!(
                provider.proxy_url.is_none(),
                "provider '{}' 应演示缺省直连",
                provider.id
            );
        }
    }
    assert_eq!(open_models.models.len(), 3);
    assert!(open_models
        .models
        .iter()
        .all(|m| m.upstream != Some(m.name.clone())));

    // 示例里 app 组合必须全部指向有供应商声明的网关模型（否则组合内模型 404）
    let declared: HashSet<_> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.name.clone()))
        .collect();
    for app in &apps {
        for model in &app.models {
            assert!(
                declared.contains(model),
                "app '{}' 组合引用了未被任何供应商声明的模型 '{model}'",
                app.id
            );
        }
    }
    // 示例必须覆盖三种组合形态：限定 / 混搭 / 空表不限制
    let combos: Vec<&[String]> = apps.iter().map(|a| a.models.as_slice()).collect();
    assert!(combos.iter().any(|m| m.is_empty()), "缺「空表不限制」示例");
    assert!(
        combos.iter().any(|m| m.len() >= 2),
        "缺「多模型限定组合」示例"
    );
}
