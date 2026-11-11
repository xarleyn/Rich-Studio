//! Shared fixtures for the regression cases: settings, a fake Telegram HTTP
//! server, router calls and the login/challenge helpers.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    metrics::Metrics,
    services::health,
    settings::Settings,
    store::{
        auth,
        posts::{self, NewPost},
    },
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;
const BOT_TOKEN: &str = "1:fixture";

pub(crate) fn settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@public".to_owned()),
        ("TG_PREVIEW_CHAT".to_owned(), "777".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn public_settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
        ("STUDIO_URL".to_owned(), "https://studio.example".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), BOT_TOKEN.to_owned()),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@operator_default".to_owned()),
        ("TG_PREVIEW_CHAT".to_owned(), "777".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

#[derive(Clone)]
pub(crate) struct FakeTelegram {
    pub(crate) calls: Arc<Mutex<Vec<Value>>>,
    pub(crate) queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    pub(crate) next_message_id: Arc<AtomicI64>,
    pub(crate) seen: Arc<Mutex<BTreeMap<String, usize>>>,
    pub(crate) drops: Arc<Mutex<BTreeMap<String, Vec<usize>>>>,
}

impl FakeTelegram {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
            seen: Arc::new(Mutex::new(BTreeMap::new())),
            drops: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(crate) fn record(&self, method: &str, body: &Value) {
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

    pub(crate) fn calls_to(&self, method: &str) -> Vec<Value> {
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

    /// Full Bot API envelopes answered before the mock's own reply, like `tg.queue` in the Python reference.
    pub(crate) fn queue(&self, method: &str, responses: Vec<Value>) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .extend(responses);
    }

    /// Drop the connection instead of answering the n-th call of `method` (1-based).
    pub(crate) fn drop_call(&self, method: &str, at: usize) {
        self.drops
            .lock()
            .unwrap_or_else(|error| panic!("drops: {error}"))
            .entry(method.to_owned())
            .or_default()
            .push(at);
    }

    pub(crate) fn take(&self, method: &str) -> Option<Value> {
        let at = {
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(|error| panic!("seen: {error}"));
            let counter = seen.entry(method.to_owned()).or_default();
            *counter += 1;
            *counter
        };
        if self
            .drops
            .lock()
            .unwrap_or_else(|error| panic!("drops: {error}"))
            .get(method)
            .is_some_and(|positions| positions.contains(&at))
        {
            return Some(json!({"__drop__":true}));
        }
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(|responses| responses.pop_front())
    }

    pub(crate) fn answer(&self, method: &str, request: &Value) -> Value {
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
            let queued = server.take(&method);
            if queued
                .as_ref()
                .and_then(|value| value["__drop__"].as_bool())
                == Some(true)
            {
                let _ = socket.shutdown().await;
                continue;
            }
            let body = queued
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
}

pub(crate) async fn finish(response: axum::response::Response) -> Reply {
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

pub(crate) async fn call(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
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
    finish(response).await
}

pub(crate) async fn mcp_call(app: Router, who: &str, tool: &str, arguments: Value) -> Value {
    let reply = call(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        &[("cookie", who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()["result"].clone()
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
        .unwrap_or_else(|| panic!("auth_url missing: {}", reply.text()))
        .to_owned();
    let state = url::Url::parse(&auth_url)
        .unwrap_or_else(|error| panic!("auth url: {error}"))
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("state missing"));
    let state_cookie = cookie(&reply.headers, "rs_telegram_login")
        .unwrap_or_else(|| panic!("challenge cookie missing"));
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
    let session =
        cookie(&reply.headers, "rs_session").unwrap_or_else(|| panic!("session cookie missing"));
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

pub(crate) fn result_oks(value: &Value) -> Vec<bool> {
    value["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results missing: {value}"))
        .iter()
        .filter_map(|result| result["ok"].as_bool())
        .collect()
}

pub(crate) async fn telegram_check(state: &AppState, metrics: &Metrics) -> Value {
    let spec = state.spec.read().await.clone();
    let loops = state.loop_status.read().await.clone();
    let updates = state.updates_status.read().await.clone();
    let report = health::report(
        &state.settings,
        &state.account,
        &spec,
        &state.logs,
        metrics,
        &loops,
        &updates,
    );
    report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == "telegram"))
        .cloned()
        .unwrap_or_else(|| panic!("telegram check is missing: {report}"))
}

pub(crate) fn save_sent_post(context: &AccountContext, chat: i64, message_id: i64) -> Value {
    let id = json!(chat);
    let record = posts::add(
        context,
        NewPost {
            chat_id: &id,
            payload: &blocks_post(),
            draft_id: None,
            status: "sent",
            message_id: Some(message_id),
            scheduled_at: None,
            error: None,
            kind: "",
            actor: "ui",
        },
    )
    .unwrap_or_else(|error| panic!("post: {error}"));
    posts::update_delivery(
        context,
        record["id"].as_str().unwrap_or_default(),
        Some(chat),
        None,
        None,
    )
    .unwrap_or_else(|error| panic!("delivery: {error}"));
    posts::get(context, record["id"].as_str().unwrap_or_default())
        .unwrap_or_else(|error| panic!("get post: {error}"))
        .unwrap_or_else(|| panic!("saved post is missing"))
}
