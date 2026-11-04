use std::collections::{BTreeMap, VecDeque};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::account_tokens;
use rich_studio::services::auth as service_auth;
use rich_studio::settings::Settings;
use rich_studio::store::{auth, posts as posts_store};
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const BOT_TOKEN: &str = "1:fixture";

fn settings(dir: &TempDir, public: bool) -> Settings {
    let mut values = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_API_KEY".to_owned(), "owner-secret".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), BOT_TOKEN.to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@test_channel".to_owned()),
        ("TG_PREVIEW_CHAT".to_owned(), "777".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]);
    if public {
        for (key, value) in [
            ("STUDIO_MODE", "server"),
            ("STUDIO_PUBLIC", "on"),
            ("TG_BOT_TOKEN", "1:fixture"),
            ("TG_BOT_USERNAME", "FixtureBot"),
            ("STUDIO_URL", "https://studio.example"),
        ] {
            values.insert(key.to_owned(), value.to_owned());
        }
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}"))
}

async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(
            builder
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body, headers)
}

fn cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_owned()
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn blocks_post() -> Value {
    json!({
        "rich_message": {
            "blocks": [
                {"type": "heading", "size": 1, "text": "Тест"},
                {"type": "paragraph", "text": ["Привет, ", {"type": "bold", "text": "мир"}]}
            ]
        }
    })
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
            "getMe" => json!({"id":42,"username":"studio_bot"}),
            "getChat" => {
                json!({"id":-1001,"type":"channel","title":"Мой","username":"mine"})
            }
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "sendRichMessage" => {
                let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({"message_id":message_id,"chat":{"id":request["chat_id"].clone()},"rich_message":{"blocks":[]}})
            }
            "sendPoll" => {
                let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({"message_id":message_id,"chat":{"id":request["chat_id"].clone()},"poll":{"id":format!("poll{message_id}")}})
            }
            _ => json!(true),
        }
    }

    fn calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    fn clear(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clear();
    }

    fn send_calls(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|call| {
                call["method"]
                    .as_str()
                    .is_some_and(|method| method.starts_with("send"))
            })
            .collect()
    }
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

fn bearer(issued: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "Bearer {}",
        issued["secret"].as_str().ok_or("secret")?
    ))
}

async fn login_as(app: &axum::Router, uid: i64) -> Result<String, Box<dyn std::error::Error>> {
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        json!({}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let state = body["auth_url"]
        .as_str()
        .and_then(|url| url.split("state=").nth(1))
        .ok_or("state")?;
    let login_cookie = cookie(&headers);
    let callback = signed_callback(state, uid, BOT_TOKEN);
    let (status, body, headers) = request(
        app.clone(),
        "GET",
        &callback,
        Value::Null,
        &[("cookie", &login_cookie)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    Ok(cookie(&headers))
}

async fn issue_token(
    app: &axum::Router,
    login: &str,
    body: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let (status, created, headers) = request(
        app.clone(),
        "POST",
        "/api/account/tokens",
        body,
        &[("cookie", login)],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    Ok(created)
}

async fn mcp_call_as(
    app: axum::Router,
    headers: &[(&str, &str)],
    tool: &str,
    arguments: Value,
) -> (Value, String) {
    let (status, body, _) = request(
        app,
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
        headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result = body["result"].clone();
    let text = result["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or("")
        .to_owned();
    (result, text)
}

fn post_actors(config: &Settings) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(posts_store::list(&AccountContext::root(config))?
        .iter()
        .map(|post| post["actor"].as_str().unwrap_or_default().to_owned())
        .collect())
}

fn cli_settings(dir: &TempDir) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))?)
}

fn run_cli(dir: &TempDir, arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rich-studio"));
    command.args(arguments);
    command.env("DATA_DIR", dir.path());
    command.env("STUDIO_BACKGROUND", "off");
    command.env("TG_UPDATES", "off");
    for name in [
        "STUDIO_API_KEY",
        "STUDIO_API_KEY_FILE",
        "STUDIO_MODE",
        "STUDIO_PUBLIC",
        "STUDIO_TOKEN",
        "STUDIO_URL",
        "TG_API_BASE",
        "TG_BOT_TOKEN",
        "TG_BOT_TOKEN_FILE",
        "TG_DEFAULT_CHAT",
        "TG_OWNER_ID",
        "TG_PREVIEW_CHAT",
        "ALLOWED_HOSTS",
    ] {
        command.env_remove(name);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("cli: {error}"))
}

fn forwarded_headers(values: &[&str]) -> Result<axum::http::HeaderMap, Box<dyn std::error::Error>> {
    let mut headers = axum::http::HeaderMap::new();
    for value in values {
        headers.append("x-forwarded-for", value.parse()?);
    }
    Ok(headers)
}

async fn login_from(
    app: axum::Router,
    peer: &str,
    forwarded: &[&str],
) -> Result<StatusCode, Box<dyn std::error::Error>> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/session")
        .header("host", "studio.example")
        .header("content-type", "application/json")
        .body(Body::from(json!({"key":"incorrect-key"}).to_string()))?;
    request.headers_mut().extend(forwarded_headers(forwarded)?);
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>()?,
    ));
    Ok(app.oneshot(request).await?.status())
}

