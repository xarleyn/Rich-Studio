//! Contract tests ported from the Python reference `tests/test_api.py`.
//!
//! The HTTP/MCP surface is shared with `http_mcp_contract.rs`; this target keeps
//! the larger end-to-end API scenarios separate so they can grow without making
//! that file unwieldy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::services::{scheduler, updates};
use rich_studio::settings::Settings;
use rich_studio::store::db::Store;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

fn merged(mut base: Value, extra: Value) -> Value {
    if let (Some(target), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    base
}

fn settings(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> Settings {
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

fn state(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> AppState {
    AppState::new(settings(dir, base, extra))
}

#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: Value,
    raw: String,
}

#[derive(Clone)]
struct Telegram {
    calls: Arc<Mutex<Vec<Call>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    fail: Arc<Mutex<BTreeMap<String, (i64, String)>>>,
    drop: Arc<Mutex<BTreeMap<String, usize>>>,
    next_message_id: Arc<AtomicI64>,
}

impl Telegram {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            fail: Arc::new(Mutex::new(BTreeMap::new())),
            drop: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
        }
    }

    fn record(&self, method: &str, path: &str, body: &Value, raw: &str) {
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

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.method == method)
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.calls_to(method).len()
    }

    fn last(&self, method: &str) -> Option<Call> {
        self.calls_to(method).pop()
    }

    fn clear_calls(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clear();
    }

    fn fail(&self, method: &str, description: &str) {
        self.fail
            .lock()
            .unwrap_or_else(|error| panic!("fail: {error}"))
            .insert(method.to_owned(), (400, description.to_owned()));
    }

    fn clear_fail(&self) {
        self.fail
            .lock()
            .unwrap_or_else(|error| panic!("fail: {error}"))
            .clear();
    }

    fn drop_responses(&self, method: &str, count: usize) {
        self.drop
            .lock()
            .unwrap_or_else(|error| panic!("drop: {error}"))
            .insert(method.to_owned(), count);
    }

    fn queue(&self, method: &str, responses: Vec<Value>) {
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

    fn should_drop(&self, method: &str) -> bool {
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

    fn answer(&self, method: &str, request: &Value) -> Value {
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

async fn telegram() -> (String, Telegram) {
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

struct Reply {
    status: StatusCode,
    value: Value,
    text: String,
    headers: HeaderMap,
}

async fn call_raw(
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

async fn call(
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

async fn upload(
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

fn context(state: &AppState) -> AccountContext {
    state.account.clone()
}

fn set_scheduled_at(state: &AppState, id: &str, at: i64) {
    Store::connect(&context(state))
        .unwrap_or_else(|error| panic!("db: {error}"))
        .execute(
            "update posts set scheduled_at=? where id=?",
            rusqlite::params![at, id],
        )
        .unwrap_or_else(|error| panic!("schedule: {error}"));
}

async fn run_due(state: &AppState, now: i64) -> usize {
    scheduler::run_due(
        state,
        state.account.clone(),
        now,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
    .unwrap_or_else(|error| panic!("scheduler: {error}"))
}

async fn raw_server(bodies: Vec<Value>) -> String {
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

#[tokio::test]
async fn spec_summary_and_type_lookup_match_the_editor() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let summary = call(app.clone(), "GET", "/api/spec", None, &[]).await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.text);
    let spec = summary.value;
    assert_eq!(spec["version"], "Bot API 10.3");
    let subtypes = spec["types"]["InputRichBlock"]["subtypes"]
        .as_array()
        .ok_or("subtypes")?;
    assert_eq!(subtypes.len(), 24);
    let constants = subtypes
        .iter()
        .map(|name| spec["types"][name.as_str().unwrap_or_default()]["const"].clone())
        .collect::<Vec<_>>();
    for expected in [
        "paragraph",
        "heading",
        "table",
        "buttons",
        "document",
        "map",
        "slideshow",
    ] {
        assert!(
            constants.iter().any(|value| value == expected),
            "missing {expected}"
        );
    }
    assert!(constants.iter().all(|value| !value.is_null()));
    assert_eq!(
        spec["types"]["RichMessageButton"]["fields"][1]["enum"],
        json!(["danger", "success", "primary", "link"])
    );

    let definition = call(
        app.clone(),
        "GET",
        "/api/spec/type/sendRichMessage",
        None,
        &[],
    )
    .await;
    assert_eq!(definition.status, StatusCode::OK, "{}", definition.text);
    assert!(
        definition.value["fields"]
            .as_array()
            .is_some_and(|fields| !fields.is_empty())
    );
    assert_eq!(
        call(app, "GET", "/api/spec/type/NoSuchType", None, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[test]
fn diff_reports_added_subtype_type_field_and_method() {
    use rich_studio::spec::diff::diff_specs;
    let old = json!({
        "types": {
            "InputRichBlock": {"subtypes": ["InputRichBlockParagraph"]},
            "InputRichBlockTable": {"fields": [{"name": "cells", "types": ["X"]}]}
        },
        "methods": {}
    });
    let new = json!({
        "types": {
            "InputRichBlock": {"subtypes": ["InputRichBlockParagraph", "InputRichBlockPoll"]},
            "InputRichBlockPoll": {"description": ["A poll block."], "fields": []},
            "InputRichBlockTable": {
                "fields": [
                    {"name": "cells", "types": ["X"]},
                    {"name": "is_compact", "types": ["Boolean"]}
                ]
            }
        },
        "methods": {"sendRichPoll": {"description": ["Sends a poll."], "fields": []}}
    });
    let kinds = diff_specs(&old, &new)
        .into_iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap_or_default().to_owned(),
                item["name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<BTreeSet<_>>();
    for expected in [
        ("subtype_added", "InputRichBlock → InputRichBlockPoll"),
        ("type_added", "InputRichBlockPoll"),
        ("field_added", "InputRichBlockTable.is_compact"),
        ("method_added", "sendRichPoll"),
    ] {
        assert!(
            kinds.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing {expected:?}: {kinds:?}"
        );
    }
}

#[tokio::test]
async fn bundled_examples_validate() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let blocks: Value = serde_json::from_str(include_str!("../../examples/post-blocks.json"))?;
    let html = include_str!("../../examples/post-showcase.html");
    for post in [
        json!({"rich_message":{"blocks":blocks}}),
        json!({"rich_message":{"html":html}}),
    ] {
        let reply = call(app.clone(), "POST", "/api/validate", Some(post), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], true, "{}", reply.text);
    }
    Ok(())
}

#[tokio::test]
async fn validation_errors_name_the_exact_problem() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let long_html = format!("<p>{}</p>", "я".repeat(33000));
    let cases = [
        (
            json!({"blocks": [{"type": "paragraf", "text": "x"}]}),
            "неизвестный type='paragraf'",
        ),
        (
            json!({"blocks": [{"type": "heading", "size": "2", "text": "x"}]}),
            "ожидалось целое число",
        ),
        (
            json!({"blocks": [{"type": "table"}]}),
            "нет обязательного поля cells",
        ),
        (
            json!({"html": "<img src=\"tg://photo?id=cover\">"}),
            "нет элемента с id='cover'",
        ),
        (
            json!({"html": "<p>a</p>", "markdown": "b"}),
            "ровно одно из html / markdown / blocks",
        ),
        (json!({}), "ровно одно из html / markdown / blocks"),
        (json!({"html": long_html}), "лимит 32768"),
    ];
    for (rich_message, fragment) in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(json!({"rich_message":rich_message})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], false, "{}", reply.text);
        assert!(
            reply.value["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|error| error.contains(fragment)),
            "missing {fragment:?}: {}",
            reply.text
        );
    }
    Ok(())
}

#[tokio::test]
async fn button_links_are_checked_like_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let good = json!({"inline_keyboard":[[
        {"text":"Сайт","url":"https://t.me/telegram"},
        {"text":"Бот","url":"tg://resolve?domain=x"}
    ]]});
    let reply = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":blocks_post()["rich_message"],"reply_markup":good})),
        &[],
    )
    .await;
    assert_eq!(reply.value["ok"], true, "{}", reply.text);

    for url in ["https://", "", "http:///path"] {
        let post = json!({
            "rich_message":blocks_post()["rich_message"],
            "reply_markup":{"inline_keyboard":[[{"text":"Сайт","url":url}]]}
        });
        let checked = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(post.clone()),
            &[],
        )
        .await;
        assert_eq!(checked.value["ok"], false, "{url}: {}", checked.text);
        assert!(
            checked.value["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|error| error.contains("кнопка «Сайт» (ряд 1, №1)")),
            "{url}: {}",
            checked.text
        );
        let sent = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":post})),
            &[],
        )
        .await;
        assert_eq!(sent.status, StatusCode::UNPROCESSABLE_ENTITY, "{url}");
    }
    assert_eq!(telegram.count("sendRichMessage"), 0);

    let rich = json!({"rich_message":{"blocks":[
        {"type":"buttons","buttons":[{"text":"Сайт","url":"https://"}]}
    ]}});
    let warning = call(app.clone(), "POST", "/api/validate", Some(rich), &[]).await;
    assert!(
        warning.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("нет адреса сайта")),
        "{}",
        warning.text
    );

    let mut actionless = json!({
        "rich_message":blocks_post()["rich_message"],
        "reply_markup":{"inline_keyboard":[[{"text":"Пусто","style":"primary"}]]}
    });
    let checked = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(actionless.clone()),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], false, "{}", checked.text);
    assert!(
        checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("не задано, что она делает")),
        "{}",
        checked.text
    );
    actionless["reply_markup"]["inline_keyboard"][0][0]["future_action"] = json!({"x":1});
    let checked = call(app, "POST", "/api/validate", Some(actionless), &[]).await;
    assert!(
        !checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("что она делает")),
        "{}",
        checked.text
    );
    Ok(())
}

