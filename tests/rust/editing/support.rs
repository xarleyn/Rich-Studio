use std::collections::BTreeMap;

use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use rich_studio::store::posts;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(crate) fn config(dir: &TempDir) -> Settings {
    Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn configured(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:fixture".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@channel".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn now() -> i64 {
    rich_studio::store::auth::now()
}

pub(crate) async fn http(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (axum::http::StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(axum::body::Body::empty, |value| {
            axum::body::Body::from(value.to_string())
        }))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}"))
    };
    (status, value)
}

pub(crate) async fn served(
    dir: &TempDir,
) -> Result<(axum::Router, AppState, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>>
{
    let (base, mock, server) = mock().await?;
    let state = AppState::new(configured(dir, &base));
    let app = build_router(state.clone());
    Ok((app, state, mock, server))
}

pub(crate) fn last_call(mock: &Mock, method: &str) -> Option<Value> {
    mock.calls
        .lock()
        .unwrap_or_else(|error| panic!("mock lock: {error}"))
        .iter()
        .rev()
        .find(|(name, _)| name == method)
        .map(|(_, body)| body.clone())
}

pub(crate) fn call_count(mock: &Mock, method: &str) -> usize {
    mock.calls
        .lock()
        .unwrap_or_else(|error| panic!("mock lock: {error}"))
        .iter()
        .filter(|(name, _)| name == method)
        .count()
}

pub(crate) fn post(text: &str) -> Value {
    json!({"rich_message":{"markdown":text}})
}

pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

pub(crate) fn add(context: &AccountContext, status: &str, payload: &Value) -> Value {
    posts::add(
        context,
        posts::NewPost {
            chat_id: &json!("@channel"),
            payload,
            draft_id: None,
            status,
            message_id: Some(10),
            scheduled_at: Some(4070930400),
            error: None,
            kind: "rich",
            actor: "fixture",
        },
    )
    .unwrap_or_else(|error| panic!("post: {error}"))
}

#[derive(Clone)]
pub(crate) struct Mock {
    pub(crate) mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    pub(crate) calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    pub(crate) entered: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) release: std::sync::Arc<tokio::sync::Notify>,
}

pub(crate) async fn mock()
-> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    use axum::{
        Json,
        extract::{Path, State},
        http::StatusCode,
    };
    let state = Mock {
        mode: Default::default(),
        calls: Default::default(),
        entered: Default::default(),
        release: Default::default(),
    };
    let app = axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async |State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>| {
        mock.calls.lock().unwrap_or_else(|error| panic!("mock lock: {error}")).push((method,body));
        let mode = mock.mode.load(std::sync::atomic::Ordering::SeqCst);
        if mode == 4 { mock.entered.notify_one(); mock.release.notified().await; }
        match mode {
            1 => (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"refused"}))),
            2 => (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}))),
            3 => (StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"error_code":502,"description":"gateway"}))),
            _ => (StatusCode::OK,Json(json!({"ok":true,"result":{"message_id":10}}))),
        }
    })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), state, server))
}
