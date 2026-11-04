use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Cursor, Read as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use rich_studio::account_context::{AccountContext, Role, TransportPolicy};
use rich_studio::errors::StoreError;
use rich_studio::services::{scheduler, updates};
use rich_studio::settings::Settings;
use rich_studio::store::db::{self, Store};
use rich_studio::store::{auth, channels, drafts, posts};
use rich_studio::telegram::TelegramError;
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn config(data_dir: &Path, extra: &[(&str, &str)]) -> Settings {
    let mut values = BTreeMap::from([(
        "DATA_DIR".to_owned(),
        data_dir.to_string_lossy().into_owned(),
    )]);
    for (name, value) in extra {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("test settings: {error}"))
}

#[test]
fn every_bad_setting_is_named_at_once() {
    let error = Settings::from_map(&BTreeMap::from([
        ("LOG_FILES".to_owned(), "x".to_owned()),
        ("SESSION_DAYS".to_owned(), "-1".to_owned()),
        ("LOG_LEVEL".to_owned(), "loud".to_owned()),
        ("STUDIO_MODE".to_owned(), "prod".to_owned()),
    ]))
    .expect_err("invalid settings");
    let names: Vec<&str> = error
        .problems
        .iter()
        .map(|problem| problem.split('=').next().expect("setting name"))
        .collect();
    assert_eq!(
        names,
        ["STUDIO_MODE", "SESSION_DAYS", "LOG_LEVEL", "LOG_FILES"]
    );
}

#[test]
fn secrets_come_from_files_and_ambiguous_secrets_are_rejected() {
    let dir = TempDir::new().expect("temp dir");
    let token = dir.path().join("token");
    std::fs::write(&token, "123:FROM_FILE\n").expect("secret fixture");
    let config = config(
        dir.path(),
        &[("TG_BOT_TOKEN_FILE", token.to_str().expect("utf-8 path"))],
    );
    assert_eq!(config.bot_token, "123:FROM_FILE");

    let error = Settings::from_map(&BTreeMap::from([(
        "STUDIO_API_KEY_FILE".to_owned(),
        "/definitely/missing/studio-key".to_owned(),
    )]))
    .expect_err("missing secret file");
    assert!(error.problems[0].starts_with("STUDIO_API_KEY_FILE"));

    let error = Settings::from_map(&BTreeMap::from([
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        (
            "TG_BOT_TOKEN_FILE".to_owned(),
            token.to_string_lossy().into_owned(),
        ),
    ]))
    .expect_err("ambiguous secret");
    assert!(error.problems[0].contains("оставьте одно"));
}

#[test]
fn account_paths_are_explicit_and_never_fall_back_to_owner() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(dir.path(), &[]);
    let root = AccountContext::root(&config);
    let first = AccountContext::account(&config, 101, Role::Publish);
    let second = AccountContext::account(&config, 202, Role::Drafts);

    assert_ne!(first.database_path(), root.database_path());
    assert_ne!(first.database_path(), second.database_path());
    assert_eq!(first.media_dir, dir.path().join("accounts/101/media"));
    assert_eq!(second.media_dir, dir.path().join("accounts/202/media"));
    assert_eq!(first.transport_policy, TransportPolicy::Account(101));
    assert_eq!(second.transport_policy, TransportPolicy::Account(202));
    assert_eq!(root.transport_policy, TransportPolicy::Owner);

    let mut first_db = Store::connect(&first).expect("first account database");
    let mut second_db = Store::connect(&second).expect("second account database");
    let mut root_db = Store::connect(&root).expect("owner database");
    for connection in [&mut first_db, &mut second_db, &mut root_db] {
        db::migrate(connection).expect("migrations");
    }
    first_db
        .execute(
            "insert into drafts(id, title, payload) values ('mine', 'First', '{}')",
            [],
        )
        .expect("first draft");
    let count = |connection: &rusqlite::Connection| {
        connection
            .query_row("select count(*) from drafts", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("draft count")
    };
    assert_eq!(count(&first_db), 1);
    assert_eq!(count(&second_db), 0);
    assert_eq!(count(&root_db), 0);
}

