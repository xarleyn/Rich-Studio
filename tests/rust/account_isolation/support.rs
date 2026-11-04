use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::settings::Settings;
use rich_studio::store::auth;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(crate) fn config(data_dir: &Path, extra: &[(&str, &str)]) -> Settings {
    let mut values = BTreeMap::from([(
        "DATA_DIR".to_owned(),
        data_dir.to_string_lossy().into_owned(),
    )]);
    for (name, value) in extra {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("test settings: {error}"))
}

pub(crate) const BOT_TOKEN: &str = "1:fixture";

pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

pub(crate) fn public_config(dir: &TempDir, base: &str) -> Settings {
    config(
        dir.path(),
        &[
            ("STUDIO_MODE", "server"),
            ("STUDIO_PUBLIC", "on"),
            ("STUDIO_URL", "https://studio.example"),
            ("TG_BOT_TOKEN", BOT_TOKEN),
            ("TG_API_BASE", base),
            ("TG_DEFAULT_CHAT", "@operator_default"),
            ("TG_PREVIEW_CHAT", "777"),
            ("TG_UPDATES", "off"),
            ("STUDIO_BACKGROUND", "off"),
            ("SPEC_PIN", "on"),
        ],
    )
}

#[derive(Clone)]
pub(crate) struct FakeTelegram {
    calls: Arc<Mutex<Vec<Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
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

    fn record(&self, method: &str, body: &Value) {
        if method == "getUpdates" {
            return;
        }
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(json!({"method":method,"body":body}));
    }

    pub(crate) fn clear_calls(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clear();
    }

    fn calls_to(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .iter()
            .filter(|call| call["method"].as_str() == Some(method))
            .cloned()
            .collect()
    }

    pub(crate) fn count(&self, method: &str) -> usize {
        self.calls_to(method).len()
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .iter()
            .filter_map(|call| call["method"].as_str().map(str::to_owned))
            .collect()
    }

    pub(crate) fn last(&self, method: &str) -> Value {
        self.calls_to(method)
            .pop()
            .unwrap_or_else(|| panic!("no {method} call"))
    }

    /// Full Bot API envelopes answered before the mock's own reply, like `tg.queue` in the Python reference.
    pub(crate) fn queue(&self, method: &str, responses: Vec<Value>) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .extend(responses);
    }

    fn take(&self, method: &str) -> Option<Value> {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(|responses| responses.pop_front())
    }

    fn answer(&self, method: &str, request: &Value) -> Value {
        match method {
            "getMe" => json!({"id":42,"is_bot":true,"username":"studio_test_bot"}),
            "getChat" => json!({"id":-1001,"type":"channel","title":"Тест"}),
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "sendRichMessage"
            | "sendPoll"
            | "sendLivePhoto"
            | "sendPaidMedia"
            | "sendMessage"
            | "editMessageText"
            | "editMessageReplyMarkup" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                let mut result = json!({
                    "message_id":id,
                    "chat":{"id":request["chat_id"].clone()},
                    "date":auth::now()
                });
                if method == "sendRichMessage" {
                    result["rich_message"] = json!({"blocks":[{"type":"paragraph","text":"ok"}]});
                }
                if method == "sendPoll" {
                    result["poll"] =
                        json!({"id":format!("poll{id}"),"question":request["question"]});
                }
                if method == "sendMessage" {
                    result["text"] = request["text"].clone();
                }
                result
            }
            _ => json!(true),
        }
    }
}

pub(crate) fn ok_envelope(result: Value) -> Value {
    json!({"ok":true,"result":result})
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
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .unwrap_or_else(|| json!({}));
            server.record(&method, &payload);
            let body = server
                .take(&method)
                .unwrap_or_else(|| ok_envelope(server.answer(&method, &payload)))
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

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) bytes: Vec<u8>,
}

impl Reply {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    pub(crate) fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }
}

async fn finish(response: axum::response::Response) -> Reply {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"))
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

pub(crate) async fn call_on(
    app: Router,
    method: &str,
    uri: &str,
    host: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    finish(response).await
}

pub(crate) async fn call(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    call_on(app, method, uri, "localhost", body, headers).await
}

pub(crate) async fn upload(app: Router, who: &str, filename: &str, data: &[u8]) -> Reply {
    let boundary = "studio-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/api/media")
        .header("host", "localhost")
        .header("cookie", who)
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
    finish(response).await
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()
                .and_then(|pair| pair.trim().strip_prefix(&prefix))
                .map(str::to_owned)
        })
}

pub(crate) fn set_cookie_has(headers: &HeaderMap, flag: &str) -> bool {
    let wanted = flag.to_ascii_lowercase();
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains(&wanted))
}

pub(crate) fn login_fields(uid: i64, auth_date: i64) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("id".to_owned(), uid.to_string()),
        ("first_name".to_owned(), "Пользователь".to_owned()),
        ("auth_date".to_owned(), auth_date.to_string()),
    ])
}

pub(crate) fn signed_hash(fields: &BTreeMap<String, String>) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest as _, Sha256};
    let check = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&Sha256::digest(BOT_TOKEN.as_bytes()))
        .unwrap_or_else(|error| panic!("hmac: {error}"));
    mac.update(check.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn query_path(fields: &BTreeMap<String, String>, state: &str) -> String {
    let mut fields = fields.clone();
    fields.insert("state".to_owned(), state.to_owned());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    format!("/auth/telegram/callback?{query}")
}

pub(crate) fn callback_path(state: &str, fields: &BTreeMap<String, String>) -> String {
    let mut signed = fields.clone();
    signed.insert("hash".to_owned(), signed_hash(fields));
    query_path(&signed, state)
}

pub(crate) async fn start_challenge(app: &Router) -> (String, String) {
    let reply = call(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let auth_url = reply.json()["auth_url"]
        .as_str()
        .unwrap_or_else(|| panic!("auth_url"))
        .to_owned();
    let state = url::Url::parse(&auth_url)
        .unwrap_or_else(|error| panic!("auth url: {error}"))
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("state"));
    let state_cookie =
        cookie(&reply.headers, "rs_telegram_login").unwrap_or_else(|| panic!("challenge cookie"));
    (state, state_cookie)
}

pub(crate) async fn login_as(app: &Router, uid: i64) -> String {
    let (state, state_cookie) = start_challenge(app).await;
    let path = callback_path(&state, &login_fields(uid, auth::now()));
    let state_header = format!("rs_telegram_login={state_cookie}");
    let reply = call(
        app.clone(),
        "GET",
        &path,
        None,
        &[("cookie", state_header.as_str())],
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    let session = cookie(&reply.headers, "rs_session").unwrap_or_else(|| panic!("session cookie"));
    format!("rs_session={session}")
}

pub(crate) async fn connect_channel(app: &Router, who: &str) -> Value {
    let reply = call(
        app.clone(),
        "POST",
        "/api/channels",
        Some(json!({"chat_id":"@mine"})),
        &[("cookie", who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()["channel"].clone()
}
