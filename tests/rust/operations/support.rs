use std::collections::BTreeMap;

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    metrics::Metrics,
    services::health,
    settings::Settings,
    store::db::{self, Store},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
pub(crate) type TestError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) fn settings(
    directory: &TempDir,
) -> Result<Settings, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))?)
}

pub(crate) fn initialize(
    context: &AccountContext,
) -> Result<rusqlite::Connection, rich_studio::errors::StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection)
}

pub(crate) fn photo(name: &str) -> Value {
    json!({"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":format!("upload:{name}")}}]}})
}

pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

pub(crate) fn telegram_settings(directory: &TempDir, base: &str) -> Result<Settings, TestError> {
    let mut settings = settings(directory)?;
    settings.api_base = base.to_owned();
    settings.bot_token = "1:x".to_owned();
    settings.default_chat = "@test_channel".to_owned();
    settings.preview_chat = "777".to_owned();
    Ok(settings)
}

/// A Bot API stand-in: any method succeeds, rich messages get a message id.
pub(crate) async fn telegram() -> Result<String, TestError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback(|request: axum::extract::Request| async move {
        let method = request
            .uri()
            .path()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let _ = to_bytes(request.into_body(), 1 << 20).await;
        let result = if method == "sendRichMessage" {
            json!({"message_id":101,"chat":{"id":1},"rich_message":{"blocks":[]}})
        } else {
            json!(true)
        };
        axum::Json(json!({"ok":true,"result":result}))
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

pub(crate) async fn json_request(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), TestError> {
    let builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
        None => builder.body(Body::empty())?,
    };
    let response = app.oneshot(request).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

pub(crate) async fn upload(app: axum::Router, name: &str, data: &[u8]) -> Result<Value, TestError> {
    let boundary = "rich-studio-operations-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .method("POST")
                .uri("/api/media")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{value}");
    Ok(value)
}

pub(crate) fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == name))
        .unwrap_or_else(|| panic!("check {name} is missing: {report}"))
}

pub(crate) fn initialized(settings: Settings) -> Result<AppState, TestError> {
    let state = AppState::new(settings);
    initialize(&state.account)?;
    Ok(state)
}

pub(crate) async fn report(state: &AppState) -> Value {
    report_with(state, &state.metrics).await
}

pub(crate) async fn report_with(state: &AppState, metrics: &Metrics) -> Value {
    let spec = state.spec.read().await.clone();
    let loops = state.loop_status.read().await.clone();
    let updates = state.updates_status.read().await.clone();
    health::report(
        &state.settings,
        &state.account,
        &spec,
        &state.logs,
        metrics,
        &loops,
        &updates,
    )
}

pub(crate) async fn request_with_headers(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, Value, HeaderMap), TestError> {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
        None => builder.body(Body::empty())?,
    };
    let response = app.oneshot(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    ))
}

pub(crate) async fn telegram_error(status: u16, body: Value) -> Result<String, TestError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback(move |_request: axum::extract::Request| {
        let body = body.clone();
        async move {
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                axum::Json(body),
            )
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

pub(crate) async fn mcp_json(
    app: axum::Router,
    tool: &str,
    arguments: Value,
) -> Result<Value, TestError> {
    let (status, body, _) = request_with_headers(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        &[
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-06-18"),
        ],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = body["result"]["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block["text"].as_str())
        .ok_or_else(|| format!("tool result text: {body}"))?;
    Ok(serde_json::from_str(text)?)
}

pub(crate) async fn mcp_lines(
    app: axum::Router,
    tool: &str,
    arguments: Value,
) -> Result<Vec<Value>, TestError> {
    let (status, body, _) = request_with_headers(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        &[
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-06-18"),
        ],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["result"]["content"]
        .as_array()
        .ok_or_else(|| format!("tool result content: {body}"))?
        .iter()
        .map(|block| {
            serde_json::from_str::<Value>(block["text"].as_str().unwrap_or_default())
                .map_err(TestError::from)
        })
        .collect()
}