#[tokio::test]
async fn send_passes_the_payload_through_unchanged() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let post = json!({
        "rich_message": {"html":"<h1>Привет</h1><tg-new-tag x='1'>future</tg-new-tag>","is_rtl":false},
        "reply_markup": {"inline_keyboard":[[{"text":"Сайт","url":"https://t.me"}]]},
        "options": {"disable_notification":true,"protect_content":true,"message_effect_id":"123"}
    });
    let reply = call(app, "POST", "/api/send", Some(json!({"post":post})), &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = telegram.last("sendRichMessage").ok_or("sendRichMessage")?;
    assert!(
        sent.path.contains("/bot123:TEST/sendRichMessage"),
        "{}",
        sent.path
    );
    assert_eq!(sent.body["chat_id"], "@test_channel");
    assert_eq!(sent.body["rich_message"], post["rich_message"]);
    assert_eq!(sent.body["reply_markup"], post["reply_markup"]);
    assert_eq!(sent.body["disable_notification"], true);
    assert_eq!(sent.body["protect_content"], true);
    assert_eq!(sent.body["message_effect_id"], "123");
    let record = &reply.value["post"];
    assert_eq!(record["status"], "sent");
    assert_eq!(record["message_id"], reply.value["message"]["message_id"]);
    Ok(())
}

#[tokio::test]
async fn invalid_post_and_unwritable_row_never_reach_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let invalid = json!({"post":{"rich_message":{"blocks":[{"type":"nope"}]}}});
    assert_eq!(
        call(app.clone(), "POST", "/api/send", Some(invalid), &[])
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(telegram.count("sendRichMessage"), 0);

    Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_insert before insert on posts
         begin select raise(abort, 'database is locked'); end;",
    )?;
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(telegram.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn numeric_chat_id_is_sent_as_an_integer() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"chat_id":"-1001234567890"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?.body["chat_id"],
        json!(-1001234567890i64)
    );
    Ok(())
}

