//! Regression scenarios ported from the Python reference `tests/test_regressions.py`.
//!
//! Each case protects a bug that was once visible to a user. Keep new cases
//! close to the behaviour they guard instead of grouping them by implementation.
//!
//! Cases already covered elsewhere are mapped instead of duplicated:
//! - `test_favicon_is_not_a_404` → `public_site::icons_discovery_head_and_fingerprint_cache_are_usable`
//! - `test_comment_exception_restores_normal_transport_policy` (refused path) →
//!   `events::reply_transport_failure_restores_account_policy`; the successful path is
//!   kept here as `linked_comment_does_not_change_transport_policy`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::errors::{ServiceError, StoreError};
use rich_studio::metrics::Metrics;
use rich_studio::services::health;
use rich_studio::services::idempotency::{Idempotency, STALE_PENDING};
use rich_studio::services::{accounts, scheduler, updates};
use rich_studio::settings::Settings;
use rich_studio::store::db::{self, Store};
use rich_studio::store::{auth, channels, drafts, editing, invite_links, posts, posts::NewPost};
use rich_studio::telegram::TelegramError;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const BOT_TOKEN: &str = "1:fixture";

fn settings(dir: &TempDir, base: &str) -> Settings {
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

fn public_settings(dir: &TempDir, base: &str) -> Settings {
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

fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

#[derive(Clone)]
struct FakeTelegram {
    calls: Arc<Mutex<Vec<Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    next_message_id: Arc<AtomicI64>,
    seen: Arc<Mutex<BTreeMap<String, usize>>>,
    drops: Arc<Mutex<BTreeMap<String, Vec<usize>>>>,
}

impl FakeTelegram {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
            seen: Arc::new(Mutex::new(BTreeMap::new())),
            drops: Arc::new(Mutex::new(BTreeMap::new())),
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

    fn clear_calls(&self) {
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

    fn count(&self, method: &str) -> usize {
        self.calls_to(method).len()
    }

    /// Full Bot API envelopes answered before the mock's own reply, like `tg.queue` in the Python reference.
    fn queue(&self, method: &str, responses: Vec<Value>) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .extend(responses);
    }

    /// Drop the connection instead of answering the n-th call of `method` (1-based).
    fn drop_call(&self, method: &str, at: usize) {
        self.drops
            .lock()
            .unwrap_or_else(|error| panic!("drops: {error}"))
            .entry(method.to_owned())
            .or_default()
            .push(at);
    }

    fn take(&self, method: &str) -> Option<Value> {
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

fn ok_envelope(result: Value) -> Value {
    json!({"ok":true,"result":result})
}

async fn fake_server() -> (String, FakeTelegram) {
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

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
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

async fn call(
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

async fn mcp_call(app: Router, who: &str, tool: &str, arguments: Value) -> Value {
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

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
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

fn login_fields(uid: i64, auth_date: i64) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("id".to_owned(), uid.to_string()),
        ("first_name".to_owned(), "Пользователь".to_owned()),
        ("auth_date".to_owned(), auth_date.to_string()),
    ])
}

fn signed_hash(fields: &BTreeMap<String, String>) -> String {
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

fn query_path(fields: &BTreeMap<String, String>, state: &str) -> String {
    let mut fields = fields.clone();
    fields.insert("state".to_owned(), state.to_owned());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    format!("/auth/telegram/callback?{query}")
}

fn callback_path(state: &str, fields: &BTreeMap<String, String>) -> String {
    let mut signed = fields.clone();
    signed.insert("hash".to_owned(), signed_hash(fields));
    query_path(&signed, state)
}

async fn start_challenge(app: &Router) -> (String, String) {
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

async fn login_as(app: &Router, uid: i64) -> String {
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

async fn connect_channel(app: &Router, who: &str) -> Value {
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

fn result_oks(value: &Value) -> Vec<bool> {
    value["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results missing: {value}"))
        .iter()
        .filter_map(|result| result["ok"].as_bool())
        .collect()
}

async fn telegram_check(state: &AppState, metrics: &Metrics) -> Value {
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

fn save_sent_post(context: &AccountContext, chat: i64, message_id: i64) -> Value {
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

#[tokio::test]
async fn preview_never_falls_back_to_the_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = settings(&dir, &base);
    config.preview_chat.clear();
    let app = build_router(AppState::new(config));
    let reply = call(
        app,
        "POST",
        "/api/preview-send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(
        reply.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("TG_PREVIEW_CHAT")
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn scheduled_post_stays_sent_when_pin_hits_network_error() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("pinChatMessage", 1);
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let mut content = blocks_post();
    content["pin"] = json!(true);
    let reply = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":content,"at":scheduler::now()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let id = reply.json()["id"]
        .as_str()
        .unwrap_or_else(|| panic!("scheduled id: {}", reply.text()))
        .to_owned();
    let sent = scheduler::run_due(
        &state,
        state.account.clone(),
        scheduler::now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    assert_eq!(sent, 1);
    let record = posts::get(&state.account, &id)?.unwrap_or_else(|| panic!("post {id}"));
    assert_eq!(record["status"], "sent", "{record}");
    assert!(record["message_id"].as_i64().is_some(), "{record}");
    assert!(
        record["error"]
            .as_str()
            .unwrap_or_default()
            .contains("закрепить"),
        "{record}"
    );
    Ok(())
}

#[tokio::test]
async fn network_error_in_one_channel_does_not_stop_the_rest() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("sendRichMessage", 2);
    let app = build_router(AppState::new(settings(&dir, &base)));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b","@c"]})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(result_oks(&reply.json()), vec![true, false, true]);
    assert_eq!(fake.count("sendRichMessage"), 3);
    Ok(())
}

#[tokio::test]
async fn single_channel_network_error_is_a_clear_502() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("sendRichMessage", 1);
    let app = build_router(AppState::new(settings(&dir, &base)));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text());
    assert!(
        reply.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("мог уйти")
    );
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn scheduler_reports_missing_chat_without_scaring() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = settings(&dir, &base);
    config.default_chat.clear();
    let state = AppState::new(config);
    let context = state.account.clone();
    let chat = json!("");
    let record = posts::add(
        &context,
        NewPost {
            chat_id: &chat,
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(scheduler::now()),
            error: None,
            kind: "rich",
            actor: "ui",
        },
    )?;
    let id = record["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(
        scheduler::run_due(
            &state,
            context.clone(),
            scheduler::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let record = posts::get(&context, &id)?.unwrap_or_else(|| panic!("post {id}"));
    assert_eq!(record["status"], "failed", "{record}");
    let error = record["error"].as_str().unwrap_or_default();
    assert!(error.contains("TG_DEFAULT_CHAT"), "{error}");
    assert!(!error.contains("мог уйти"), "{error}");
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn healthcheck_works_behind_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, "http://127.0.0.1:9");
    config.api_key = "secret".to_owned();
    let app = build_router(AppState::new(config));
    assert_eq!(
        call(app.clone(), "GET", "/healthz", None, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/config", None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn poll_is_checked_like_a_poll_after_a_post() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let poll = json!({"question":"Какой?","options":(0..13).map(|i| format!("вариант {i}")).collect::<Vec<_>>()});
    for body in [
        json!({"poll":poll}),
        json!({"poll":poll,"at":scheduler::now()+3600}),
    ] {
        let reply = call(app.clone(), "POST", "/api/poll", Some(body), &[]).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text()
        );
        assert!(
            reply.text().contains("больше 12 вариантов"),
            "{}",
            reply.text()
        );
    }
    assert!(posts::list(&state.account)?.is_empty());
    assert_eq!(fake.count("sendPoll"), 0);
    Ok(())
}

#[tokio::test]
async fn removed_admin_receives_no_new_private_topics() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let state = AppState::new(public_settings(&dir, &base));
    let app = build_router(state.clone());
    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(
            json!({"id":-1001,"type":"supergroup","is_forum":true}),
        )],
    );
    connect_channel(&app, &who).await;
    fake.queue("getChatMember", vec![ok_envelope(json!({"status":"left"}))]);
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"message":{"chat":{"id":-1001},"message_thread_id":77,"forum_topic_created":{"name":"Private new topic"}}}),
        )
        .await?,
        Some("topic")
    );
    let listed = call(
        app,
        "GET",
        "/api/channels",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(listed[0]["topics"], json!([]));
    Ok(())
}

#[tokio::test]
async fn first_comment_requires_current_source_rights_and_linked_discussion() -> TestResult {
    for (allowed, linked) in [(false, true), (true, false), (true, true)] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let mut config = public_settings(&dir, &base);
        config.updates_on = true;
        let state = AppState::new(config);
        let app = build_router(state.clone());
        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        let mut post = blocks_post();
        post["comment"] = json!({"html":"<p>First comment</p>"});
        let sent = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"chat_id":"-1001","post":post})),
            &[("cookie", who.as_str())],
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
        let saved = sent.json()["post"].clone();
        fake.queue(
            "getChatMember",
            vec![ok_envelope(
                json!({"status":if allowed {"creator"} else {"left"}}),
            )],
        );
        fake.queue(
            "getChat",
            vec![ok_envelope(json!({
                "id":-1001,
                "type":"channel",
                "linked_chat_id":if linked {-1002} else {-1003},
            }))],
        );
        fake.clear_calls();
        assert_eq!(
            updates::handle_update(
                &state,
                &json!({
                    "message":{
                        "chat":{"id":-1002},
                        "message_id":900,
                        "is_automatic_forward":true,
                        "forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":saved["message_id"]},
                    }
                }),
            )
            .await?,
            Some("comment")
        );
        assert_eq!(
            fake.count("sendRichMessage"),
            usize::from(allowed && linked),
            "allowed={allowed} linked={linked}"
        );
        let result = call(
            app,
            "GET",
            &format!("/api/posts/{}", saved["id"].as_str().unwrap_or_default()),
            None,
            &[("cookie", who.as_str())],
        )
        .await
        .json();
        assert_eq!(
            result["comment_status"],
            if allowed && linked { "sent" } else { "failed" },
            "allowed={allowed} linked={linked}: {result}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn public_database_growth_is_bounded_and_accounts_are_independent() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let app = build_router(AppState::new(config.clone()));
    let who = login_as(&app, 101).await;
    let payload = json!({"rich_message":{"html":"x".repeat(200_000)}});
    let mut statuses = Vec::new();
    for index in 0..8 {
        let reply = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"title":index.to_string(),"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await;
        statuses.push(reply.status);
    }
    assert!(
        statuses.contains(&StatusCode::PAYLOAD_TOO_LARGE),
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|status| [StatusCode::OK, StatusCode::PAYLOAD_TOO_LARGE].contains(status)),
        "{statuses:?}"
    );
    let context = AccountContext::account(&config, 101, Role::Publish);
    let connection = Store::connect(&context)?;
    let pages: i64 = connection.query_row("pragma page_count", [], |row| row.get(0))?;
    let page_size: i64 = connection.query_row("pragma page_size", [], |row| row.get(0))?;
    assert!(pages * page_size <= 1024 * 1024, "{pages} x {page_size}");
    let other = login_as(&app, 202).await;
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", other.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn revoked_account_does_not_receive_reactions_or_block_other_admin() -> TestResult {
    for status in ["left", "member", "administrator", "unavailable"] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        let first = login_as(&app, 101).await;
        let second = login_as(&app, 202).await;
        let mut records = Vec::new();
        for uid in [101, 202] {
            let context = AccountContext::account(&config, uid, Role::Publish);
            channels::save(
                &context,
                &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel"}),
            )?;
            records.push(save_sent_post(&context, -1001, 77));
        }
        fake.queue(
            "getChatMember",
            vec![
                if status == "unavailable" {
                    json!({"ok":false,"error_code":403,"description":"Forbidden"})
                } else {
                    ok_envelope(json!({"status":status,"can_post_messages":false}))
                },
                ok_envelope(json!({"status":"creator"})),
            ],
        );
        assert_eq!(
            updates::handle_update(
                &state,
                &json!({
                    "message_reaction_count":{
                        "chat":{"id":-1001},
                        "message_id":77,
                        "reactions":[{"type":{"type":"emoji","emoji":"👍"},"total_count":3}],
                    }
                }),
            )
            .await?,
            Some("reactions")
        );
        let first_post = call(
            app.clone(),
            "GET",
            &format!(
                "/api/posts/{}",
                records[0]["id"].as_str().unwrap_or_default()
            ),
            None,
            &[("cookie", first.as_str())],
        )
        .await
        .json();
        assert!(
            first_post["reactions"].is_null()
                || first_post["reactions"]
                    .as_object()
                    .is_some_and(|value| value.is_empty()),
            "{status}: {first_post}"
        );
        let second_post = call(
            app,
            "GET",
            &format!(
                "/api/posts/{}",
                records[1]["id"].as_str().unwrap_or_default()
            ),
            None,
            &[("cookie", second.as_str())],
        )
        .await
        .json();
        assert_eq!(second_post["reactions"], json!({"👍":3}), "{status}");
    }
    Ok(())
}

