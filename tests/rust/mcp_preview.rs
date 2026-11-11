use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::settings::Settings;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn request(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())));
    let response = app
        .oneshot(request.unwrap_or_else(|error| panic!("request: {error}")))
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        },
    )
}

async fn rpc(app: Router, method: &str, params: Value) -> Value {
    request(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params})),
    )
    .await
    .1
}

fn app(dir: &TempDir) -> Router {
    let settings = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    build_router(AppState::new(settings))
}

#[tokio::test]
async fn preview_resource_is_self_contained_and_tool_never_publishes() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir);
    let listed = rpc(app.clone(), "tools/list", json!({})).await;
    let preview = listed["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["name"] == "preview_post")
        .ok_or("preview_post tool")?;
    assert_eq!(
        preview["_meta"]["ui"]["resourceUri"],
        "ui://rich-studio/preview"
    );
    assert_eq!(preview["annotations"]["readOnlyHint"], true);
    let resource = rpc(
        app.clone(),
        "resources/read",
        json!({"uri":"ui://rich-studio/preview"}),
    )
    .await;
    let page = &resource["result"]["contents"][0];
    assert_eq!(page["mimeType"], "text/html;profile=mcp-app");
    let html = page["text"].as_str().ok_or("html")?;
    assert!(html.contains("RichRender") && html.contains("ui/initialize"));
    assert!(
        !html
            .split("<body>")
            .next()
            .unwrap_or_default()
            .contains("src=")
    );
    assert_eq!(
        html.matches("</script").count(),
        html.matches("<script").count()
    );

    let post = json!({"rich_message":{"blocks":[{"type":"paragraph","text":"Предпросмотр"}]}});
    let shown = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"preview_post","arguments":{"post":post.clone()}}),
    )
    .await;
    // Captured Python Post.model_dump defaults; rich_message itself is kept verbatim.
    assert_eq!(
        shown["result"]["structuredContent"]["post"],
        json!({
            "rich_message":post["rich_message"], "reply_markup":null, "options":{}, "pin":false,
            "comment":null, "callbacks":{}, "poll_after":null,
        })
    );
    assert_eq!(
        shown["result"]["structuredContent"]["validation"]["ok"],
        true
    );
    let invalid = rpc(
        app,
        "tools/call",
        json!({"name":"preview_post","arguments":{"post":{"rich_message":{}}}}),
    )
    .await;
    assert!(
        !invalid["result"]["structuredContent"]["validation"]["errors"]
            .as_array()
            .unwrap_or(&vec![])
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn preview_draft_round_trip_and_service_worker_headers() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir);
    let saved = rpc(app.clone(), "tools/call", json!({"name":"save_draft","arguments":{"title":"Черновик","post":{"rich_message":{"html":"<p>x</p>"}}}})).await;
    let id = saved["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("draft id")?;
    let shown = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"preview_post","arguments":{"draft_id":id}}),
    )
    .await;
    assert_eq!(shown["result"]["structuredContent"]["version"], 0);
    let (status, _) = request(
        app.clone(),
        "GET",
        &format!("/render/draft/{id}?theme=dark"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .uri("/sw.js")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("service-worker-allowed")
            .and_then(|value| value.to_str().ok()),
        Some("/")
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    Ok(())
}