#[test]
fn account_database_quota_is_enforced_but_root_has_no_account_quota() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(dir.path(), &[("ACCOUNT_DATABASE_MB", "1")]);
    let account = AccountContext::account(&config, 101, Role::Publish);
    let root = AccountContext::root(&config);
    let mut account_db = Store::connect(&account).expect("account connection");
    let root_db = Store::connect(&root).expect("root connection");
    db::migrate(&mut account_db).expect("account migrations");

    let page_size: i64 = account_db
        .query_row("pragma page_size", [], |row| row.get(0))
        .expect("page size");
    let account_pages: i64 = account_db
        .query_row("pragma max_page_count", [], |row| row.get(0))
        .expect("account max pages");
    let root_pages: i64 = root_db
        .query_row("pragma max_page_count", [], |row| row.get(0))
        .expect("root max pages");
    assert_eq!(account_pages, 1024 * 1024 / page_size);
    assert!(root_pages > account_pages);

    let error = account_db
        .execute(
            "insert into drafts(id, title, payload) values ('big', 'Big', zeroblob(2097152))",
            [],
        )
        .expect_err("account quota");
    assert!(matches!(
        StoreError::from_sqlite(error, &account),
        StoreError::StorageFull
    ));
}

const BOT_TOKEN: &str = "1:fixture";

fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

