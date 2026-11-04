use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::{AppState, build_router, settings::Settings};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tower::ServiceExt as _;

use crate::fake_telegram::{FakeTelegram, fake_telegram};

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(crate) fn app(dir: &TempDir, publish: bool) -> Router {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), "http://127.0.0.1:1".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
        (
            "MCP_PUBLISH".to_owned(),
            if publish { "on" } else { "off" }.to_owned(),
        ),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    build_router(AppState::new(settings))
}
pub(crate) async fn request_with_headers(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("response body: {error}"));
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
pub(crate) async fn request(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    request_with_headers(app, method, path, body, &[]).await
}
pub(crate) async fn request_raw(
    app: Router,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("response body: {error}"));
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
pub(crate) async fn rpc(app: Router, method: &str, params: Value) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":9,"method":method,"params":params})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

pub(crate) fn blocks_post() -> Value {
    json!({
        "rich_message": {
            "blocks": [
                {"type": "heading", "size": 1, "text": "Тест"},
                {"type": "paragraph", "text": ["Привет, ", {"type": "bold", "text": "мир"}]}
            ]
        }
    })
}

pub(crate) fn settings_with(dir: &TempDir, values: &[(&str, &str)]) -> Settings {
    let mut map = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]);
    for (name, value) in values {
        map.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&map).unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn fake_state(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> AppState {
    let mut values = vec![
        ("TG_API_BASE", base),
        ("TG_BOT_TOKEN", "123:TEST"),
        ("TG_DEFAULT_CHAT", "@test_channel"),
        ("TG_PREVIEW_CHAT", "777"),
        ("TG_UPDATES", "off"),
        ("MCP_PUBLISH", "on"),
    ];
    values.extend_from_slice(extra);
    AppState::new(settings_with(dir, &values))
}

pub(crate) async fn studio_with_fake(
    dir: &TempDir,
    extra: &[(&str, &str)],
) -> (String, FakeTelegram, AppState, Router) {
    let (base, fake) = fake_telegram().await;
    let state = fake_state(dir, &base, extra);
    let app = build_router(state.clone());
    (base, fake, state, app)
}

pub(crate) async fn call(app: Router, tool: &str, arguments: Value) -> (Value, Value, String) {
    let result = rpc(
        app,
        "tools/call",
        json!({"name":tool,"arguments":arguments}),
    )
    .await["result"]
        .clone();
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if result["isError"].as_bool() == Some(true) {
        return (result, Value::Null, text);
    }
    let data = serde_json::from_str(&text).unwrap_or(Value::Null);
    (result, data, text)
}

pub(crate) async fn call_ok(app: Router, tool: &str, arguments: Value) -> Value {
    let (result, data, text) = call(app, tool, arguments).await;
    assert_eq!(result["isError"], false, "{tool}: {text}");
    data
}

pub(crate) async fn call_fail(app: Router, tool: &str, arguments: Value) -> String {
    let (result, _, text) = call(app, tool, arguments).await;
    assert_eq!(result["isError"], true, "{tool}: {text}");
    text
}

pub(crate) async fn call_list(app: Router, tool: &str, arguments: Value) -> Vec<Value> {
    let result = rpc(
        app,
        "tools/call",
        json!({"name":tool,"arguments":arguments}),
    )
    .await["result"]
        .clone();
    assert_eq!(result["isError"], false, "{tool}: {result}");
    result["content"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|block| {
            serde_json::from_str(block["text"].as_str().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

pub(crate) fn media_names(state: &AppState) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&state.account.media_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

pub(crate) async fn upload_file(
    app: Router,
    name: &str,
    data: &[u8],
    content_type: &str,
) -> (StatusCode, Value) {
    let boundary = "studio-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    request_raw(
        app,
        "POST",
        "/api/media",
        &format!("multipart/form-data; boundary={boundary}"),
        body,
    )
    .await
}

pub(crate) async fn file_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if data.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&data);
            let path = request
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let response = match path.as_str() {
                "/to-inside" => {
                    b"HTTP/1.1 302 Found\r\nLocation: http://intranet/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                }
                "/hop" => {
                    b"HTTP/1.1 302 Found\r\nLocation: /hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                }
                _ => {
                    let body = vec![b'x'; 3 * 1024 * 1024];
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&body);
                    response
                }
            };
            if socket.write_all(&response).await.is_err() {
                break;
            }
        }
    });
    format!("http://{address}")
}