#[tokio::test]
async fn telegram_refusal_is_listed_with_chat_and_title() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.fail(
        "sendRichMessage",
        "Bad Request: not enough rights to send rich messages",
    );
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@chan"})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(
        reply.value["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("not enough rights")
    );
    let listed = call(app, "GET", "/api/posts", None, &[]).await;
    let [post] = listed.value.as_array().ok_or("posts")?.as_slice() else {
        panic!("expected one post: {}", listed.text);
    };
    assert_eq!(post["status"], "failed");
    assert!(
        post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not enough rights")
    );
    assert_eq!(post["chat_id"], "@chan");
    assert_eq!(post["title"], "Тест");
    assert_eq!(post["link"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn failed_channel_rows_are_listed_and_filterable() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.fail("sendRichMessage", "Bad Request: chat not found");
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b"]})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text);
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    assert_eq!(
        rows.as_array()
            .into_iter()
            .flatten()
            .map(|post| post["status"].clone())
            .collect::<Vec<_>>(),
        [json!("failed"), json!("failed")]
    );
    let failed = call(
        app.clone(),
        "GET",
        "/api/posts?status=failed&chat=@a",
        None,
        &[],
    )
    .await
    .value;
    assert_eq!(failed[0]["chat_id"], "@a");
    assert_eq!(
        call(app, "GET", "/api/posts?status=sent", None, &[])
            .await
            .value,
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn successful_draft_resend_drops_only_its_refusal() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    telegram.fail(
        "sendRichMessage",
        "Bad Request: inline keyboard button URL 'https://' is invalid",
    );
    for chat in ["@c", "@other"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":blocks_post(),"draft_id":draft_id,"chat_id":chat})),
                &[],
            )
            .await
            .status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{chat}"
        );
    }
    telegram.clear_fail();
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"draft_id":draft_id,"chat_id":"@c"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    let pairs = rows
        .as_array()
        .ok_or("posts")?
        .iter()
        .map(|post| {
            (
                post["status"].as_str().unwrap_or_default().to_owned(),
                post["chat_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        pairs,
        [
            ("sent".to_owned(), "@c".to_owned()),
            ("failed".to_owned(), "@other".to_owned())
        ]
    );
    let failed_id = rows[1]["id"].as_str().ok_or("failed id")?;
    let removed = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{failed_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(removed.value["status"], "removed");
    assert_eq!(
        call(app, "GET", "/api/posts", None, &[])
            .await
            .value
            .as_array()
            .into_iter()
            .flatten()
            .map(|post| post["status"].clone())
            .collect::<Vec<_>>(),
        [json!("sent")]
    );
    Ok(())
}

#[tokio::test]
async fn failed_row_recording_never_hides_the_telegram_error() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    // The first send creates the posts table the trigger attaches to.
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_failed before update of status on posts
         when NEW.status='failed'
         begin select raise(abort, 'database is locked'); end;",
    )?;
    telegram.fail("sendRichMessage", "Bad Request: chat not found");
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(reply.text.contains("chat not found"), "{}", reply.text);
    Ok(())
}

#[tokio::test]
async fn posts_list_shows_title_thumb_link_and_hides_payload() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let uploaded = upload(app.clone(), "cover.png", b"\x89PNG fake", "image/png", &[]).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text);
    let uploaded = uploaded.value;
    let reference = uploaded["ref"].as_str().ok_or("ref")?;
    let html =
        "<figure><img src=\"tg://photo?id=c\"></figure><h1>Анонс &amp; эфир</h1><p>Подробности</p>";
    let rich = json!({"html":html,"media":[{"id":"c","media":{"type":"photo","media":reference}}]});
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":{"rich_message":rich},"chat_id":"@my_chan"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":{"rich_message":{"markdown":"## **Итоги** недели\n\nтекст"}},"chat_id":"-1001234567890"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let rows = call(app, "GET", "/api/posts", None, &[]).await.value;
    let rows = rows.as_array().ok_or("posts")?;
    assert_eq!(rows.len(), 2);
    let (markdown, first) = (&rows[0], &rows[1]);
    assert_eq!(first["title"], "Анонс & эфир");
    assert_eq!(
        first["thumb"],
        format!(
            "/media/{}",
            reference.strip_prefix("upload:").unwrap_or_default()
        )
    );
    assert_eq!(
        first["link"],
        format!(
            "https://t.me/my_chan/{}",
            first["message_id"].as_i64().ok_or("message id")?
        )
    );
    assert_eq!(first["pinned"], false);
    assert!(first.get("payload").is_none());
    assert_eq!(markdown["title"], "Итоги недели");
    assert_eq!(markdown["thumb"], Value::Null);
    assert_eq!(
        markdown["link"],
        format!(
            "https://t.me/c/1234567890/{}",
            markdown["message_id"].as_i64().ok_or("message id")?
        )
    );
    Ok(())
}

#[tokio::test]
async fn upload_is_attached_and_stored_as_upload_reference() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let uploaded = upload(app.clone(), "обложка 1.png", b"PNGDATA", "image/png", &[])
        .await
        .value;
    assert_eq!(uploaded["kind"], "photo");
    let reference = uploaded["ref"].as_str().ok_or("ref")?.to_owned();
    assert!(reference.starts_with("upload:"));
    let post = json!({"rich_message":{"blocks":[
        {"type":"photo","photo":{"type":"photo","media":reference}}
    ]}});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = telegram.last("sendRichMessage").ok_or("send")?;
    assert!(sent.raw.contains("attach://f0"), "{}", sent.raw);
    assert!(sent.raw.contains("PNGDATA"), "{}", sent.raw);
    let id = reply.value["post"]["id"].as_str().ok_or("post id")?;
    let stored = call(app, "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(
        stored["payload"]["rich_message"]["blocks"][0]["photo"]["media"],
        reference
    );
    Ok(())
}

#[tokio::test]
async fn delete_published_post_sends_telegram_and_marks_it_deleted() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]
        .clone();
    let id = sent["id"].as_str().ok_or("post id")?;
    let removed = call(app, "DELETE", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
    assert_eq!(removed.value["status"], "deleted");
    assert_eq!(
        telegram.last("deleteMessage").ok_or("delete")?.body["message_id"],
        sent["message_id"]
    );
    Ok(())
}

