use std::collections::{BTreeMap, VecDeque};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use rich_studio::store::{auth, posts as posts_store};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

pub(crate) const BOT_TOKEN: &str = "1:fixture";

pub(crate) fn settings(dir: &TempDir, public: bool) -> Settings {
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

pub(crate) async fn request(
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

pub(crate) fn cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

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

#[derive(Clone)]
pub(crate) struct FakeTelegram {
    calls: Arc<Mutex<Vec<Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    next_message_id: Arc<AtomicI64>,
}

impl FakeTelegram {
    pub(crate) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
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

    pub(crate) fn answer(&self, method: &str, request: &Value) -> Value {
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

    pub(crate) fn calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    pub(crate) fn clear(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clear();
    }

    pub(crate) fn send_calls(&self) -> Vec<Value> {
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

pub(crate) fn bearer(issued: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "Bearer {}",
        issued["secret"].as_str().ok_or("secret")?
    ))
}

pub(crate) async fn login_as(
    app: &axum::Router,
    uid: i64,
) -> Result<String, Box<dyn std::error::Error>> {
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

pub(crate) async fn issue_token(
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

pub(crate) async fn mcp_call_as(
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

pub(crate) fn post_actors(config: &Settings) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(posts_store::list(&AccountContext::root(config))?
        .iter()
        .map(|post| post["actor"].as_str().unwrap_or_default().to_owned())
        .collect())
}

pub(crate) fn cli_settings(dir: &TempDir) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))?)
}

pub(crate) fn run_cli(dir: &TempDir, arguments: &[&str]) -> std::process::Output {
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

pub(crate) fn forwarded_headers(
    values: &[&str],
) -> Result<axum::http::HeaderMap, Box<dyn std::error::Error>> {
    let mut headers = axum::http::HeaderMap::new();
    for value in values {
        headers.append("x-forwarded-for", value.parse()?);
    }
    Ok(headers)
}

pub(crate) async fn login_from(
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

pub(crate) fn signed_callback(state: &str, uid: i64, token: &str) -> String {
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
