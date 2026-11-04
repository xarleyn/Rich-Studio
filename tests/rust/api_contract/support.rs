use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::AppState;
use rich_studio::account_context::AccountContext;
use rich_studio::services::scheduler;
use rich_studio::settings::Settings;
use rich_studio::store::db::Store;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

pub(crate) fn merged(mut base: Value, extra: Value) -> Value {
    if let (Some(target), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    base
}

pub(crate) fn settings(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> Settings {
    let mut values = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "123:TEST".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@test_channel".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]);
    for (name, value) in extra {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn state(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> AppState {
    AppState::new(settings(dir, base, extra))
}

#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Value,
    pub(crate) raw: String,
}

#[derive(Clone)]
pub(crate) struct Telegram {
    calls: Arc<Mutex<Vec<Call>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    fail: Arc<Mutex<BTreeMap<String, (i64, String)>>>,
    drop: Arc<Mutex<BTreeMap<String, usize>>>,
    next_message_id: Arc<AtomicI64>,
}

impl Telegram {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            fail: Arc::new(Mutex::new(BTreeMap::new())),
            drop: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
        }
    }

    pub(crate) fn record(&self, method: &str, path: &str, body: &Value, raw: &str) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(Call {
                method: method.to_owned(),
                path: path.to_owned(),
                body: body.clone(),
                raw: raw.to_owned(),
            });
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    pub(crate) fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.method == method)
            .collect()
    }

    pub(crate) fn count(&self, method: &str) -> usize {
        self.calls_to(method).len()
    }

    pub(crate) fn last(&self, method: &str) -> Option<Call> {
        self.calls_to(method).pop()
    }

    pub(crate) fn clear_calls(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clear();
    }

    pub(crate) fn fail(&self, method: &str, description: &str) {
        self.fail
            .lock()
            .unwrap_or_else(|error| panic!("fail: {error}"))
            .insert(method.to_owned(), (400, description.to_owned()));
    }

    pub(crate) fn clear_fail(&self) {
        self.fail
            .lock()
            .unwrap_or_else(|error| panic!("fail: {error}"))
            .clear();
    }

    pub(crate) fn drop_responses(&self, method: &str, count: usize) {
        self.drop
            .lock()
            .unwrap_or_else(|error| panic!("drop: {error}"))
            .insert(method.to_owned(), count);
    }

    pub(crate) fn queue(&self, method: &str, responses: Vec<Value>) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .extend(responses);
    }

    pub(crate) fn take(&self, method: &str) -> Option<Value> {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(|responses| responses.pop_front())
    }

    pub(crate) fn should_drop(&self, method: &str) -> bool {
        let mut drop = self
            .drop
            .lock()
            .unwrap_or_else(|error| panic!("drop: {error}"));
        match drop.get_mut(method) {
            Some(remaining) if *remaining > 0 => {
                *remaining -= 1;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn answer(&self, method: &str, request: &Value) -> Value {
        let chat_id = request["chat_id"]
            .as_i64()
            .map_or_else(|| json!(-1001), |id| json!(id));
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        match method {
            "getMe" => json!({"id":42,"is_bot":true,"username":"studio_test_bot"}),
            "getChat" => json!({
                "id":-1001,
                "type":"channel",
                "title":"Тестовый канал",
                "username":"test_channel",
                "is_forum":false
            }),
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "getUpdates" => json!([]),
            "sendRichMessage" => json!({
                "message_id":message_id,
                "chat":{"id":chat_id},
                "rich_message":{"blocks":[]}
            }),
            "sendPoll" => json!({
                "message_id":message_id,
                "chat":{"id":chat_id},
                "poll":{"id":format!("poll{message_id}")}
            }),
            "sendLivePhoto" | "sendPaidMedia" => {
                json!({"message_id":message_id,"chat":{"id":chat_id}})
            }
            "stopPoll" => json!({
                "total_voter_count":4,
                "is_closed":true,
                "options":[
                    {"text":"Утром","voter_count":3},
                    {"text":"Вечером","voter_count":1}
                ]
            }),
            _ => json!(true),
        }
    }
}

pub(crate) async fn telegram() -> (String, Telegram) {
    let telegram = Telegram::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let server = telegram.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 16384];
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
            let raw = String::from_utf8_lossy(&data).into_owned();
            let path = raw.split(' ').nth(1).unwrap_or_default().to_owned();
            let method = path.rsplit('/').next().unwrap_or_default().to_owned();
            let body = raw
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .unwrap_or(Value::Null);
            server.record(&method, &path, &body, &raw);
            if server.should_drop(&method) {
                let _ = socket.shutdown().await;
                continue;
            }
            let failure = server
                .fail
                .lock()
                .unwrap_or_else(|error| panic!("fail: {error}"))
                .get(&method)
                .cloned();
            let body = if let Some((code, description)) = failure {
                json!({"ok":false,"error_code":code,"description":description})
            } else if let Some(response) = server.take(&method) {
                response
            } else {
                json!({"ok":true,"result":server.answer(&method,&body)})
            }
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
    (format!("http://{address}"), telegram)
}

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) value: Value,
    pub(crate) text: String,
    pub(crate) headers: HeaderMap,
}

pub(crate) async fn call_raw(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        builder = builder.header("host", "localhost");
    }
    if body.is_some()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, Body::from))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"))
        .to_vec();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        value,
        text,
        headers,
    }
}

pub(crate) async fn call(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    call_raw(
        app,
        method,
        path,
        body.map(|value| value.to_string().into_bytes()),
        headers,
    )
    .await
}

pub(crate) async fn upload(
    app: Router,
    filename: &str,
    data: &[u8],
    content_type: &str,
    headers: &[(&str, &str)],
) -> Reply {
    let boundary = "studio-contract-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let value = format!("multipart/form-data; boundary={boundary}");
    let mut all = vec![("content-type", value.as_str())];
    all.extend_from_slice(headers);
    call_raw(app, "POST", "/api/media", Some(body), &all).await
}

pub(crate) fn context(state: &AppState) -> AccountContext {
    state.account.clone()
}

pub(crate) fn set_scheduled_at(state: &AppState, id: &str, at: i64) {
    Store::connect(&context(state))
        .unwrap_or_else(|error| panic!("db: {error}"))
        .execute(
            "update posts set scheduled_at=? where id=?",
            rusqlite::params![at, id],
        )
        .unwrap_or_else(|error| panic!("schedule: {error}"));
}

pub(crate) async fn run_due(state: &AppState, now: i64) -> usize {
    scheduler::run_due(
        state,
        state.account.clone(),
        now,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
    .unwrap_or_else(|error| panic!("scheduler: {error}"))
}

pub(crate) async fn raw_server(bodies: Vec<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let mut bodies: VecDeque<Value> = bodies.into();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;
            let body = bodies.pop_front().unwrap_or_else(|| json!({})).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    format!("http://{address}")
}