#[tokio::test]
async fn schedule_time_and_state_transitions_are_guarded() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"post":blocks_post(),"at":scheduler::now() - 3600})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    for at in ["2030-01-01T09:00:00", "завтра"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/schedule",
                Some(json!({"post":blocks_post(),"at":at})),
                &[],
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{at}"
        );
    }
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":"2030-01-01T09:00:00+03:00"})),
        &[],
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    assert_eq!(scheduled.value["scheduled_at"], 1893477600i64);
    let id = scheduled.value["id"].as_str().ok_or("id")?.to_owned();
    let cancelled = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(cancelled.value["status"], "cancelled");
    assert_eq!(telegram.count("deleteMessage"), 0);

    let future = call(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":scheduler::now() + 3600})),
        &[],
    )
    .await
    .value;
    let id = future["id"].as_str().ok_or("id")?.to_owned();
    Store::connect(&state.account)?
        .execute("update posts set status='deleted' where id=?", [&id])?;
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/posts/{id}"),
            None,
            &[]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["status"],
        "deleted"
    );
    Store::connect(&state.account)?.execute(
        "update posts set status='failed', error='x' where id=?",
        [&id],
    )?;
    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/edit"),
            Some(blocks_post()),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let removed = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(removed.value["status"], "removed");
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn schedule_to_several_channels_reports_each_one() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b"],"at":scheduler::now() + 999})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let all = reply.value["all"].as_array().ok_or("all")?;
    assert_eq!(
        all.iter()
            .map(|post| post["chat_id"].clone())
            .collect::<Vec<_>>(),
        [json!("@a"), json!("@b")]
    );
    Ok(())
}

#[tokio::test]
async fn send_to_several_channels_deduplicates_and_reports_each_one() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b","@a","-1002"]})),
        &[],
    )
    .await
    .value;
    assert_eq!(
        telegram
            .calls_to("sendRichMessage")
            .into_iter()
            .map(|call| call.body["chat_id"].clone())
            .collect::<Vec<_>>(),
        [json!("@a"), json!("@b"), json!(-1002)]
    );
    assert_eq!(
        reply["results"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|result| result["ok"].clone())
            .collect::<Vec<_>>(),
        [json!(true), json!(true), json!(true)]
    );
    Ok(())
}

#[tokio::test]
async fn render_pages_escape_payload_and_whitelist_theme() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let evil = json!({"rich_message":{"html":"</script><script>alert(1)</script>"}});
    let page = call(
        app.clone(),
        "POST",
        "/api/render?theme=dark",
        Some(evil),
        &[],
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        !page.text.contains("<script>alert(1)</script>"),
        "{}",
        page.text
    );
    assert!(!page.text.contains("__THEME__"));
    assert!(page.text.contains("data-theme=\"dark\""), "{}", page.text);
    assert!(
        page.headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("script-src 'self'")),
        "render CSP"
    );

    let injected = call(
        app.clone(),
        "POST",
        "/api/render?theme=x%22%3Balert(1)//",
        Some(blocks_post()),
        &[],
    )
    .await;
    assert!(!injected.text.contains("x\";alert(1)"), "{}", injected.text);
    assert!(
        injected.text.contains("data-theme=\"auto\""),
        "{}",
        injected.text
    );

    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"x","post":{"rich_message":{"html":"<p>a</p>"}},"\",x=alert(1)//":1})),
        &[],
    )
    .await
    .value;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let draft_page = call(
        app,
        "GET",
        &format!("/render/draft/{id}?theme=__PAYLOAD__"),
        None,
        &[],
    )
    .await;
    assert!(
        draft_page.text.contains("data-theme=\"auto\""),
        "{}",
        draft_page.text
    );
    assert!(
        !draft_page.text.contains("data-theme=\"{"),
        "{}",
        draft_page.text
    );

    let index = call(build_router(state(&dir, &base, &[])), "GET", "/", None, &[]).await;
    assert!(
        index
            .headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("script-src 'self'")),
        "index CSP"
    );
    Ok(())
}

#[tokio::test]
async fn check_reports_bot_username_and_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(app, "GET", "/api/check", None, &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["can_post"], true);
    assert_eq!(reply.value["bot"]["username"], "studio_test_bot");
    Ok(())
}