fn public_config(dir: &TempDir, base: &str) -> Settings {
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
struct FakeTelegram {
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

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .iter()
            .filter_map(|call| call["method"].as_str().map(str::to_owned))
            .collect()
    }

    fn last(&self, method: &str) -> Value {
        self.calls_to(method)
            .pop()
            .unwrap_or_else(|| panic!("no {method} call"))
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

    fn header(&self, name: &str) -> String {
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

async fn call_on(
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

async fn call(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    call_on(app, method, uri, "localhost", body, headers).await
}

async fn upload(app: Router, who: &str, filename: &str, data: &[u8]) -> Reply {
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

fn set_cookie_has(headers: &HeaderMap, flag: &str) -> bool {
    let wanted = flag.to_ascii_lowercase();
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains(&wanted))
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
    let session = cookie(&reply.headers, "rs_session").unwrap_or_else(|| panic!("session cookie"));
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

#[tokio::test]
async fn public_mode_is_closed_without_operator_keys() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    assert_eq!(
        call(app.clone(), "GET", "/api/drafts", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app.clone(), "GET", "/api/session", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app.clone(), "GET", "/", None, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/auth/config", None, &[]).await.json(),
        json!({"telegram":true,"bot_username":"YourBot","owner_login":false})
    );
    Ok(())
}

#[tokio::test]
async fn signed_login_sets_cookie_identity_and_logout_clears_it() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    let session = call(
        app.clone(),
        "GET",
        "/api/session",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(session["user"]["telegram_id"], json!(101));
    assert_eq!(session["level"], json!("publish"));
    assert_eq!(session["name"], json!("tg:101"));

    let config = call(
        app.clone(),
        "GET",
        "/api/config",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(config["default_chat"], json!(""));
    assert_eq!(config["preview_chat"], json!("101"));
    assert!(drafts::list(&AccountContext::root(&settings))?.is_empty());

    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            "/api/session",
            None,
            &[("cookie", who.as_str())]
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/drafts", None, &[("cookie", who.as_str())])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn forged_identity_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings.clone()));

    for change in ["id", "hash", "first_name"] {
        let (state, state_cookie) = start_challenge(&app).await;
        let base = login_fields(101, auth::now());
        let mut signed = base.clone();
        let hash = if change == "hash" {
            "подделка".to_owned()
        } else {
            signed_hash(&base)
        };
        signed.insert("hash".to_owned(), hash);
        match change {
            "id" => {
                signed.insert("id".to_owned(), "999".to_owned());
            }
            "first_name" => {
                signed.insert("first_name".to_owned(), "другое".to_owned());
            }
            _ => {}
        }
        let path = query_path(&signed, &state);
        let state_header = format!("rs_telegram_login={state_cookie}");
        let reply = call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{change}");
    }
    assert!(auth::account_ids(&settings)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn old_and_future_signatures_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    for offset in [-600_i64, 600] {
        let (state, state_cookie) = start_challenge(&app).await;
        let path = callback_path(&state, &login_fields(101, auth::now() + offset));
        let state_header = format!("rs_telegram_login={state_cookie}");
        let reply = call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{offset}");
    }
    Ok(())
}

#[tokio::test]
async fn login_requires_browser_challenge_and_cannot_be_replayed() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    let (state, state_cookie) = start_challenge(&app).await;
    let path = callback_path(&state, &login_fields(101, auth::now()));
    let state_header = format!("rs_telegram_login={state_cookie}");
    assert_eq!(
        call(app.clone(), "GET", &path, None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let duplicated = format!("{path}&id=999");
    assert_eq!(
        call(
            app,
            "GET",
            &duplicated,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn session_cookie_is_httponly_secure_and_challenge_is_lax() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    let start = call_on(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        "localhost",
        Some(json!({})),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    for flag in ["HttpOnly", "Secure", "SameSite=lax"] {
        assert!(set_cookie_has(&start.headers, flag), "{flag}");
    }
    let auth_url = start.json()["auth_url"]
        .as_str()
        .expect("auth_url")
        .to_owned();
    let state = url::Url::parse(&auth_url)
        .expect("auth url")
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("state");
    let state_cookie = cookie(&start.headers, "rs_telegram_login").expect("challenge cookie");
    let path = callback_path(&state, &login_fields(101, auth::now()));
    let state_header = format!("rs_telegram_login={state_cookie}");
    let callback = call_on(
        app,
        "GET",
        &path,
        "localhost",
        None,
        &[
            ("cookie", state_header.as_str()),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(callback.status, StatusCode::SEE_OTHER);
    for flag in ["HttpOnly", "Secure", "SameSite=strict"] {
        assert!(set_cookie_has(&callback.headers, flag), "{flag}");
    }
    assert_eq!(callback.header("cache-control"), "no-store");
    Ok(())
}

#[tokio::test]
async fn accounts_and_operator_have_separate_drafts_and_files() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let root = AccountContext::root(&settings);
    let mut root_db = Store::connect(&root)?;
    db::migrate(&mut root_db)?;
    drop(root_db);
    drafts::save(
        &root,
        None,
        "Личный черновик владельца",
        &blocks_post(),
        None,
        None,
        None,
    )?;
    let app = build_router(AppState::new(settings));

    let first = login_as(&app, 101).await;
    let created = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Первый","post":blocks_post()})),
        &[("cookie", &first)],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    let created = created.json();
    let id = created["id"].as_str().expect("draft id").to_owned();
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("cookie", &first)]
        )
        .await
        .json()
        .as_array()
        .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        upload(app.clone(), &first, "mine.txt", b"first")
            .await
            .status,
        StatusCode::OK
    );

    let second = login_as(&app, 202).await;
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/media",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/api/drafts/{id}"),
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/render/draft/{id}"),
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        upload(app.clone(), &second, "mine.txt", b"second")
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &first)]
        )
        .await
        .bytes
        .as_slice(),
        b"first"
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &second)]
        )
        .await
        .bytes
        .as_slice(),
        b"second"
    );
    assert_eq!(
        call(
            app,
            "GET",
            "/media/..%2F..%2Fstudio.db",
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let titles: Vec<String> = drafts::list(&root)?
        .into_iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(titles, vec!["Личный черновик владельца".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn only_administrators_can_connect_channels() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChatMember",
        vec![
            ok_envelope(json!({"status":"administrator","can_post_messages":true})),
            ok_envelope(json!({"status":"member"})),
        ],
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@someone"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/channels",
            None,
            &[("cookie", &who)]
        )
        .await
        .json(),
        json!([])
    );
    let channel = connect_channel(&app, &who).await;
    assert_eq!(channel["chat_id"], json!("-1001"));

    let other = login_as(&app, 202).await;
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &other)])
            .await
            .json(),
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn bot_needs_admin_posting_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@mine"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &who)])
            .await
            .json(),
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn public_publish_cannot_use_operator_default_or_unlinked_chat() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let at = auth::now() + 120;
    for (path, body) in [
        ("/api/send", json!({"post":blocks_post()})),
        ("/api/edit", json!({"post":blocks_post(),"message_id":100})),
        ("/api/schedule", json!({"post":blocks_post(),"at":at})),
        ("/api/preview-send", json!({"post":blocks_post()})),
    ] {
        let mut body = body;
        body["chat_id"] = json!("@stranger");
        assert_eq!(
            call(app.clone(), "POST", path, Some(body), &[("cookie", &who)])
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert!(
        !fake
            .methods()
            .iter()
            .any(|method| method.starts_with("send") || method.starts_with("edit"))
    );
    Ok(())
}

#[tokio::test]
async fn send_rechecks_rights_and_uses_numeric_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"-1001"})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        fake.last("sendRichMessage")["body"]["chat_id"],
        json!(-1001)
    );

    fake.clear_calls();
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"chat_id":"-1001"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn preview_goes_to_the_signed_in_user() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    let reply = call(
        app,
        "POST",
        "/api/preview-send",
        Some(json!({"post":blocks_post()})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fake.last("sendRichMessage")["body"]["chat_id"], json!(101));
    Ok(())
}

