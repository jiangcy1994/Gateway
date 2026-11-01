use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;
use top_gateway::admin::RouteInput;
use top_gateway::build_app;
use top_gateway::config::{self, Settings};
use top_gateway::health;
use top_gateway::router::Route;
use top_gateway::state::AppState;

/// A running gateway plus its temp routes file.
struct Gateway {
    addr: SocketAddr,
    _routes_dir: TempDir,
    routes_path: PathBuf,
}

/// A mock sub-gateway: `/health` returns ok, everything else echoes method/path/
/// query/headers back as JSON.
async fn spawn_upstream() -> String {
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .fallback(echo);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn echo(method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Json<Value> {
    let mut echoed = serde_json::Map::new();
    for (name, value) in headers.iter() {
        if let Ok(text) = value.to_str() {
            echoed.insert(name.as_str().to_string(), json!(text));
        }
    }
    Json(json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "headers": echoed,
    }))
}

fn echo_route(upstream: &str) -> Route {
    let mut headers = HashMap::new();
    headers.insert("x-gateway-key".to_string(), "test-secret".to_string());
    Route {
        id: "echo".to_string(),
        prefix: "/echo".to_string(),
        upstream: upstream.to_string(),
        strip_prefix: true,
        enabled: true,
        description: "echo sub-gateway".to_string(),
        headers,
        timeout_secs: Some(5),
        health_path: "/health".to_string(),
    }
}

fn test_settings(routes_path: PathBuf, admin_key: Option<&str>) -> Settings {
    Settings {
        bind: "127.0.0.1:0".to_string(),
        admin_key: admin_key.map(str::to_string),
        routes_file: routes_path,
        providers_file: std::path::PathBuf::from("providers.toml"),
        health_interval_secs: 1,
        health_timeout_secs: 2,
        default_timeout_secs: 30,
    }
}

async fn spawn_gateway(routes: Vec<Route>, _persistent: bool, admin_key: Option<&str>) -> Gateway {
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    // Always exercise the full TOML round trip: write the file, then load it into
    // a fresh state, exactly like `run` does at startup.
    config::save_routes(&routes_path, &routes).unwrap();
    let settings = test_settings(routes_path.clone(), admin_key);
    let state = AppState::new(settings).unwrap();
    state
        .routes
        .set_all(config::load_routes(&routes_path).unwrap());
    health::spawn(state.clone());
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Gateway {
        addr,
        _routes_dir: dir,
        routes_path,
    }
}

fn url(gw: &Gateway, path: &str) -> String {
    format!("http://{}{path}", gw.addr)
}

