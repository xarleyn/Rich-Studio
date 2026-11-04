use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::settings::Settings;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(crate) fn settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@main".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

#[derive(Clone)]
pub(crate) struct FakeTelegram {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<serde_json::Value>>>>,
    next_message_id: Arc<AtomicI64>,
}

impl FakeTelegram {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
        }
    }

    fn record(&self, method: &str, body: &serde_json::Value) {
        if method == "getUpdates" {
            return;
        }
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(json!({"method":method,"body":body}));
    }

    pub(crate) fn calls_to(&self, method: &str) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .iter()
            .filter(|call| call["method"].as_str() == Some(method))
            .cloned()
            .collect()
    }

    pub(crate) fn last(&self, method: &str) -> Option<serde_json::Value> {
        self.calls_to(method).pop()
    }

    /// Full Bot API envelopes answered before the mock's own reply, like `tg.queue` in the Python reference.
    pub(crate) fn queue(&self, method: &str, responses: Vec<serde_json::Value>) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .extend(responses);
    }

    fn take(&self, method: &str) -> Option<serde_json::Value> {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(|responses| responses.pop_front())
    }

    fn answer(&self, method: &str, request: &serde_json::Value) -> serde_json::Value {
        match method {
            "getMe" => json!({"id":42,"username":"studio_bot"}),
            "getChat" => {
                json!({"id":-1002627012139i64,"type":"supergroup","title":"МОИ ЗАМЕТКИ","is_forum":true})
            }
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "sendRichMessage" | "sendPoll" | "sendMessage" => {
                let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                let mut result =
                    json!({"message_id":message_id,"chat":{"id":request["chat_id"].clone()}});
                if method == "sendRichMessage" {
                    result["rich_message"] = json!({"blocks":[]});
                }
                if method == "sendPoll" {
                    result["poll"] = json!({"id":format!("poll{message_id}")});
                }
                result
            }
            _ => json!(true),
        }
    }
}

pub(crate) async fn fake_server() -> (String, FakeTelegram) {
    let fake = FakeTelegram::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let server = fake.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap_or(0);
                if count == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..count]);
                if data.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data).to_lowercase();
                    let length = headers
                        .split("content-length:")
                        .nth(1)
                        .and_then(|tail| tail.lines().next())
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let end = data
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap_or(0)
                        + 4;
                    if data.len() >= end + length {
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
                .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                .unwrap_or_else(|| json!({}));
            server.record(&method, &payload);
            let body = server
                .take(&method)
                .unwrap_or_else(|| json!({"ok":true,"result":server.answer(&method,&payload)}))
                .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    (format!("http://{address}"), fake)
}

pub(crate) async fn chat_server() -> String {
    fake_server().await.0
}

pub(crate) fn blocks_post() -> serde_json::Value {
    json!({"rich_message": {"blocks": [{"type": "paragraph", "text": "Привет"}]}})
}

pub(crate) async fn http_request(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt as _;

    let mut builder = Request::builder()
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
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
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json: {error}")),
    )
}

pub(crate) fn detail(value: &Value) -> &str {
    value["detail"].as_str().unwrap_or_default()
}