#[tokio::test]
async fn cross_account_reply_source_is_refused() -> TestResult {
    for path in ["/api/send", "/api/preview-send"] {
        for encoded in [false, true] {
            let dir = TempDir::new()?;
            let (base, fake) = fake_server().await;
            let settings = public_config(&dir, &base);
            let app = build_router(AppState::new(settings));

            let who = login_as(&app, 101).await;
            connect_channel(&app, &who).await;
            let parameters = json!({"chat_id":-100999,"message_id":1});
            let reply = if encoded {
                json!(parameters.to_string())
            } else {
                parameters
            };
            let mut post = blocks_post();
            post["options"] = json!({"reply_parameters":reply});
            fake.clear_calls();
            let response = call(
                app,
                "POST",
                path,
                Some(json!({"post":post,"chat_id":"-1001"})),
                &[("cookie", &who)],
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{path} encoded={encoded}: {}",
                response.text()
            );
            assert!(
                !fake
                    .methods()
                    .iter()
                    .any(|method| method.starts_with("send")),
                "{path} encoded={encoded}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn reply_source_guard_applies_to_every_transport_method() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let context = AccountContext::account(&settings, 101, Role::Publish);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    for method in ["sendPoll", "sendLivePhoto", "sendPaidMedia"] {
        let error = transport
            .call(
                &context,
                method,
                json!({"chat_id":101,"reply_parameters":{"chat_id":-100999}}),
                Uploads::default(),
            )
            .await
            .expect_err("cross-account reply source");
        assert!(
            matches!(&error, TelegramError::Policy(_)),
            "{method}: {error}"
        );
        assert!(error.to_string().contains("привяжите"), "{method}: {error}");
    }
    Ok(())
}

#[tokio::test]
async fn reply_to_own_linked_channel_is_allowed_and_rechecks_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(json!({
            "id":-1001,"type":"channel","title":"Мой","username":"mine"
        }))],
    );
    connect_channel(&app, &who).await;

    for source in [json!(-1001), json!("@mine")] {
        let mut post = blocks_post();
        post["options"] = json!({"reply_parameters":{"chat_id":source,"message_id":1}});
        fake.clear_calls();
        let reply = call(
            app.clone(),
            "POST",
            "/api/preview-send",
            Some(json!({"post":post.clone()})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(
            fake.last("sendRichMessage")["body"]["reply_parameters"]["chat_id"],
            json!(-1001)
        );

        fake.queue(
            "getChatMember",
            vec![ok_envelope(json!({"status":"member"}))],
        );
        fake.clear_calls();
        let refused = call(
            app.clone(),
            "POST",
            "/api/preview-send",
            Some(json!({"post":post})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(fake.count("sendRichMessage"), 0);
    }
    Ok(())
}

#[tokio::test]
async fn user_operation_right_is_required_even_when_bot_has_it() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let context = AccountContext::account(&settings, 101, Role::Publish);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());

    for (method, right) in [
        ("createChatInviteLink", "can_invite_users"),
        ("editMessageText", "can_edit_messages"),
        ("deleteMessage", "can_delete_messages"),
        ("pinChatMessage", "can_edit_messages"),
    ] {
        fake.queue(
            "getChatMember",
            vec![ok_envelope(
                json!({"status":"administrator","can_post_messages":true}),
            )],
        );
        let error = transport
            .call(
                &context,
                method,
                json!({"chat_id":-1001}),
                Uploads::default(),
            )
            .await
            .expect_err("missing user right");
        assert!(
            error.to_string().contains(right),
            "{method}: expected {right}, got {error}"
        );

        let mut member = json!({"status":"administrator","can_post_messages":true});
        member[right] = json!(true);
        fake.queue("getChatMember", vec![ok_envelope(member)]);
        transport
            .call(
                &context,
                method,
                json!({"chat_id":-1001}),
                Uploads::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("{method}: {error}"));
        assert_eq!(fake.last(method)["body"]["chat_id"], json!(-1001));
    }
    Ok(())
}

#[tokio::test]
async fn operator_endpoints_and_logs_are_unavailable_to_users() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    for path in ["/api/logs", "/api/export"] {
        assert_eq!(
            call(app.clone(), "GET", path, None, &[("cookie", &who)])
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    for path in ["/api/raw/getMe", "/api/spec/refresh"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                path,
                Some(json!({})),
                &[("cookie", &who)]
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let health = call(app.clone(), "GET", "/api/health", None, &[("cookie", &who)])
        .await
        .json();
    assert_eq!(health["metrics"], json!({}));
    assert_eq!(health["recent_problems"], json!([]));
    let names: BTreeSet<String> = health["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|check| check["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "scheduler".to_owned(),
            "posts".to_owned(),
            "sends".to_owned(),
            "spec".to_owned(),
        ])
    );
    let mcp = call(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"studio_logs","arguments":{}}})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(mcp.json()["result"]["isError"], json!(true));
    Ok(())
}