/// An ephemeral port that was bound and released; connections to it are refused.
fn dropped_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test]
async fn proxy_health_endpoint_answers() {
    let gw = spawn_gateway(vec![], false, None).await;
    let body: Value = reqwest::get(url(&gw, "/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn unmatched_path_returns_404() {
    let gw = spawn_gateway(vec![], false, None).await;
    let resp = reqwest::get(url(&gw, "/nothing/here")).await.unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/nothing/here"));
}

#[tokio::test]
async fn proxy_forwards_stripped_path_query_and_injected_headers() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(vec![echo_route(&upstream)], true, None).await;

    let body: Value = reqwest::get(url(&gw, "/echo/status?a=1&b=two"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/status");
    assert_eq!(body["query"], "a=1&b=two");
    // injected route header reaches the upstream, and the original host is forwarded
    assert_eq!(body["headers"]["x-gateway-key"], "test-secret");
    assert_eq!(body["headers"]["x-forwarded-host"], gw.addr.to_string());
    assert_eq!(body["headers"]["x-forwarded-path"], "/echo/status");
}

#[tokio::test]
async fn proxy_passes_client_authorization_through() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(vec![echo_route(&upstream)], true, None).await;
    let resp = reqwest::Client::new()
        .get(url(&gw, "/echo/who"))
        .header("authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["headers"]["authorization"], "Bearer client-token");
}

#[tokio::test]
async fn proxy_forwards_post_body_and_method() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(vec![echo_route(&upstream)], true, None).await;
    let resp = reqwest::Client::new()
        .post(url(&gw, "/echo/submit"))
        .json(&json!({"hello": "world"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["method"], "POST");
    assert_eq!(body["headers"]["content-type"], "application/json");
}

#[tokio::test]
async fn strip_prefix_false_keeps_full_path() {
    let upstream = spawn_upstream().await;
    let mut route = echo_route(&upstream);
    route.strip_prefix = false;
    let gw = spawn_gateway(vec![route], true, None).await;
    let body: Value = reqwest::get(url(&gw, "/echo/keep"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["path"], "/echo/keep");
}

#[tokio::test]
async fn disabled_route_returns_503() {
    let upstream = spawn_upstream().await;
    let mut route = echo_route(&upstream);
    route.enabled = false;
    let gw = spawn_gateway(vec![route], true, None).await;
    let resp = reqwest::get(url(&gw, "/echo/x")).await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn longest_matching_prefix_wins() {
    let upstream = spawn_upstream().await;
    let mut general = echo_route(&upstream);
    general.prefix = "/echo".to_string();
    let mut specific = echo_route(&upstream);
    specific.prefix = "/echo/special".to_string();
    specific.id = "echo-special".to_string();
    let gw = spawn_gateway(vec![general, specific], true, None).await;
    // both resolve to the same upstream, so compare via stats counters instead
    reqwest::get(url(&gw, "/echo/special/deep")).await.unwrap();
    let metrics: Value = reqwest::get(url(&gw, "/__admin/metrics"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let by_id: HashMap<String, Value> = metrics["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["route"]["id"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    // only the longest match should count the request
    assert_eq!(
        by_id["echo-special"]["stats"]["total_requests"].as_u64(),
        Some(1)
    );
    assert_eq!(by_id["echo"]["stats"]["total_requests"].as_u64(), Some(0));
}

#[tokio::test]
async fn unreachable_upstream_returns_502() {
    // bind and drop an ephemeral port: connecting to it fails fast with a
    // connection refused instead of timing out like a filtered port does
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    };
    let mut route = echo_route(&format!("http://127.0.0.1:{port}"));
    // generous timeout: on some Windows hosts a loopback connection refusal itself
    // takes a couple of seconds, and we want 502 (refused), not 504 (timed out)
    route.timeout_secs = Some(15);
    let gw = spawn_gateway(vec![route], true, None).await;
    let resp = reqwest::get(url(&gw, "/echo/x")).await.unwrap();
    assert_eq!(resp.status(), 502);
}

#[tokio::test]
async fn admin_crud_roundtrip_and_persistence() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(vec![], false, Some("admin-key")).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}/__admin", gw.addr);

    // empty by default
    let list: Value = client
        .get(format!("{base}/routes"))
        .header("x-admin-key", "admin-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list["routes"].as_array().unwrap().is_empty());

    // missing/bad key is rejected, correct key works
    assert_eq!(
        client
            .get(format!("{base}/routes"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/routes"))
            .header("x-admin-key", "wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Bearer form is also accepted
    assert_eq!(
        client
            .get(format!("{base}/routes"))
            .header("authorization", "Bearer admin-key")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // create with defaults
    let created = client
        .post(format!("{base}/routes"))
        .header("x-admin-key", "admin-key")
        .json(&json!({"id": "svc", "prefix": "/svc", "upstream": upstream}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let route: Value = created.json().await.unwrap();
    assert_eq!(route["strip_prefix"], true);
    assert_eq!(route["enabled"], true);
    assert_eq!(route["health_path"], "/health");

    // the new route proxies immediately (no restart needed)
    let body: Value = reqwest::get(url(&gw, "/svc/anything"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["path"], "/anything");

    // invalid and duplicate creates are rejected
    assert_eq!(
        client
            .post(format!("{base}/routes"))
            .header("x-admin-key", "admin-key")
            .json(&json!({"prefix": "no-slash", "upstream": upstream}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .post(format!("{base}/routes"))
            .header("x-admin-key", "admin-key")
            .json(&json!({"id": "svc2", "prefix": "/svc", "upstream": upstream}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );

    // update merges: disabling takes effect live
    let updated: Value = client
        .put(format!("{base}/routes/svc"))
        .header("x-admin-key", "admin-key")
        .json(&json!({"enabled": false, "description": "disabled for test"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["prefix"], "/svc"); // untouched fields preserved
    assert_eq!(
        reqwest::get(url(&gw, "/svc/anything"))
            .await
            .unwrap()
            .status(),
        503
    );

    // unknown id is a 404
    assert_eq!(
        client
            .put(format!("{base}/routes/ghost"))
            .header("x-admin-key", "admin-key")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // delete persists to disk
    let deleted: Value = client
        .delete(format!("{base}/routes/svc"))
        .header("x-admin-key", "admin-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], "svc");
    let on_disk = std::fs::read_to_string(&gw.routes_path).unwrap();
    assert!(!on_disk.contains("prefix = \"/svc\""));

    // and reloads empty
    let list: Value = client
        .get(format!("{base}/routes"))
        .header("x-admin-key", "admin-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list["routes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn admin_without_key_configured_is_open() {
    let gw = spawn_gateway(vec![], false, None).await;
    let resp = reqwest::get(url(&gw, "/__admin/routes")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn routes_file_loads_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let routes_path = dir.path().join("routes.toml");
    config::save_routes(&routes_path, &[echo_route("http://127.0.0.1:9999")]).unwrap();
    let settings = test_settings(routes_path, None);
    let state = AppState::new(settings).unwrap();
    let routes = config::load_routes(state.settings.routes_file.as_path()).unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id, "echo");
}

#[tokio::test]
async fn health_marks_enabled_route_healthy_and_disabled_separately() {
    let upstream = spawn_upstream().await;
    let mut disabled = echo_route(&upstream);
    disabled.id = "off".to_string();
    disabled.prefix = "/off".to_string();
    disabled.enabled = false;
    let gw = spawn_gateway(vec![echo_route(&upstream), disabled], true, None).await;

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let health: Value = reqwest::get(url(&gw, "/__admin/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let by_id: HashMap<String, Value> = health["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r.clone()))
            .collect();
        // before the first sweep the map is empty: retry instead of indexing
        let Some(echo) = by_id.get("echo") else {
            assert!(
                std::time::Instant::now() <= deadline,
                "health snapshot empty: {health}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if echo["status"] == "healthy" {
            assert_eq!(by_id["off"]["status"], "disabled");
            assert!(echo["latency_ms"].as_u64().is_some());
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("upstream never reported healthy: {health}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn probe_endpoint_reports_unhealthy_route() {
    let upstream = format!("http://127.0.0.1:{}", dropped_port());
    let gw = spawn_gateway(vec![echo_route(&upstream)], true, None).await;
    let resp = reqwest::Client::new()
        .post(url(&gw, "/__admin/probe"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
}

#[tokio::test]
async fn metrics_count_requests_and_errors() {
    let upstream = spawn_upstream().await;
    let gw = spawn_gateway(vec![echo_route(&upstream)], true, None).await;
    reqwest::get(url(&gw, "/echo/a")).await.unwrap();
    reqwest::get(url(&gw, "/echo/b")).await.unwrap();
    reqwest::get(url(&gw, "/echo/c")).await.unwrap();

    let metrics: Value = reqwest::get(url(&gw, "/__admin/metrics"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stats = &metrics["routes"][0]["stats"];
    assert_eq!(stats["total_requests"].as_u64(), Some(3));
    assert_eq!(stats["in_flight"].as_u64(), Some(0));
    assert_eq!(stats["errors"].as_u64(), Some(0));
    assert!(stats["avg_latency_ms"].as_f64().unwrap() >= 0.0);
}

#[tokio::test]
async fn route_input_round_trips_through_json() {
    // RouteInput is what the admin API consumes; make sure defaults compile and apply.
    let input: RouteInput =
        serde_json::from_str(r#"{"prefix":"/a","upstream":"http://127.0.0.1:9"}"#).unwrap();
    let route: Route = input.into();
    assert!(route.strip_prefix);
    assert!(route.enabled);
    assert!(route.id.len() > 8); // generated id
    assert!(route.validate().is_ok());
    let bad: Result<RouteInput, _> = serde_json::from_str(r#"{"upstream":"http://127.0.0.1:9"}"#);
    assert!(bad.is_err());
}

#[tokio::test]
async fn unknown_admin_route_is_404_not_proxied() {
    let gw = spawn_gateway(vec![], false, None).await;
    let resp = reqwest::get(url(&gw, "/__admin/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);
}