#[tokio::test]
async fn trusted_proxy_keeps_visitor_login_budgets_separate_and_direct_spoofing_cannot_reset_them()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    for _ in 0..30 {
        assert_eq!(
            login_from(app.clone(), "127.0.0.1:5600", &["203.0.113.10"]).await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5601", &["203.0.113.10"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5602", &["203.0.113.11"]).await?,
        StatusCode::UNAUTHORIZED
    );
    // An untrusted direct client cannot get fresh buckets by rotating XFF.
    for host in 1..=30 {
        assert_eq!(
            login_from(
                app.clone(),
                "198.51.100.20:5603",
                &[&format!("192.0.2.{host}")]
            )
            .await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "198.51.100.20:5604", &["192.0.2.31"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app, "198.51.100.21:5605", &["192.0.2.31"]).await?,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn proxy_chain_budget_stops_at_first_untrusted_hop_and_invalid_headers_use_peer()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8".parse()?;
    for key in ["login:203.0.113.9", "login:10.1.2.3"] {
        for _ in 0..30 {
            assert!(auth::take_budget(&config, key, 30, 60)?);
        }
    }
    let app = build_router(AppState::new(config));
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.1, 203.0.113.9", "10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.2, 203.0.113.9, 10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "10.1.2.3:5000", &["203.0.113.10, 10.30.40.50"]).await?,
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        "",
        "unknown",
        "192.0.2.1,",
        "203.0.113.10, bad-proxy",
        "999.1.1.1",
        "192.0.2.1:99999",
    ] {
        assert_eq!(
            login_from(app.clone(), "10.1.2.3:5001", &[invalid]).await?,
            StatusCode::TOO_MANY_REQUESTS,
            "invalid XFF: {invalid:?}"
        );
    }
    assert_eq!(
        login_from(app, "10.1.2.3:5002", &[]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    Ok(())
}

#[test]
fn forwarded_address_policy_covers_ipv6_cidrs_wildcard_and_invalid_settings()
-> Result<(), Box<dyn std::error::Error>> {
    use rich_studio::security::client_ip;
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    assert!(config.forwarded_allow_ips.contains("127.0.0.1".parse()?));
    assert!(!config.forwarded_allow_ips.contains("::1".parse()?));
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8,::1".parse()?;
    for (peer, forwarded, expected) in [
        (
            "10.1.2.3:5000",
            "192.0.2.1, 203.0.113.2, 10.20.30.40",
            "203.0.113.2",
        ),
        ("10.1.2.3:5000", "10.2.3.4, 10.3.4.5", "10.2.3.4"),
        ("[fd00::2]:5000", "2001:db8::4, fd00::3", "2001:db8::4"),
        (
            "[::1]:5000",
            "[2001:db8::5]:9876, [fd00::3]:4321",
            "2001:db8::5",
        ),
        ("[::1]:5000", "[2001:db8::5]", "2001:db8::5"),
        ("10.1.2.3:5000", "203.0.113.6:9876, 10.2.3.4", "203.0.113.6"),
        ("[fe00::1]:5000", "2001:db8::5", "fe00::1"),
    ] {
        assert_eq!(
            client_ip(
                &config,
                &forwarded_headers(&[forwarded])?,
                Some(peer.parse()?)
            ),
            Some(expected.parse()?),
            "{peer}: {forwarded}"
        );
    }
    let mut invalid = axum::http::HeaderMap::new();
    invalid.insert(
        "x-forwarded-for",
        axum::http::HeaderValue::from_bytes(&[0xff])?,
    );
    assert_eq!(
        client_ip(&config, &invalid, Some("10.1.2.3:5000".parse()?)),
        Some("10.1.2.3".parse()?)
    );
    config.forwarded_allow_ips = "*".parse()?;
    let headers = forwarded_headers(&["192.0.2.1, 203.0.113.2"])?;
    assert_eq!(
        client_ip(&config, &headers, Some("198.51.100.1:50".parse()?)),
        Some("192.0.2.1".parse()?)
    );
    assert_eq!(client_ip(&config, &headers, None), None);
    config.forwarded_allow_ips = "".parse()?;
    assert_eq!(
        client_ip(&config, &headers, Some("127.0.0.1:50".parse()?)),
        Some("127.0.0.1".parse()?)
    );
    for raw in [
        "127.0.0.1/33",
        "fd00::/129",
        "localhost",
        "10.0.0.0/bad",
        "*,127.0.0.1",
    ] {
        assert!(
            Settings::from_map(&BTreeMap::from([(
                "FORWARDED_ALLOW_IPS".to_owned(),
                raw.to_owned()
            )]))
            .is_err(),
            "accepted malformed proxy setting: {raw}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_key_is_enforced_and_legacy_key_session_dies_on_rotation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"name":null,"level":"admin"}));
    let session = cookie(&headers);
    assert!(!session.contains("owner-secret"));
    assert!(headers["set-cookie"].to_str()?.contains("HttpOnly"));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::OK
    );
    config.api_key = "rotated".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_sessions_tokens_and_drafts_are_isolated_over_http()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    for uid in [101, 202] {
        auth::save_account(
            &config,
            &json!({"id":uid,"first_name":format!("User {uid}")}),
        )?;
        auth::add_account_session(&config, &format!("session-{uid}"), uid, 3600)?;
    }
    let app = build_router(AppState::new(config.clone()));
    let one = [("cookie", "rs_session=session-101")];
    let two = [("cookie", "rs_session=session-202")];
    let (status, draft, _) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        json!({"title":"Private","post":{"rich_message":{"markdown":"secret"}}}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = format!("/api/drafts/{}", draft["id"].as_str().ok_or("draft id")?);
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &two)
            .await
            .1,
        json!([])
    );
    let (status, token, _) = request(
        app.clone(),
        "POST",
        "/api/account/tokens",
        json!({"name":"Agent","level":"read"}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let bearer = format!("Bearer {}", token["secret"].as_str().ok_or("secret")?);
    let credential = [("authorization", bearer.as_str())];
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"Denied","post":{}}),
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/account/tokens",
            Value::Null,
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":token["secret"]}),
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let token_path = format!(
        "/api/account/tokens/{}",
        token["token"]["id"].as_str().ok_or("token id")?
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/api/session", Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}

fn signed_callback(state: &str, uid: i64, token: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};
    let mut fields = BTreeMap::from([
        ("id", uid.to_string()),
        ("first_name", "User".to_owned()),
        ("auth_date", auth::now().to_string()),
    ]);
    let check = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&Sha256::digest(token.as_bytes()))
        .unwrap_or_else(|error| panic!("hmac: {error}"));
    mac.update(check.as_bytes());
    let hash = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fields.insert("hash", hash);
    fields.insert("state", state.to_owned());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    format!("/auth/telegram/callback?{query}")
}

#[tokio::test]
async fn telegram_login_is_signed_browser_bound_one_use_and_owner_stays_root()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 101;
    let app = build_router(AppState::new(config.clone()));
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        json!({}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = body["auth_url"]
        .as_str()
        .and_then(|url| url.split("state=").nth(1))
        .ok_or("state")?;
    let login_cookie = cookie(&headers);
    let callback = signed_callback(state, 101, "1:fixture");
    assert_eq!(
        request(app.clone(), "GET", &callback, Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let tampered = signed_callback(state, 101, "wrong-token");
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &tampered,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app.clone(),
        "GET",
        &callback,
        Value::Null,
        &[("cookie", &login_cookie)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let owner_cookie = cookie(&headers);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .1["level"],
        "admin"
    );
    assert!(auth::account(&config, 101)?.is_none());
    assert!(!config.account_dir(101).exists());
    assert_eq!(
        request(
            app,
            "GET",
            &callback,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    config.owner_telegram_id = 202;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_token_limits_expiry_and_management_validation_are_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    auth::save_account(&config, &json!({"id":101,"first_name":"User"}))?;
    auth::add_account_session(&config, "browser", 101, 3600)?;
    let app = build_router(AppState::new(config.clone()));
    let owner = [("cookie", "rs_session=browser")];
    for body in [
        json!({"name":"x","level":"admin"}),
        json!({"name":"x","telegram_id":202}),
        json!({"name":"x","expires_in_days":true}),
        json!({"name":"x","expires_in_days":0}),
        json!({"name":"   ","level":"draft"}),
        json!({"name":"a\nb","level":"draft"}),
        json!({"name":"x".repeat(81),"level":"draft"}),
        json!({"name":"x","expires_in_days":366}),
        json!({"name":"x","secret":"chosen"}),
    ] {
        assert_eq!(
            request(app.clone(), "POST", "/api/account/tokens", body, &owner)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut first = Value::Null;
    let mut second = Value::Null;
    for index in 0..account_tokens::MAX_ACTIVE {
        let (status, token, _) = request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":format!("Agent{index}")}),
            &owner,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        if index == 0 {
            first = token.clone();
        }
        if index == 1 {
            second = token;
        }
    }
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"overflow"}),
            &owner
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    auth::database(&config)?.execute(
        "update account_tokens set expires_at=? where id=?",
        rusqlite::params![auth::now(), first["token"]["id"].as_str()],
    )?;
    let bearer = format!("Bearer {}", first["secret"].as_str().ok_or("secret")?);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("authorization", &bearer)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"replacement"}),
            &owner
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let second_id = second["token"]["id"].as_str().ok_or("token id")?;
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("/api/account/tokens/{second_id}"),
            Value::Null,
            &owner
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, unlimited, _) = request(
        app,
        "POST",
        "/api/account/tokens",
        json!({"name":"unlimited","expires_in_days":null}),
        &owner,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(unlimited["token"]["expires_at"].is_null());
    Ok(())
}

#[tokio::test]
async fn revoked_last_token_does_not_open_service_and_mcp_enforces_roles()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "reader", "read-secret", "read")?;
    let app = build_router(AppState::new(config.clone()));
    let headers = [("authorization", "Bearer read-secret")];
    assert_eq!(
        request(app.clone(), "POST", "/api/send", json!({}), &headers)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"x"}}}}}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["isError"], true);
    auth::revoke_token(&config, "reader")?;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn unsafe_origins_unknown_hosts_and_mcp_publish_off_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.mcp_publish = false;
    let app = build_router(AppState::new(config));
    let owner = [("x-api-key", "owner-secret")];
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({}),
            &[
                ("x-api-key", "owner-secret"),
                ("origin", "http://localhost:9999")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let request_bad_host = Request::builder()
        .uri("/healthz")
        .header("host", "attacker.example")
        .body(Body::empty())?;
    assert_eq!(
        app.clone().oneshot(request_bad_host).await?.status(),
        StatusCode::BAD_REQUEST
    );
    let (_, tools, _) = request(
        app.clone(),
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &owner,
    )
    .await;
    assert!(
        !tools["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "publish_post")
    );
    let (_, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish_post","arguments":{}}}), &owner).await;
    assert_eq!(result["result"]["isError"], true);
    Ok(())
}

#[tokio::test]
async fn open_studio_stays_open_until_a_key_or_token_exists() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::OK
    );
    let (status, session, _) = request(app.clone(), "GET", "/api/session", Value::Null, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session, json!({"name":null,"level":"admin","auth":false}));
    auth::add_token(&config, "agent", "rs_test_agent", "read")?;
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/healthz", Value::Null, &[]).await.0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn token_levels_gate_reads_drafts_publishing_and_x_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    for (name, secret, level) in [
        ("r", "rs_test_r", "read"),
        ("d", "rs_test_d", "draft"),
        ("p", "rs_test_p", "publish"),
    ] {
        auth::add_token(&config, name, secret, level)?;
    }
    let app = build_router(AppState::new(config));
    let read = [("authorization", "Bearer rs_test_r")];
    let draft = [("authorization", "Bearer rs_test_d")];
    let publish = [("authorization", "Bearer rs_test_p")];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"x","post":blocks_post()}),
            &read
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"x","post":blocks_post()}),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, denied, _) = request(
        app.clone(),
        "POST",
        "/api/send",
        json!({"post":blocks_post()}),
        &draft,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        denied["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("publish")
    );
    assert!(fake.calls().is_empty());
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &publish
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", "rs_test_r")]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", "wrong")]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn every_route_that_posts_to_a_channel_requires_publish() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "d", "rs_test_d", "draft")?;
    let app = build_router(AppState::new(config));
    let draft = [("authorization", "Bearer rs_test_d")];
    for (method, path) in [
        ("POST", "/api/send"),
        ("POST", "/api/edit"),
        ("POST", "/api/schedule"),
        ("POST", "/api/poll"),
        ("POST", "/api/posts/x/stop-poll"),
        ("POST", "/api/posts/x/pin"),
        ("DELETE", "/api/posts/x/pin"),
        ("POST", "/api/posts/x/buttons"),
        ("POST", "/api/posts/x/edit"),
        ("POST", "/api/posts/x/reschedule"),
        ("DELETE", "/api/posts/x"),
    ] {
        let body = if method == "POST" {
            json!({})
        } else {
            Value::Null
        };
        assert_eq!(
            request(app.clone(), method, path, body, &draft).await.0,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn preview_to_another_chat_requires_publish() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "d", "rs_test_d", "draft")?;
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    let draft = [("authorization", "Bearer rs_test_d")];
    let publish = [("authorization", "Bearer rs_test_p")];
    let send = json!({"post":blocks_post()});
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            send.clone(),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":" 777"}),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"@chan"}),
            &draft
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"@chan"}),
            &publish
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut no_preview = config;
    no_preview.preview_chat.clear();
    let without_preview = build_router(AppState::new(no_preview));
    assert_eq!(
        request(
            without_preview,
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"777"}),
            &draft
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn actor_is_the_token_name_and_optional_client() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "claude-laptop", "rs_test_claude", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &[
                ("authorization", "Bearer rs_test_claude"),
                ("x-studio-client", "mcp:claude")
            ]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &[("authorization", "Bearer rs_test_claude")]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post_actors(&config)?,
        vec!["claude-laptop", "mcp:claude@claude-laptop"]
    );
    Ok(())
}