#[tokio::test]
async fn raw_method_passthrough_and_invalid_method() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(app.clone(), "POST", "/api/raw/getMe", Some(json!({})), &[])
            .await
            .value["result"]["id"],
        42
    );
    assert_eq!(
        call(app, "POST", "/api/raw/get-me;rm", Some(json!({})), &[],)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn configured_key_protects_media_render_docs_and_static() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("STUDIO_API_KEY", "secret")]));
    for path in [
        "/media/a.png",
        "/render/draft/x",
        "/docs",
        "/openapi.json",
        "/api/spec?key=secret",
    ] {
        assert_eq!(
            call(app.clone(), "GET", path, None, &[]).await.status,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    assert_eq!(
        call(app.clone(), "GET", "/static/js/main.js", None, &[])
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/spec",
            None,
            &[("cookie", "rs_key=secret")],
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let session = call(
        app.clone(),
        "POST",
        "/api/session",
        Some(json!({"key":"secret"})),
        &[],
    )
    .await;
    assert_eq!(session.status, StatusCode::OK, "{}", session.text);
    let cookie = session
        .headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        call(app, "GET", "/api/spec", None, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn configured_key_accepts_the_header_too() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("STUDIO_API_KEY", "secret")]));
    assert_eq!(
        call(app.clone(), "GET", "/api/drafts", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app, "GET", "/api/drafts", None, &[("x-api-key", "secret")],)
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn non_json_and_cross_site_write_bodies_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let text = json!({"post":blocks_post()}).to_string().into_bytes();
    assert_eq!(
        call_raw(
            app.clone(),
            "POST",
            "/api/send",
            Some(text),
            &[("content-type", "text/plain")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        call_raw(
            app.clone(),
            "POST",
            "/api/raw/deleteMessage",
            Some(b"chat_id=@x&message_id=1".to_vec()),
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        call_raw(
            app,
            "POST",
            "/api/raw/logOut",
            Some(Vec::new()),
            &[("content-type", "text/plain")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for method in ["sendRichMessage", "deleteMessage", "logOut"] {
        assert_eq!(telegram.count(method), 0, "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn foreign_host_is_refused_but_allowed_hosts_work() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("host", "evil.example")],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            app,
            "GET",
            "/api/drafts",
            None,
            &[("host", "localhost:8080")],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn cross_site_writes_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let cases: &[&[(&str, &str)]] = &[
        &[("origin", "https://evil.example")],
        &[("origin", "null")],
        &[("sec-fetch-site", "cross-site")],
        &[("origin", "http://localhost:8080")],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:8080"),
        ],
        &[("host", "localhost:443"), ("origin", "http://localhost")],
        &[
            ("origin", "https://evil.example"),
            ("sec-fetch-site", "same-origin"),
        ],
    ];
    for headers in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            Some(json!({})),
            headers,
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{headers:?}: {}",
            reply.text
        );
        let media = upload(
            app.clone(),
            "x.html",
            b"<script>1</script>",
            "text/html",
            headers,
        )
        .await;
        assert_eq!(media.status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn same_origin_writes_are_allowed() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let cases: &[&[(&str, &str)]] = &[
        &[
            ("origin", "http://localhost"),
            ("sec-fetch-site", "same-origin"),
        ],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:7424"),
            ("sec-fetch-site", "same-origin"),
        ],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:7424"),
        ],
        &[("host", "[::1]:7424"), ("origin", "http://[::1]:7424")],
        &[("host", "localhost:443"), ("origin", "https://localhost")],
    ];
    for headers in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            Some(json!({})),
            headers,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{headers:?}: {}", reply.text);
    }
    Ok(())
}

#[tokio::test]
async fn raw_methods_require_confirmation_outside_the_allowlist() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(app.clone(), "POST", "/api/raw/logOut", Some(json!({})), &[])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert!(telegram.calls().is_empty());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/raw/logOut",
            Some(json!({})),
            &[("x-confirm", "yes")],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert!(telegram.last("logOut").is_some());
    telegram.clear_calls();
    for method in [
        "deleteMessages",
        "setChatTitle",
        "unpinAllChatMessages",
        "getUpdates",
        "someMethodTelegramAddsLater",
    ] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                &format!("/api/raw/{method}"),
                Some(json!({})),
                &[],
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{method}"
        );
    }
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn raw_read_and_send_methods_run_without_confirmation() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for method in [
        "getChat",
        "sendMessage",
        "editMessageText",
        "answerCallbackQuery",
        "copyMessage",
        "stopPoll",
        "pinChatMessage",
    ] {
        let reply = call(
            app.clone(),
            "POST",
            &format!("/api/raw/{method}"),
            Some(json!({})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{method}: {}", reply.text);
        assert!(telegram.last(method).is_some(), "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn upload_over_limit_is_refused_and_uploads_are_served_safely() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("MAX_UPLOAD_MB", "0.001")]));
    let before = call(app.clone(), "GET", "/api/media", None, &[])
        .await
        .value;
    let reply = upload(app.clone(), "big.png", &[b'x'; 5000], "image/png", &[]).await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.text
    );
    assert!(
        reply.value["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("МБ"),
        "{}",
        reply.text
    );
    assert_eq!(
        call(app.clone(), "GET", "/api/media", None, &[])
            .await
            .value,
        before
    );

    let html = upload(
        app.clone(),
        "page.html",
        b"<script>alert(1)</script>",
        "text/html",
        &[],
    )
    .await
    .value;
    let name = html["name"].as_str().ok_or("name")?;
    let served = call(app.clone(), "GET", &format!("/media/{name}"), None, &[]).await;
    assert_eq!(
        served
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/octet-stream")
    );
    assert!(
        served
            .headers
            .get("content-disposition")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("attachment")
    );
    assert!(
        served
            .headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .starts_with("sandbox")
    );
    let png = upload(app.clone(), "a.png", b"png", "image/png", &[])
        .await
        .value;
    let png_name = png["name"].as_str().ok_or("name")?;
    let served = call(app, "GET", &format!("/media/{png_name}"), None, &[]).await;
    assert_eq!(
        served
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        served
            .headers
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    Ok(())
}

#[test]
fn upload_limit_follows_own_bot_api_server() -> TestResult {
    let dir = TempDir::new()?;
    let bare = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))?;
    assert_eq!(bare.max_upload_mb, 50.0);
    let own = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), "http://localhost:8081".to_owned()),
    ]))?;
    assert_eq!(own.max_upload_mb, 2000.0);
    let explicit = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("MAX_UPLOAD_MB".to_owned(), "10".to_owned()),
    ]))?;
    assert_eq!(explicit.max_upload_mb, 10.0);
    Ok(())
}

#[tokio::test]
async fn options_cannot_override_core_params() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for key in ["chat_id", "rich_message", "reply_markup"] {
        let mut post = blocks_post();
        post["options"] = json!({key:"@other"});
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":post})),
                &[]
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{key}"
        );
    }
    assert_eq!(telegram.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn pin_after_send_and_pin_endpoints() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let mut post = blocks_post();
    post["pin"] = json!(true);
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let pin = telegram.last("pinChatMessage").ok_or("pin")?.body;
    assert_eq!(
        pin,
        json!({"chat_id":-1001,"message_id":reply.value["message"]["message_id"],"disable_notification":true})
    );
    assert_eq!(reply.value["post"]["pinned"], 1);
    assert_eq!(reply.value["post"]["chat_num"], -1001);
    let sent = telegram.last("sendRichMessage").ok_or("send")?;
    assert_eq!(
        sent.body
            .as_object()
            .map(|object| object.keys().cloned().collect::<BTreeSet<_>>()),
        Some(BTreeSet::from([
            "chat_id".to_owned(),
            "rich_message".to_owned()
        ]))
    );

    telegram.fail("pinChatMessage", "Bad Request: not enough rights");
    let mut failing = blocks_post();
    failing["pin"] = json!(true);
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":failing})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("закрепить")),
        "{}",
        reply.text
    );
    telegram.clear_fail();

    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]
        .clone();
    let id = sent["id"].as_str().ok_or("id")?;
    let pinned = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/pin"),
        Some(json!({"disable_notification":false})),
        &[],
    )
    .await;
    assert_eq!(pinned.value["pinned"], 1);
    assert_eq!(
        telegram.last("pinChatMessage").ok_or("pin")?.body["disable_notification"],
        false
    );
    let unpinned = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}/pin"),
        None,
        &[],
    )
    .await;
    assert_eq!(unpinned.value["pinned"], 0);
    assert_eq!(
        telegram.last("unpinChatMessage").ok_or("unpin")?.body["message_id"],
        sent["message_id"]
    );

    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/pin"),
            None,
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        telegram.last("pinChatMessage").ok_or("pin")?.body["disable_notification"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn button_endpoint_replaces_markup_and_checks_links() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let id = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]["id"]
        .as_str()
        .ok_or("id")?
        .to_owned();
    let keyboard = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let changed = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard,"callbacks":{"like":"Спасибо!"}})),
        &[],
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    assert_eq!(
        telegram
            .last("editMessageReplyMarkup")
            .ok_or("markup")?
            .body["reply_markup"],
        keyboard
    );
    assert_eq!(changed.value["payload"]["reply_markup"], keyboard);
    assert_eq!(
        changed.value["payload"]["callbacks"],
        json!({"like":"Спасибо!"})
    );
    assert_eq!(telegram.count("editMessageText"), 0);

    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/buttons"),
            Some(json!({"reply_markup":null})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let cleared = telegram
        .last("editMessageReplyMarkup")
        .ok_or("markup")?
        .body;
    assert!(cleared.get("reply_markup").is_none(), "{cleared}");

    let calls = telegram.calls().len();
    let bad = json!({"inline_keyboard":[[{"text":"Сайт","url":""}]]});
    let refused = call(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":bad})),
        &[],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );
    assert!(refused.text.contains("другой тип"), "{}", refused.text);
    assert_eq!(telegram.calls().len(), calls);
    Ok(())
}