#[tokio::test]
async fn database_full_after_a_send_never_frees_idempotency_key() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let state = AppState::new(config.clone());
    let app = build_router(state.clone());
    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    let connection = Store::connect(&context)?;
    connection.execute_batch(
        "create table acceptance_audit(payload blob);
         create trigger full_after_send before update of status on posts
         when NEW.status='sent' begin
           insert into acceptance_audit values(zeroblob(2097152));
         end;",
    )?;
    drop(connection);
    let headers = [
        ("cookie", who.as_str()),
        ("idempotency-key", "quota-after-send"),
    ];
    let body = json!({"chat_id":"-1001","post":blocks_post()});
    let first = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(body.clone()),
        &headers,
    )
    .await;
    assert_eq!(
        first.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        first.text()
    );
    Store::connect(&context)?.execute_batch("drop trigger full_after_send")?;
    let retried = call(app, "POST", "/api/send", Some(body), &headers).await;
    assert_eq!(
        retried.status,
        StatusCode::BAD_GATEWAY,
        "{}",
        retried.text()
    );
    assert!(retried.text().contains("мог уйти"), "{}", retried.text());
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn draft_count_limit_serializes_writers_and_allows_cleanup() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let config = public_settings(&dir, &base);
    let app = build_router(AppState::new(config.clone()));
    let who = login_as(&app, 101).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    Store::connect(&context)?.execute(
        "with recursive seq(n) as (select 1 union all select n+1 from seq where n<998)
         insert into drafts(id,title,payload,created_at,updated_at)
         select 'seed-'||n,'','{}',0,0 from seq",
        [],
    )?;
    let mut tasks = Vec::new();
    for index in 0..8 {
        let app = app.clone();
        let who = who.clone();
        tasks.push(tokio::spawn(async move {
            call(
                app,
                "POST",
                "/api/drafts",
                Some(json!({"title":index.to_string(),"post":blocks_post()})),
                &[("cookie", who.as_str())],
            )
            .await
        }));
    }
    let mut statuses = Vec::new();
    let mut created = Vec::new();
    for task in tasks {
        let reply = task.await?;
        statuses.push(reply.status);
        if reply.status == StatusCode::OK {
            created.push(reply.json()["id"].as_str().unwrap_or_default().to_owned());
        }
    }
    statuses.sort_by_key(|status| status.as_u16());
    let mut expected = vec![StatusCode::OK; 2];
    expected.extend(std::iter::repeat_n(StatusCode::PAYLOAD_TOO_LARGE, 6));
    assert_eq!(statuses, expected);
    let did = created
        .first()
        .unwrap_or_else(|| panic!("no created draft"));
    assert_eq!(
        call(
            app.clone(),
            "PUT",
            &format!("/api/drafts/{did}"),
            Some(json!({"post":blocks_post()})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/drafts/{did}"),
            None,
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"title":"9","post":blocks_post()})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn public_draft_byte_limit_covers_all_stored_fields() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let app = build_router(AppState::new(public_settings(&dir, &base)));
    let who = login_as(&app, 101).await;
    let huge = "a".repeat(1024 * 1024 + 1);
    for field in ["title", "post", "post_id", "chat_ids"] {
        let value = match field {
            "post" => json!({"rich_message":{"html":huge}}),
            "chat_ids" => json!([format!("@{huge}")]),
            _ => json!(huge),
        };
        let reply = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"post":blocks_post(),field:value})),
            &[("cookie", who.as_str())],
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{field}: {}",
            reply.text()
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_cannot_bypass_public_database_quota() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let app = build_router(AppState::new(config));
    let who = login_as(&app, 101).await;
    let payload = json!({"rich_message":{"html":"x".repeat(200_000)}});
    for _ in 0..8 {
        let _ = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await;
    }
    let result = mcp_call(
        app.clone(),
        &who,
        "save_draft",
        json!({"title":"Over quota","post":payload}),
    )
    .await;
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.to_string().contains("Лимит базы"), "{result}");
    let listed = call(
        app.clone(),
        "GET",
        "/api/drafts",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    for draft in listed.as_array().cloned().unwrap_or_default() {
        let id = draft["id"].as_str().unwrap_or_default();
        assert_eq!(
            call(
                app.clone(),
                "DELETE",
                &format!("/api/drafts/{id}"),
                None,
                &[("cookie", who.as_str())],
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn database_quota_also_limits_post_history_and_preserves_operator() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    let post = save_sent_post(&context, -1001, 77);
    let id = post["id"].as_str().unwrap_or_default();
    let large = json!({"html":"x".repeat(200_000)});
    let mut full = false;
    for _ in 0..8 {
        let taken = editing::claim_version(&context, id, None)?
            .unwrap_or_else(|| panic!("post version is missing"));
        match editing::replace_sent(
            &context,
            id,
            &json!({"rich_message":{"markdown":"next"}}),
            &large,
            "ui",
            taken,
        ) {
            Ok(_) => {}
            Err(StoreError::StorageFull) => {
                full = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert!(full, "post history did not hit the account quota");
    let root = AccountContext::root(&config);
    let mut root_connection = Store::connect(&root)?;
    db::migrate(&mut root_connection)?;
    drop(root_connection);
    for index in 0..8 {
        drafts::save(&root, None, "Operator", &large, None, None, None)?
            .unwrap_or_else(|| panic!("operator draft {index} is missing"));
    }
    Ok(())
}

#[tokio::test]
async fn health_error_order_does_not_depend_on_clock_resolution() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, "http://127.0.0.1:9");
    config.updates_on = true;
    config.background = false;
    let state = AppState::new(config);
    let metrics = Metrics::default();
    metrics.telegram_call("getMe", true, None, 0, None, false);
    metrics.telegram_call("getUpdates", false, Some("ConnectError"), 0, None, true);
    assert_eq!(telegram_check(&state, &metrics).await["status"], "warn");
    metrics.telegram_call("getUpdates", true, None, 0, None, true);
    assert_eq!(telegram_check(&state, &metrics).await["status"], "ok");
    Ok(())
}

#[tokio::test]
async fn lowered_database_quota_keeps_existing_data_readable_and_deletable() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, "http://127.0.0.1:9");
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    let mut saved = Vec::new();
    for index in 0..8 {
        saved.push(
            drafts::save(
                &context,
                None,
                &index.to_string(),
                &json!({"html":"x".repeat(200_000)}),
                None,
                None,
                None,
            )?
            .unwrap_or_else(|| panic!("draft {index} is missing")),
        );
    }
    let mut strict_config = config.clone();
    strict_config.account_database_mb = 1.0;
    let strict = AccountContext::account(&strict_config, 101, Role::Publish);
    let id = saved[0]["id"].as_str().unwrap_or_default();
    assert_eq!(drafts::get(&strict, id)?, Some(saved[0].clone()));
    let error = drafts::save(
        &strict,
        None,
        "No further growth",
        &json!({"html":"y".repeat(200_000)}),
        None,
        None,
        None,
    )
    .err()
    .unwrap_or_else(|| panic!("growth was accepted past the lowered quota"));
    assert!(matches!(error, StoreError::StorageFull), "{error}");
    drafts::delete(&strict, id)?;
    assert!(
        drafts::save(
            &strict,
            None,
            "Reuses freed pages",
            &json!({"html":"z".repeat(100_000)}),
            None,
            None,
            None,
        )?
        .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn revoked_account_does_not_receive_poll_or_invite_updates() -> TestResult {
    for event in ["poll", "invite"] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        login_as(&app, 101).await;
        let context = AccountContext::account(&config, 101, Role::Publish);
        channels::save(
            &context,
            &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel"}),
        )?;
        let post = save_sent_post(&context, -1001, 77);
        posts::update_delivery(
            &context,
            post["id"].as_str().unwrap_or_default(),
            Some(-1001),
            Some("private-poll"),
            None,
        )?;
        invite_links::save(&context, "test-invite", "-1001", "ui", &json!({}))?;
        fake.queue("getChatMember", vec![ok_envelope(json!({"status":"left"}))]);
        let update = if event == "poll" {
            json!({"poll":{"id":"private-poll","total_voter_count":3,"options":[]}})
        } else {
            json!({"chat_join_request":{"chat":{"id":-1001},"invite_link":{"invite_link":"test-invite"}}})
        };
        updates::handle_update(&state, &update).await?;
        let saved = posts::get(&context, post["id"].as_str().unwrap_or_default())?
            .unwrap_or_else(|| panic!("post is missing"));
        assert!(
            saved["poll_results"].is_null()
                || saved["poll_results"]
                    .as_object()
                    .is_some_and(|value| value.is_empty()),
            "{event}: {saved}"
        );
        assert_eq!(
            invite_links::get(&context, "test-invite")?
                .unwrap_or_else(|| panic!("invite link is missing"))["requests"],
            json!(0),
            "{event}"
        );
        assert!(fake.count("getChatMember") >= 1, "{event}");
    }
    Ok(())
}

#[tokio::test]
async fn own_dm_preview_callback_needs_no_channel_membership() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let config = public_settings(&dir, &base);
    let state = AppState::new(config.clone());
    let app = build_router(state.clone());
    login_as(&app, 101).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    updates::remember_preview(
        &context,
        &json!(101),
        77,
        &json!({"button":"Private preview answer"}),
    )
    .await?;
    fake.clear_calls();
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"callback_query":{"id":"query","data":"button","message":{"chat":{"id":101},"message_id":77}}}),
        )
        .await?,
        Some("callback")
    );
    let answers = fake.calls_to("answerCallbackQuery");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["body"]["text"], "Private preview answer");
    assert_eq!(fake.count("getChatMember"), 0);
    Ok(())
}