#[tokio::test]
async fn paid_broadcast_is_refused_before_sending() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let mut post = blocks_post();
    post["options"] = json!({"allow_paid_broadcast":true});
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":post,"chat_id":"-1001"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn scheduler_handles_accounts_and_revoked_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    connect_channel(&app, &first).await;
    let second = login_as(&app, 202).await;
    connect_channel(&app, &second).await;
    let at = auth::now();
    for who in [&first, &second] {
        let reply = call(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"chat_id":"-1001","post":blocks_post(),"at":at})),
            &[("cookie", who)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        scheduler::run_all(&state, Arc::new(AtomicBool::new(false))).await?,
        2
    );
    let first_posts = call(
        app.clone(),
        "GET",
        "/api/posts",
        None,
        &[("cookie", &first)],
    )
    .await
    .json();
    assert_eq!(first_posts[0]["status"], json!("failed"));
    let second_posts = call(app, "GET", "/api/posts", None, &[("cookie", &second)])
        .await
        .json();
    assert_eq!(second_posts[0]["status"], json!("sent"));
    assert!(posts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn telegram_events_reach_only_the_owning_account() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    connect_channel(&app, &first).await;
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"chat_id":"-1001","post":blocks_post()})),
        &[("cookie", &first)],
    )
    .await
    .json();
    let post = sent["post"].clone();
    let id = post["id"].as_str().expect("post id");
    let message_id = post["message_id"].as_i64().expect("message id");
    let second = login_as(&app, 202).await;

    let event = json!({
        "message_reaction_count": {
            "chat": {"id": -1001},
            "message_id": message_id,
            "reactions": [{"type": {"type": "emoji", "emoji": "👍"}, "total_count": 3}],
        }
    });
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("reactions")
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}"),
            None,
            &[("cookie", &first)]
        )
        .await
        .json()["reactions"],
        json!({"👍":3})
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/posts",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert!(posts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn adding_bot_connects_channel_to_the_actor() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    let second = login_as(&app, 202).await;
    let event = json!({
        "my_chat_member": {
            "from": {"id": 101},
            "chat": {"id": -1001},
            "new_chat_member": {"status": "administrator"}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("channel_connected")
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/channels",
            None,
            &[("cookie", &first)]
        )
        .await
        .json()
        .as_array()
        .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &second)])
            .await
            .json(),
        json!([])
    );
    assert!(channels::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn parallel_requests_keep_their_account_context() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let first = login_as(&app, 101).await;
    let second = login_as(&app, 202).await;
    let mut tasks = Vec::new();
    for index in 0..12 {
        let app = app.clone();
        let who = if index % 2 == 1 {
            first.clone()
        } else {
            second.clone()
        };
        let title = (index % 2).to_string();
        tasks.push(tokio::spawn(async move {
            let reply = call(
                app,
                "POST",
                "/api/drafts",
                Some(json!({"title":title,"post":blocks_post()})),
                &[("cookie", &who)],
            )
            .await;
            (reply.status, reply.json())
        }));
    }
    for task in tasks {
        let (status, body) = task.await?;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let first_reply = call(
        app.clone(),
        "GET",
        "/api/drafts",
        None,
        &[("cookie", &first)],
    )
    .await
    .json();
    let first_titles: Vec<String> = first_reply
        .as_array()
        .expect("drafts")
        .iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(first_titles, vec!["1".to_owned(); 6]);
    let second_reply = call(app, "GET", "/api/drafts", None, &[("cookie", &second)])
        .await
        .json();
    let second_titles: Vec<String> = second_reply
        .as_array()
        .expect("drafts")
        .iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(second_titles, vec!["0".to_owned(); 6]);
    assert!(drafts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn partial_send_keeps_success_and_idempotency_after_rights_are_revoked() -> TestResult {
    for first_revoked in [false, true] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let settings = public_config(&dir, &base);
        let app = build_router(AppState::new(settings));

        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        fake.queue(
            "getChat",
            vec![ok_envelope(
                json!({"id":-1002,"type":"channel","title":"Второй"}),
            )],
        );
        let second = call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@second"})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.text());

        let mut members = vec![
            ok_envelope(json!({"status":"administrator","can_post_messages":true})),
            ok_envelope(json!({"status":"member"})),
        ];
        if first_revoked {
            members.reverse();
        }
        fake.queue("getChatMember", members);
        let headers = [("cookie", who.as_str()), ("idempotency-key", "partial")];
        let body = json!({"post":blocks_post(),"chat_ids":["-1001","-1002"]});
        let response = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(body.clone()),
            &headers,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text());
        let first = response.json();
        let oks: Vec<bool> = first["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|result| result["ok"].as_bool())
            .collect();
        assert_eq!(
            oks,
            if first_revoked {
                vec![false, true]
            } else {
                vec![true, false]
            }
        );
        let sent = fake.count("sendRichMessage");
        assert_eq!(sent, 1);
        let replay = call(app, "POST", "/api/send", Some(body), &headers).await;
        assert_eq!(replay.json(), first);
        assert_eq!(fake.count("sendRichMessage"), 1);
    }
    Ok(())
}

#[tokio::test]
async fn quota_removes_partial_upload_and_other_accounts_have_their_own_quota() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut settings = public_config(&dir, &base);
    settings.account_storage_mb = 4.0 / (1024.0 * 1024.0);
    let app = build_router(AppState::new(settings));

    let first = login_as(&app, 101).await;
    assert_eq!(
        upload(app.clone(), &first, "one.txt", b"1234").await.status,
        StatusCode::OK
    );
    assert_eq!(
        upload(app.clone(), &first, "two.txt", b"x").await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let names: Vec<String> = call(
        app.clone(),
        "GET",
        "/api/media",
        None,
        &[("cookie", &first)],
    )
    .await
    .json()
    .as_array()
    .expect("media")
    .iter()
    .filter_map(|file| file["name"].as_str().map(str::to_owned))
    .collect();
    assert_eq!(names, vec!["one.txt".to_owned()]);

    let second = login_as(&app, 202).await;
    assert_eq!(
        upload(app, &second, "two.txt", b"1234").await.status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn login_flood_is_limited_and_logout_still_works() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    let mut last = None;
    for _ in 0..30 {
        last = Some(
            call(
                app.clone(),
                "POST",
                "/api/auth/telegram/start",
                Some(json!({})),
                &[],
            )
            .await,
        );
    }
    let last = last.expect("last challenge");
    assert_eq!(last.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(last.header("retry-after"), "60");
    assert_eq!(
        call(app, "DELETE", "/api/session", None, &[("cookie", &who)])
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[test]
fn public_requires_https_server_configuration() {
    let error = Settings::from_map(&BTreeMap::from([(
        "STUDIO_PUBLIC".to_owned(),
        "on".to_owned(),
    )]))
    .expect_err("public mode without a server address");
    assert!(
        error
            .problems
            .iter()
            .any(|problem| problem.contains("STUDIO_MODE=server")),
        "{error}"
    );
    let config = Settings::from_map(&BTreeMap::from([
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        ("STUDIO_URL".to_owned(), "https://studio.example".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
    ]))
    .expect("valid public configuration");
    assert!(config.public);
    assert_eq!(config.bot_username, "YourBot");
}

#[tokio::test]
async fn operator_export_includes_account_databases_and_media() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    let created = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Мой пост","post":blocks_post()})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    assert_eq!(
        upload(app.clone(), &who, "mine.txt", b"mine").await.status,
        StatusCode::OK
    );
    auth::add_token(&settings, "owner", "owner-secret", "admin")?;
    let exported = call(
        app,
        "GET",
        "/api/export",
        None,
        &[("authorization", "Bearer owner-secret")],
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK, "{}", exported.text());
    let mut archive = zip::ZipArchive::new(Cursor::new(exported.bytes))?;
    let names: BTreeSet<String> = archive
        .file_names()
        .map(|name| name.map(|name| name.into_owned()))
        .collect::<Result<_, _>>()?;
    for expected in [
        "studio.db",
        "accounts/101/studio.db",
        "accounts/101/media/mine.txt",
    ] {
        assert!(names.contains(expected), "{expected}: {names:?}");
    }
    let mut media = Vec::new();
    archive
        .by_name("accounts/101/media/mine.txt")?
        .read_to_end(&mut media)?;
    assert_eq!(media, b"mine");
    Ok(())
}

#[tokio::test]
async fn landing_host_and_app_host_are_separate() -> TestResult {
    let dir = TempDir::new()?;
    let settings = config(
        dir.path(),
        &[
            ("STUDIO_MODE", "server"),
            ("STUDIO_PUBLIC", "on"),
            ("STUDIO_URL", "https://app.example.com"),
            ("STUDIO_LANDING_URL", "https://example.com"),
            ("TG_BOT_TOKEN", BOT_TOKEN),
            ("TG_API_BASE", "http://127.0.0.1:9"),
            ("TG_UPDATES", "off"),
            ("STUDIO_BACKGROUND", "off"),
            ("SPEC_PIN", "on"),
        ],
    );
    let app = build_router(AppState::new(settings));

    let landing = call_on(app.clone(), "GET", "/", "example.com", None, &[]).await;
    assert_eq!(landing.status, StatusCode::OK);
    let landing_text = landing.text();
    assert!(landing_text.contains("Хороший пост."), "{landing_text}");
    assert!(
        landing_text.contains("href=\"https://app.example.com?lang=ru\""),
        "{landing_text}"
    );
    assert!(!landing_text.contains("id=\"title\""), "{landing_text}");
    assert!(
        !landing
            .header("content-security-policy")
            .contains("https://telegram.org")
    );

    let application = call_on(app.clone(), "GET", "/", "app.example.com", None, &[]).await;
    assert_eq!(application.status, StatusCode::OK);
    assert!(application.text().contains("id=\"title\""));
    assert!(
        application
            .header("content-security-policy")
            .contains("https://telegram.org")
    );

    let session = call_on(app.clone(), "GET", "/api/session", "example.com", None, &[]).await;
    assert_eq!(session.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        session.header("location"),
        "https://app.example.com/api/session"
    );

    assert_eq!(
        call_on(
            app.clone(),
            "POST",
            "/api/auth/telegram/start",
            "example.com",
            Some(json!({})),
            &[]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let start = call_on(
        app,
        "POST",
        "/api/auth/telegram/start",
        "app.example.com",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(start.status, StatusCode::OK);
    assert!(
        start.json()["auth_url"]
            .as_str()
            .expect("auth_url")
            .starts_with("https://app.example.com/auth/telegram/callback?"),
        "{}",
        start.text()
    );
    assert!(
        !start
            .header("set-cookie")
            .to_ascii_lowercase()
            .contains("domain=")
    );
    Ok(())
}

#[test]
fn landing_configuration_requires_a_separate_https_host() {
    let base = BTreeMap::from([
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        (
            "STUDIO_URL".to_owned(),
            "https://app.example.com".to_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
    ]);
    let mut values = base.clone();
    values.insert(
        "STUDIO_LANDING_URL".to_owned(),
        "https://example.com".to_owned(),
    );
    let config = Settings::from_map(&values).expect("landing configuration");
    assert!(
        config.allowed_hosts.contains("example.com")
            && config.allowed_hosts.contains("app.example.com")
    );
    assert!(config.is_landing_host("example.com"));
    assert!(!config.is_landing_host("app.example.com"));

    for url in [
        "http://example.com",
        "https://app.example.com",
        "https://example.com/sub",
        "https://user@example.com",
    ] {
        let mut values = base.clone();
        values.insert("STUDIO_LANDING_URL".to_owned(), url.to_owned());
        let error = Settings::from_map(&values).expect_err(url);
        assert!(
            error
                .problems
                .iter()
                .any(|problem| problem.contains("STUDIO_LANDING_URL")),
            "{url}: {error}"
        );
    }
}

#[tokio::test]
async fn username_and_numeric_id_of_one_channel_do_not_send_twice() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(json!({
            "id":-1001,"type":"channel","title":"Мой канал","username":"mine"
        }))],
    );
    connect_channel(&app, &who).await;
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@mine","-1001"]})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["results"].as_array().map(Vec::len), Some(1));
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}