#[tokio::test]
async fn callback_clicks_are_answered_named_and_counted() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut post = blocks_post();
    post["reply_markup"] = button;
    post["callbacks"] = json!({"like":{"text":"Голос учтён","show_alert":true}});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await
    .value;
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    let mut update = json!({
        "update_id":1,
        "callback_query":{
            "id":"cq1",
            "data":"like",
            "from":{"id":5},
            "message":{"message_id":message_id,"chat":{"id":-1001}}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("callback")
    );
    assert_eq!(
        telegram.last("answerCallbackQuery").ok_or("answer")?.body,
        json!({"callback_query_id":"cq1","text":"Голос учтён","show_alert":true})
    );
    update["callback_query"]["id"] = json!("cq2");
    updates::handle_update(&state, &update).await?;
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    let stored = call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(stored["clicks"], json!({"like":2}));
    let listed = call(app, "GET", "/api/posts", None, &[]).await.value;
    let row = listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|post| post["id"] == id)
        .ok_or("listed")?;
    assert_eq!(row["clicks"], json!({"like":2}));
    assert_eq!(row["buttons"], json!({"like":"👍"}));

    updates::handle_update(
        &state,
        &json!({
            "update_id":2,
            "callback_query":{"id":"cq3","data":"x","message":{"message_id":1,"chat":{"id":-5}}}
        }),
    )
    .await?;
    assert_eq!(
        telegram.last("answerCallbackQuery").ok_or("answer")?.body,
        json!({"callback_query_id":"cq3"})
    );
    Ok(())
}

#[tokio::test]
async fn first_comment_replies_to_the_forward_once() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let mut post = blocks_post();
    post["comment"] = json!({"html":"<p>Ссылки из эфира</p>"});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await
    .value;
    assert_eq!(reply["post"]["comment_status"], "waiting");
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    let forward = json!({
        "update_id":3,
        "message":{
            "message_id":77,
            "chat":{"id":-2002,"type":"supergroup"},
            "is_automatic_forward":true,
            "forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":message_id}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &forward).await?,
        Some("comment")
    );
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("comment")?.body,
        json!({
            "chat_id":-2002,
            "rich_message":{"html":"<p>Ссылки из эфира</p>"},
            "reply_parameters":{"message_id":77}
        })
    );
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[],)
            .await
            .value["comment_status"],
        "sent"
    );
    let calls = telegram.calls().len();
    updates::handle_update(&state, &forward).await?;
    assert_eq!(telegram.calls().len(), calls);
    Ok(())
}

#[tokio::test]
async fn invalid_first_comment_is_refused_before_send() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let mut post = blocks_post();
    post["comment"] = json!({"html":"a","markdown":"b"});
    let reply = call(app, "POST", "/api/send", Some(json!({"post":post})), &[]).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn reaction_counts_store_emoji_custom_and_paid() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    updates::handle_update(
        &state,
        &json!({
            "update_id":4,
            "message_reaction_count":{
                "chat":{"id":-1001},
                "message_id":message_id,
                "date":0,
                "reactions":[
                    {"type":{"type":"emoji","emoji":"👍"},"total_count":5},
                    {"type":{"type":"custom_emoji","custom_emoji_id":"123"},"total_count":1},
                    {"type":{"type":"paid"},"total_count":2}
                ]
            }
        }),
    )
    .await?;
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["reactions"],
        json!({"👍":5,"custom:123":1,"paid":2})
    );
    Ok(())
}

#[tokio::test]
async fn malformed_and_deep_blocks_are_reported_not_crashing() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for rich_message in [
        json!({"blocks": [{"type": "list", "items": ["x"]}]}),
        json!({"blocks": [{"type": "table", "cells": [["a"]]}]}),
        json!({"blocks": "not a list"}),
    ] {
        let reply = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(json!({"rich_message":rich_message})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], false, "{}", reply.text);
    }

    let mut blocks = Vec::new();
    {
        let mut node = &mut blocks;
        for _ in 0..110 {
            let inner: Vec<Value> = Vec::new();
            node.push(json!({"type":"blockquote","blocks":inner}));
            node = node
                .last_mut()
                .and_then(|block| block.get_mut("blocks"))
                .and_then(Value::as_array_mut)
                .ok_or("nested blocks")?;
        }
    }
    let deep = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":{"blocks":blocks}})),
        &[],
    )
    .await;
    assert_eq!(deep.status, StatusCode::OK, "{}", deep.text);
    assert_eq!(deep.value["ok"], false, "{}", deep.text);
    assert!(
        deep.value["errors"][0]
            .as_str()
            .unwrap_or_default()
            .contains("вложенность"),
        "{}",
        deep.text
    );

    let wrong =
        json!({"blocks":[{"type":"photo","photo":{"type":"video","media":"https://x/y.mp4"}}]});
    let checked = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":wrong})),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], false);
    assert!(
        checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("должно быть 'photo'")),
        "{}",
        checked.text
    );
    let valid = json!({
        "html":"<video src='tg://video?id=v'></video>",
        "media":[{"id":"v","media":{"type":"video","media":"https://x/y.mp4","supports_streaming":true}}]
    });
    let checked = call(
        app,
        "POST",
        "/api/validate",
        Some(json!({"rich_message":valid})),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], true, "{}", checked.text);
    assert_eq!(checked.value["warnings"], json!([]), "{}", checked.text);
    Ok(())
}

