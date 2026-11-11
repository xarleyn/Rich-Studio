use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

pub(crate) fn post() -> Value {
    json!({"rich_message":{"markdown":"Привет, ё 🚀","future":{"big":9007199254740993u64}},"options":{"future_option":{"keep":true}}})
}

pub(crate) fn context(dir: &TempDir) -> AccountContext {
    AccountContext::root(&settings(dir, ""))
}

pub(crate) fn settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@public".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) enum Reply {
    Json(u16, Value),
    Drop,
}

pub(crate) fn message(payload: &Value, id: i64) -> Value {
    json!({"message_id":id,"chat":{"id":payload["chat_id"].clone()},"rich_message":{"blocks":[]}})
}

pub(crate) async fn mock<F>(handler: F) -> (String, mpsc::UnboundedReceiver<(String, Value)>)
where
    F: Fn(usize, &str, &Value) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let (send, receive) = mpsc::unbounded_channel();
    let handler = std::sync::Arc::new(handler);
    tokio::spawn(async move {
        let mut count = 0usize;
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            count += 1;
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            let method = request
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            let payload = request
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|body| serde_json::from_str(body).ok())
                .unwrap_or_else(|| json!({}));
            let _ = send.send((method.clone(), payload.clone()));
            match handler(count, &method, &payload) {
                Reply::Drop => {
                    let _ = socket.shutdown().await;
                }
                Reply::Json(status, value) => {
                    let body = value.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} ERROR\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    (format!("http://{address}"), receive)
}

pub(crate) fn ok(result: Value) -> Reply {
    Reply::Json(200, json!({"ok":true,"result":result}))
}

pub(crate) fn refused(description: &str) -> Reply {
    Reply::Json(
        400,
        json!({"ok":false,"error_code":400,"description":description}),
    )
}

pub(crate) async fn request(
    app: Router,
    uri: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}")),
    )
}

pub(crate) fn sent_count(
    requests: &mut mpsc::UnboundedReceiver<(String, Value)>,
    method: &str,
) -> usize {
    let mut count = 0;
    while let Ok((name, _)) = requests.try_recv() {
        if name == method {
            count += 1;
        }
    }
    count
}

pub(crate) fn sent_payloads(
    requests: &mut mpsc::UnboundedReceiver<(String, Value)>,
    method: &str,
) -> Vec<Value> {
    let mut payloads = Vec::new();
    while let Ok((name, payload)) = requests.try_recv() {
        if name == method {
            payloads.push(payload);
        }
    }
    payloads
}

pub(crate) fn last_payload(
    requests: &mut mpsc::UnboundedReceiver<(String, Value)>,
    method: &str,
) -> Option<Value> {
    sent_payloads(requests, method).pop()
}

pub(crate) async fn request_as(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
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
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
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

pub(crate) async fn upload(
    app: Router,
    filename: &str,
    data: &[u8],
    content_type: &str,
) -> (StatusCode, Value) {
    let boundary = "studio-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .header("host", "localhost")
        .method("POST")
        .uri("/api/media")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap_or_else(|error| panic!("upload request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("upload response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("upload body: {error}"));
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("upload json {bytes:?}: {error}"));
    (status, value)
}

pub(crate) fn set_scheduled_at(dir: &TempDir, id: &str, at: i64) {
    let connection = rich_studio::store::db::Store::connect(&context(dir))
        .unwrap_or_else(|error| panic!("db: {error}"));
    connection
        .execute(
            "update posts set scheduled_at=? where id=?",
            rusqlite::params![at, id],
        )
        .unwrap_or_else(|error| panic!("schedule update: {error}"));
}

pub(crate) async fn schedule(app: Router, body: Value) -> (StatusCode, Value) {
    request(app, "/api/schedule", body, &[]).await
}

pub(crate) fn callback_update(chat: i64, message_id: i64, data: &str) -> Value {
    json!({"update_id":1,"callback_query":{"id":"cq","data":data,"message":{"message_id":message_id,"chat":{"id":chat}}}})
}