#[tokio::test]
async fn linked_comment_does_not_change_transport_policy() -> TestResult {
    for refused in [false, true] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        fake.queue(
            "getChat",
            vec![ok_envelope(
                json!({"id":-1001,"type":"channel","linked_chat_id":-1002}),
            )],
        );
        if refused {
            fake.queue(
                "sendRichMessage",
                vec![json!({"ok":false,"error_code":403,"description":"Forbidden"})],
            );
        }
        let context = AccountContext::account(&config, 101, Role::Publish);
        let transport = state.transport();
        let result = transport
            .send_linked_comment(
                &context,
                &json!(-1001),
                &json!(-1002),
                77,
                json!({"html":"Comment"}),
            )
            .await;
        if refused {
            assert!(
                matches!(result, Err(TelegramError::Refused { .. })),
                "{result:?}"
            );
        } else {
            result.unwrap_or_else(|error| panic!("comment: {error}"));
        }
        let error = transport
            .call(
                &context,
                "sendRichMessage",
                json!({"chat_id":-1002,"rich_message":{"html":"Other"}}),
                Uploads::default(),
            )
            .await
            .expect_err("an unlinked chat must stay outside the comment exception");
        assert!(
            matches!(error, TelegramError::Policy(_)),
            "refused={refused}: {error}"
        );
        assert!(error.to_string().contains("привяжите"), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn real_database_full_recording_send_result_leaves_key_pending() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    channels::save(
        &context,
        &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel","can_post":1,"rights":{}}),
    )?;
    let transport = state.transport();
    let action_context = context.clone();
    let action_transport = transport.clone();
    let result = Idempotency::new(context.clone())
        .run(Some("full-result"), "send", &json!({}), "api", move || {
            let context = action_context.clone();
            let transport = action_transport.clone();
            async move {
                transport
                    .call(
                        &context,
                        "sendRichMessage",
                        json!({"chat_id":-1001,"rich_message":{"html":"Once"}}),
                        Uploads::default(),
                    )
                    .await
                    .map_err(ServiceError::Telegram)?;
                Ok(json!({"response":"x".repeat(2*1024*1024)}))
            }
        })
        .await;
    assert!(
        matches!(result, Err(ServiceError::Store(StoreError::StorageFull))),
        "{result:?}"
    );
    let connection = Store::connect(&context)?;
    let status: String = connection.query_row(
        "select status from idempotency where key='full-result'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(status, "pending");
    let conflict = Idempotency::new(context.clone())
        .run(Some("full-result"), "send", &json!({}), "api", || async {
            panic!("a fresh pending key must not run its action again")
        })
        .await;
    assert!(
        matches!(conflict, Err(ServiceError::Conflict(_))),
        "{conflict:?}"
    );
    connection.execute(
        "update idempotency set created_at=created_at-? where key='full-result'",
        [STALE_PENDING + 1],
    )?;
    let stale = Idempotency::new(context)
        .run(Some("full-result"), "send", &json!({}), "api", || async {
            panic!("a stale pending key must not run its action again")
        })
        .await;
    assert!(
        matches!(stale, Err(ServiceError::MayHaveGone(_))),
        "{stale:?}"
    );
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}