#[tokio::test]
async fn poll_after_follows_the_post_and_inherits_options() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let mut post = blocks_post();
    post["options"] = json!({"disable_notification":true});
    post["poll_after"] = poll.clone();
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["warnings"], json!([]), "{}", reply.text);
    let methods = telegram
        .calls()
        .into_iter()
        .filter(|call| ["sendRichMessage", "sendPoll"].contains(&call.method.as_str()))
        .map(|call| call.method)
        .collect::<Vec<_>>();
    assert_eq!(methods, ["sendRichMessage", "sendPoll"]);
    assert!(
        telegram
            .last("sendRichMessage")
            .ok_or("send")?
            .body
            .get("poll_after")
            .is_none()
    );
    let sent_poll = telegram.last("sendPoll").ok_or("poll")?.body;
    assert_eq!(sent_poll["question"], "Во сколько эфир?");
    assert_eq!(
        sent_poll["options"],
        json!([{"text":"Утром"},{"text":"Вечером"}])
    );
    assert_eq!(sent_poll["disable_notification"], true);
    assert_eq!(sent_poll["chat_id"], -1001);
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    let rows = rows.as_array().ok_or("posts")?;
    assert_eq!(
        rows.iter()
            .take(2)
            .map(|post| post["kind"].clone())
            .collect::<Vec<_>>(),
        [json!("poll"), json!("rich")]
    );
    assert_eq!(rows[0]["status"], "sent");
    let dry = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post,"dry_run":true})),
        &[],
    )
    .await
    .value;
    assert!(
        dry["after_send"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|step| step == "sendPoll (poll_after)"),
        "{dry}"
    );

    telegram.fail("sendPoll", "Bad Request: poll can't be sent");
    let failed = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll}))})),
        &[],
    )
    .await;
    assert_eq!(failed.status, StatusCode::OK, "{}", failed.text);
    assert_eq!(failed.value["post"]["status"], "sent");
    assert!(
        failed.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("опрос после поста")),
        "{}",
        failed.text
    );
    let poll_row = call(app, "GET", "/api/posts?status=failed", None, &[])
        .await
        .value;
    let poll_row = poll_row
        .as_array()
        .into_iter()
        .flatten()
        .find(|post| post["kind"] == "poll")
        .ok_or("failed poll")?;
    assert!(
        poll_row["error"]
            .as_str()
            .unwrap_or_default()
            .contains("poll can't be sent"),
        "{poll_row}"
    );
    Ok(())
}

#[tokio::test]
async fn poll_after_is_validated_like_sendpoll() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let cases = [
        (
            json!({"question":"?","options":["один"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "два варианта",
        ),
        (
            merged(poll.clone(), json!({"type":"quiz"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "правильный ответ",
        ),
        (
            merged(poll.clone(), json!({"is_anonymous":"да"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "is_anonymous",
        ),
    ];
    for (bad, status, fragment) in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":merged(blocks_post(),json!({"poll_after":bad}))})),
            &[],
        )
        .await;
        assert_eq!(reply.status, status, "{fragment}: {}", reply.text);
        assert!(reply.text.contains(fragment), "{fragment}: {}", reply.text);
    }
    let thread = call(
        app,
        "POST",
        "/api/send",
        Some(json!({
            "post":merged(blocks_post(),json!({"poll_after":merged(poll.clone(),json!({"message_thread_id":5}))})),
            "chat_ids":["@a","@b"],
            "dry_run":true
        })),
        &[],
    )
    .await;
    assert_eq!(thread.status, StatusCode::BAD_REQUEST, "{}", thread.text);
    assert!(thread.text.contains("тема форума"), "{}", thread.text);
    assert_eq!(telegram.count("sendRichMessage"), 0);
    assert_eq!(telegram.count("sendPoll"), 0);
    Ok(())
}

#[tokio::test]
async fn scheduled_post_sends_its_poll_after() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let at = scheduler::now() + 600;
    let scheduled = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll})),"at":at})),
        &[],
    )
    .await
    .value;
    let id = scheduled["id"].as_str().ok_or("id")?.to_owned();
    set_scheduled_at(&state, &id, scheduler::now() - 1);
    run_due(&state, scheduler::now()).await;
    let calls = telegram
        .calls()
        .into_iter()
        .filter(|call| ["sendRichMessage", "sendPoll"].contains(&call.method.as_str()))
        .map(|call| call.method)
        .collect::<Vec<_>>();
    assert_eq!(calls[calls.len() - 2..], ["sendRichMessage", "sendPoll"]);
    Ok(())
}