#[tokio::test]
async fn revoked_token_and_its_sessions_stop_working() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "gone", "rs_test_gone", "read")?;
    let app = build_router(AppState::new(config.clone()));
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"rs_test_gone"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session = cookie(&headers);
    let read = [("authorization", "Bearer rs_test_gone")];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::OK
    );
    assert!(auth::revoke_token(&config, "gone")?);
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn editor_session_cookie_is_httponly_and_logout_ends_it() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, false);
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":"nope"}),
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let raw = headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .ok_or("set-cookie")?;
    assert!(raw.contains("rs_session="));
    assert!(!raw.contains("owner-secret"));
    assert!(raw.contains("HttpOnly"));
    assert!(raw.contains("SameSite=Strict"));
    assert!(!raw.contains("Secure"));
    let session = cookie(&headers);
    let (status, who, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &session)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["level"], "admin");
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            "/api/session",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app,
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("Secure")
    );
    Ok(())
}

#[tokio::test]
async fn key_session_ends_when_key_changes_or_is_removed() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key = "old".to_owned();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":"old"}),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"old"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session = cookie(&headers);
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut rotated = config.clone();
    rotated.api_key = "new".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(rotated.clone())),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    auth::add_token(&config, "t", "rs_test_t", "read")?;
    let mut removed = rotated;
    removed.api_key.clear();
    assert_eq!(
        request(
            build_router(AppState::new(removed)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn session_of_an_unknown_token_is_not_admin() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "t", "rs_test_t", "read")?;
    auth::add_session(&config, "orphan", Some("ghost"), 3600, None, None)?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", "rs_session=orphan")]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn non_ascii_key_is_refused_and_accepted_when_current() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key = "secret".to_owned();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app, "POST", "/api/session", json!({"key":"ключ"}), &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut cyrillic = config;
    cyrillic.api_key = "ключ-ёж".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(cyrillic)),
            "POST",
            "/api/session",
            json!({"key":"ключ-ёж"}),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn raw_api_is_admin_only_and_can_be_switched_off() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    auth::add_token(&config, "a", "rs_test_a", "admin")?;
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_p")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_a")]
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut disabled = config;
    disabled.raw_api = false;
    assert_eq!(
        request(
            build_router(AppState::new(disabled)),
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_a")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn backup_download_is_admin_only() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("authorization", "Bearer rs_test_p")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn mcp_tools_check_token_level() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "agent", "rs_test_agent", "draft")?;
    auth::add_token(&config, "reader", "rs_test_reader", "read")?;
    auth::add_token(&config, "pub", "rs_test_pub", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    let draft = [("authorization", "Bearer rs_test_agent")];
    let read = [("authorization", "Bearer rs_test_reader")];
    let publish = [("authorization", "Bearer rs_test_pub")];
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft,
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let (result, text) = mcp_call_as(
        app.clone(),
        &draft,
        "publish_post",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("publish"));
    assert!(fake.calls().is_empty());
    let (result, _) = mcp_call_as(app.clone(), &read, "list_drafts", json!({})).await;
    assert_eq!(result["isError"], false);
    let (result, text) = mcp_call_as(
        app.clone(),
        &read,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("draft"));
    let (result, _) = mcp_call_as(
        app.clone(),
        &publish,
        "publish_post",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], false);
    assert_eq!(post_actors(&config)?, vec!["mcp@pub"]);
    Ok(())
}

#[test]
fn cli_makes_lists_and_revokes_tokens() -> TestResult {
    let dir = TempDir::new()?;
    let added = run_cli(&dir, &["token", "add", "ci-bot", "--level", "publish"]);
    assert_eq!(added.status.code(), Some(0));
    let secret = String::from_utf8_lossy(&added.stdout).trim().to_owned();
    assert!(secret.starts_with("rs_"));
    let config = cli_settings(&dir)?;
    let principal = service_auth::by_secret(&config, &secret)?.ok_or("token is missing")?;
    assert_eq!(principal.name.as_deref(), Some("ci-bot"));
    assert_eq!(principal.role, Role::Publish);
    let duplicate = run_cli(&dir, &["token", "add", "ci-bot"]);
    assert_eq!(duplicate.status.code(), Some(2));
    let invalid = run_cli(&dir, &["token", "add", "bad name"]);
    assert_eq!(invalid.status.code(), Some(2));
    let listed = run_cli(&dir, &["token", "list"]);
    assert_eq!(listed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("ci-bot"));
    let revoked = run_cli(&dir, &["token", "revoke", "ci-bot"]);
    assert_eq!(revoked.status.code(), Some(0));
    assert!(service_auth::by_secret(&config, &secret)?.is_none());
    let again = run_cli(&dir, &["token", "revoke", "ci-bot"]);
    assert_eq!(again.status.code(), Some(1));
    Ok(())
}

#[tokio::test]
async fn browser_can_issue_list_and_revoke_own_account_token() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.public_url = "https://testserver".to_owned();
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 101).await?;
    let created = issue_token(
        &app,
        &login,
        json!({"name":"Claude laptop","level":"draft"}),
    )
    .await?;
    let secret = created["secret"].as_str().ok_or("secret")?.to_owned();
    let (status, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["mcp_url"], "https://testserver/mcp");
    assert_eq!(listed["tokens"], json!([created["token"].clone()]));
    let listed_text = listed.to_string();
    assert!(!listed_text.contains(&secret));
    assert!(!listed_text.contains("hash"));
    assert!(
        created["token"]["expires_at"]
            .as_i64()
            .ok_or("expires_at")?
            > auth::now()
    );
    let connection = auth::database(&config)?;
    let hash: String =
        connection.query_row("select hash from account_tokens", [], |row| row.get(0))?;
    assert_eq!(hash, auth::hash_secret(&secret));
    let row: String = connection.query_row(
        "select id||telegram_id||name||hash||level||created_at||ifnull(expires_at,'')||ifnull(last_used_at,'')||ifnull(revoked_at,'') from account_tokens",
        [],
        |row| row.get(0),
    )?;
    assert!(!row.contains(&secret));
    let credential = bearer(&created)?;
    let (status, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("authorization", &credential)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(101));
    let login = login_as(&app, 101).await?;
    let (status, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed["tokens"][0]["last_used_at"]
            .as_i64()
            .ok_or("last_used_at")?
            > 0
    );
    let id = created["token"]["id"].as_str().ok_or("token id")?;
    let path = format!("/api/account/tokens/{id}");
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &path,
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &path,
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("authorization", &credential)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn account_tokens_cannot_manage_credentials_or_become_browser_sessions() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    for level in ["read", "draft", "publish"] {
        let created = issue_token(
            &app,
            &login,
            json!({"name":format!("Claude {level}"),"level":level}),
        )
        .await?;
        let credential = bearer(&created)?;
        let headers = [
            ("cookie", login.as_str()),
            ("authorization", credential.as_str()),
        ];
        assert_eq!(
            request(
                app.clone(),
                "GET",
                "/api/account/tokens",
                Value::Null,
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                app.clone(),
                "POST",
                "/api/account/tokens",
                json!({"name":"escalate"}),
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let id = created["token"]["id"].as_str().ok_or("token id")?;
        assert_eq!(
            request(
                app.clone(),
                "DELETE",
                &format!("/api/account/tokens/{id}"),
                Value::Null,
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                app.clone(),
                "POST",
                "/api/session",
                json!({"key":created["secret"]}),
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, session, _) = request(
        app,
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    Ok(())
}

#[tokio::test]
async fn account_isolation_covers_root_tokens_drafts_and_token_ids() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    auth::add_token(&config, "operator", "operator-secret", "admin")?;
    let app = build_router(AppState::new(config));
    let a = login_as(&app, 101).await?;
    let first = issue_token(&app, &a, json!({"name":"Claude laptop"})).await?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"A private","post":blocks_post()}),
            &[("cookie", &a)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let b = login_as(&app, 202).await?;
    let second = issue_token(&app, &b, json!({"name":"Claude laptop"})).await?;
    let (_, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &b)],
    )
    .await;
    assert_eq!(listed["tokens"], json!([second["token"].clone()]));
    let first_id = first["token"]["id"].as_str().ok_or("token id")?;
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("/api/account/tokens/{first_id}"),
            Value::Null,
            &[("cookie", &b)]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/account/tokens",
            Value::Null,
            &[("authorization", "Bearer operator-secret")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let first_bearer = bearer(&first)?;
    let second_bearer = bearer(&second)?;
    let (_, own) = mcp_call_as(
        app.clone(),
        &[("authorization", first_bearer.as_str())],
        "list_drafts",
        json!({}),
    )
    .await;
    let (_, other) = mcp_call_as(
        app.clone(),
        &[("authorization", second_bearer.as_str())],
        "list_drafts",
        json!({}),
    )
    .await;
    let (_, operator) = mcp_call_as(
        app.clone(),
        &[("authorization", "Bearer operator-secret")],
        "list_drafts",
        json!({}),
    )
    .await;
    assert!(own.contains("A private"));
    assert!(!other.contains("A private"));
    assert!(!operator.contains("A private"));
    let first_secret = first["secret"].as_str().ok_or("secret")?;
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", first_secret)]
        )
        .await
        .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn mcp_and_http_enforce_account_roles_and_current_telegram_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, true);
    config.api_base = origin;
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    fake.queue(
        "getChat",
        vec![json!({"ok":true,"result":{"id":-1001,"type":"channel","title":"Мой","username":"mine"}})],
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/channels",
            json!({"chat_id":"@mine"}),
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let read = issue_token(&app, &login, json!({"name":"read","level":"read"})).await?;
    let draft = issue_token(&app, &login, json!({"name":"draft","level":"draft"})).await?;
    let publish = issue_token(&app, &login, json!({"name":"publish","level":"publish"})).await?;
    let read_bearer = bearer(&read)?;
    let draft_bearer = bearer(&draft)?;
    let publish_bearer = bearer(&publish)?;
    let read_headers = [("authorization", read_bearer.as_str())];
    let draft_headers = [("authorization", draft_bearer.as_str())];
    let publish_headers = [("authorization", publish_bearer.as_str())];
    let (result, _) = mcp_call_as(app.clone(), &read_headers, "list_drafts", json!({})).await;
    assert_eq!(result["isError"], false);
    let (result, _) = mcp_call_as(
        app.clone(),
        &read_headers,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft_headers,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &draft_headers
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for headers in [
        read_headers.as_slice(),
        draft_headers.as_slice(),
        publish_headers.as_slice(),
    ] {
        assert_eq!(
            request(app.clone(), "GET", "/api/logs", Value::Null, headers)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(app.clone(), "POST", "/api/raw/getMe", json!({}), headers)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (result, _) = mcp_call_as(app.clone(), headers, "studio_logs", json!({})).await;
        assert_eq!(result["isError"], true);
    }
    let (result, _) = mcp_call_as(
        app.clone(),
        &publish_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    fake.clear();
    fake.queue(
        "getChatMember",
        vec![json!({"ok":true,"result":{"status":"member"}})],
    );
    let (result, _) = mcp_call_as(
        app,
        &publish_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine","idempotency_key":"new"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(fake.send_calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn account_token_expiration_and_personal_mode_are_checked_per_request() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 101).await?;
    let created = issue_token(&app, &login, json!({"name":"Claude laptop"})).await?;
    let credential = bearer(&created)?;
    let headers = [("authorization", credential.as_str())];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &headers)
            .await
            .0,
        StatusCode::OK
    );
    let mut personal = config.clone();
    personal.public = false;
    auth::add_token(&personal, "keep-auth-on", "keep-auth-secret", "read")?;
    assert_eq!(
        request(
            build_router(AppState::new(personal)),
            "GET",
            "/api/drafts",
            Value::Null,
            &headers
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let connection = auth::database(&config)?;
    connection.execute(
        "update account_tokens set expires_at=?",
        rusqlite::params![auth::now()],
    )?;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &headers
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_token_management_requires_telegram_login_and_same_origin() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(app.clone(), "GET", "/api/account/tokens", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let login = login_as(&app, 101).await?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"x"}),
            &[
                ("cookie", login.as_str()),
                ("origin", "https://evil.example")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "DELETE",
            "/api/account/tokens/unknown",
            Value::Null,
            &[("cookie", &login), ("sec-fetch-site", "cross-site")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn owner_login_opens_root_admin_without_public_account() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    let (status, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["name"], Value::Null);
    assert_eq!(session["level"], "admin");
    assert!(session.get("user").is_none());
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(auth::account(&config, 555)?.is_none());
    assert!(!auth::account_ids(&config)?.contains(&555));
    assert!(!config.account_dir(555).exists());
    let connection = auth::database(&config)?;
    let sessions: i64 = connection.query_row(
        "select count(*) from account_sessions where telegram_id=?",
        [555],
        |row| row.get(0),
    )?;
    assert_eq!(sessions, 0);
    Ok(())
}

#[tokio::test]
async fn regular_user_alongside_configured_owner_stays_public() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    let (_, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(101));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn same_telegram_id_is_regular_without_owner_setting() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    let (_, session, _) = request(
        app,
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(555));
    assert!(auth::account_ids(&config)?.contains(&555));
    Ok(())
}

#[tokio::test]
async fn owner_session_dies_when_owner_setting_changes() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    assert_eq!(
        request(
            app,
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut cleared = config.clone();
    cleared.owner_telegram_id = 0;
    assert_eq!(
        request(
            build_router(AppState::new(cleared)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mut other = config;
    other.owner_telegram_id = 777;
    assert_eq!(
        request(
            build_router(AppState::new(other)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn auth_config_reports_owner_login() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let (status, body, _) = request(app, "GET", "/api/auth/config", Value::Null, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["owner_login"], false);
    let mut owner = config;
    owner.owner_telegram_id = 555;
    let (status, body, _) = request(
        build_router(AppState::new(owner)),
        "GET",
        "/api/auth/config",
        Value::Null,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["owner_login"], true);
    Ok(())
}