#[tokio::test]
async fn poll_validation_send_results_and_stop() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/poll",
            Some(json!({"poll":{"question":"?","options":["один"]}})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/poll",
            Some(json!({"poll":{"question":"?","options":["a","b"],"chat_id":"@x"}})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert!(telegram.calls().is_empty());

    let poll = json!({
        "question":"Когда эфир?",
        "options":["Утром",{"text":"Вечером"}],
        "members_only":true,
        "country_codes":["RU","FT"],
        "description":"Голосуют подписчики старше суток"
    });
    let reply = call(
        app.clone(),
        "POST",
        "/api/poll",
        Some(json!({"poll":poll,"pin":true})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = telegram.last("sendPoll").ok_or("sendPoll")?.body;
    assert_eq!(
        body["options"],
        json!([{"text":"Утром"},{"text":"Вечером"}])
    );
    assert_eq!(body["members_only"], true);
    assert_eq!(body["country_codes"], json!(["RU", "FT"]));
    assert_eq!(body["chat_id"], "@test_channel");
    assert!(body.get("pin").is_none(), "{body}");
    assert!(telegram.last("pinChatMessage").is_some());
    let record = reply.value["results"][0]["post"].clone();
    assert_eq!(record["kind"], "poll");
    let id = record["id"].as_str().ok_or("poll id")?.to_owned();
    let poll_id = record["poll_id"].as_str().ok_or("poll_id")?.to_owned();
    updates::handle_update(
        &state,
        &json!({
            "update_id":5,
            "poll":{
                "id":poll_id,
                "total_voter_count":4,
                "is_closed":false,
                "options":[
                    {"text":"Утром","voter_count":3},
                    {"text":"Вечером","voter_count":1}
                ]
            }
        }),
    )
    .await?;
    assert_eq!(
        call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["poll_results"]["total"],
        4
    );
    let stopped = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/stop-poll"),
        None,
        &[],
    )
    .await;
    assert_eq!(stopped.value["poll_results"]["is_closed"], true);
    assert_eq!(
        call(
            app,
            "POST",
            &format!("/api/posts/{id}/edit"),
            Some(blocks_post()),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn scheduled_poll_is_sent_by_the_scheduler() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/poll",
        Some(json!({"poll":{"question":"?","options":["a","b"]},"at":scheduler::now() + 999})),
        &[],
    )
    .await
    .value;
    let id = scheduled["scheduled"][0]["id"]
        .as_str()
        .ok_or("scheduled poll id")?
        .to_owned();
    set_scheduled_at(&state, &id, scheduler::now() - 1);
    run_due(&state, scheduler::now()).await;
    let stored = call(app, "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(stored["status"], "sent");
    assert!(stored["poll_id"].as_str().is_some());
    assert_eq!(
        telegram.last("sendPoll").ok_or("sendPoll")?.body["question"],
        "?"
    );
    Ok(())
}

#[tokio::test]
async fn poll_that_may_have_gone_stays_listed() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"С опросом","post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    telegram.drop_responses("sendPoll", 2);
    for _ in 0..2 {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll})),"draft_id":draft_id})),
                &[],
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    let gone = call(app, "GET", "/api/posts?status=failed", None, &[])
        .await
        .value;
    let gone = gone
        .as_array()
        .into_iter()
        .flatten()
        .filter(|post| post["kind"] == "poll")
        .collect::<Vec<_>>();
    assert_eq!(gone.len(), 2, "{gone:?}");
    assert!(
        gone.iter().all(|post| post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Опрос мог уйти")),
        "{gone:?}"
    );
    Ok(())
}

#[tokio::test]
async fn updates_status_is_off_by_default() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let status = call(app, "GET", "/api/updates", None, &[]).await.value;
    assert_eq!(status["enabled"], false);
    assert_eq!(status["state"], "off");
    Ok(())
}

#[tokio::test]
async fn updates_offset_survives_restart() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[("TG_UPDATES", "on")]);
    rich_studio::store::meta::set(&state.account, updates::OFFSET_KEY, "500")?;
    updates::poll_once(&state).await?;
    let first = telegram
        .calls_to("getUpdates")
        .first()
        .cloned()
        .ok_or("getUpdates")?;
    assert_eq!(first.body["offset"], 500);
    Ok(())
}

#[tokio::test]
async fn updates_loop_handles_updates_and_reports_conflict() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[("TG_UPDATES", "on")]);
    telegram.queue(
        "getUpdates",
        vec![
            json!({"ok":true,"result":[
                {"update_id":10,"poll":{"id":"nope"}},
                {"update_id":11,"callback_query":{"id":"c","data":"d"}}
            ]}),
            json!({"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}),
        ],
    );
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(updates::run(state.clone(), stopping.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if state.updates_status.read().await.state == "conflict" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    stopping.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(30), task).await???;
    let calls = telegram.calls_to("getUpdates");
    assert!(calls.len() >= 2, "{calls:?}");
    assert!(
        calls[0].body["allowed_updates"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|value| value == "message_reaction_count"),
        "{}",
        calls[0].body
    );
    assert_eq!(calls[1].body["offset"], 12);
    assert_eq!(state.updates_status.read().await.state, "conflict");
    assert_eq!(state.updates_status.read().await.handled, 2);
    Ok(())
}

#[tokio::test]
async fn spec_refresh_refuses_bad_downloads() -> TestResult {
    use rich_studio::spec::source::Spec;
    let dir = TempDir::new()?;
    let base = raw_server(vec![
        json!({}),
        json!({"error":"rate limited"}),
        json!({"version":"Bot API 9.0","types":{},"methods":{}}),
    ])
    .await;
    let config = settings(
        &dir,
        "http://127.0.0.1:9",
        &[("SPEC_URL", &format!("{base}/api.json"))],
    );
    let mut spec = Spec::load_initial(&config);
    let good_version = spec.version().to_owned();
    for _ in 0..3 {
        let report = spec.refresh(&config, true).await;
        assert_eq!(report["updated"], false, "{report}");
        assert!(report["error"].as_str().is_some(), "{report}");
        assert_eq!(spec.version(), good_version);
    }
    Ok(())
}

#[tokio::test]
async fn spec_refresh_endpoint_reports_gaps_and_failure() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(
        &dir,
        &base,
        &[("SPEC_URL", "http://127.0.0.1:9/unavailable")],
    ));
    let summary = call(app.clone(), "GET", "/api/spec", None, &[]).await.value;
    assert_eq!(summary["gaps"], json!([]));
    let refreshed = call(app.clone(), "POST", "/api/spec/refresh", None, &[]).await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.text);
    assert_eq!(refreshed.value["updated"], false);
    assert!(
        refreshed.value["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{}",
        refreshed.text
    );
    let check = call(app, "GET", "/api/spec", None, &[]).await.value["last_check"].clone();
    assert!(check["at"].as_i64().unwrap_or_default() > 0, "{check}");
    assert!(
        check["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn editor_modules_are_served_as_javascript() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let static_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app/static");
    let index = std::fs::read_to_string(static_dir.join("index.html"))?;
    assert!(
        index.contains("<script type=\"module\" src=\"/static/js/main.js\">"),
        "editor index entry point changed"
    );
    let pattern = regex::Regex::new(r"from '\./([\w-]+\.js)'")?;
    let mut imports = BTreeSet::new();
    for entry in std::fs::read_dir(static_dir.join("js"))? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("js") {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        for capture in pattern.captures_iter(&source) {
            if let Some(name) = capture.get(1) {
                imports.insert(name.as_str().to_owned());
            }
        }
    }
    assert!(!imports.is_empty());
    for name in imports {
        let reply = call(app.clone(), "GET", &format!("/static/js/{name}"), None, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert!(
            reply
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .contains("javascript"),
            "{name}: {:?}",
            reply.headers.get("content-type")
        );
    }
    Ok(())
}
