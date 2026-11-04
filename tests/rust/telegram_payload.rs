use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::logs::{scrub, scrub_all};
use rich_studio::services::media::MediaService;
use rich_studio::settings::Settings;
use rich_studio::telegram::TelegramError;
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::{Uploads, collect_uploads};
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

const TOKEN: &str = "123:ABC-def_9";

fn context(dir: &TempDir) -> AccountContext {
    let settings = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    AccountContext::root(&settings)
}

async fn server(responses: Vec<(u16, String)>) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let (send, receive) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("accept: {error}"));
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            let expected = loop {
                let count = socket
                    .read(&mut buffer)
                    .await
                    .unwrap_or_else(|error| panic!("read: {error}"));
                if count == 0 {
                    break None;
                }
                data.extend_from_slice(&buffer[..count]);
                if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let total = end + 4 + length;
                    if data.len() >= total {
                        break Some(total);
                    }
                }
            };
            let expected = expected.unwrap_or(data.len());
            send.send(String::from_utf8_lossy(&data[..expected]).into_owned())
                .unwrap_or_else(|error| panic!("record: {error}"));
            let reason = if status == 200 { "OK" } else { "ERROR" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("write: {error}"));
        }
    });
    (format!("http://{address}"), receive)
}

#[test]
fn uploads_rewrite_only_file_fields_and_deduplicate() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    MediaService::new(context.clone(), 50.0, 500.0).upload("pic.png", b"png")?;
    let payload = json!({
        "text":"upload:pic.png остаётся текстом",
        "rich_message":{"future":17,"blocks":[
            {"photo":{"media":"upload:pic.png"}},
            {"photo":{"media":"upload:pic.png"}},
            {"media":"upload:pic.png"}
        ]}
    });
    let mut uploads = Uploads::default();
    let changed = collect_uploads(&payload, &context.media_dir, &mut uploads)?;
    assert_eq!(changed["text"], payload["text"]);
    assert_eq!(changed["rich_message"]["future"], 17);
    assert_eq!(
        changed["rich_message"]["blocks"][0]["photo"]["media"],
        "attach://f0"
    );
    assert_eq!(changed["rich_message"]["blocks"][2]["media"], "attach://f0");
    assert_eq!(uploads.files.len(), 1);
    let bad = json!({"media":"upload:../outside"});
    assert!(collect_uploads(&bad, &context.media_dir, &mut Uploads::default()).is_err());
    Ok(())
}

#[tokio::test]
async fn json_and_multipart_keep_the_payload_and_hide_the_token()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let token = "123:secret-token";
    let (base, mut requests) = server(vec![
        (200, json!({"ok":true,"result":{"echo":true}}).to_string()),
        (200, json!({"ok":true,"result":true}).to_string()),
        (400, format!("not-json {token}")),
    ])
    .await;
    let transport = TelegramTransport::new(base, token.to_owned());
    let payload = json!({"chat_id":"@канал","future_field":{"big":9007199254740993u64},"rich_message":{"html":"<p>Привет</p>"}});
    let result = transport
        .call(
            &context,
            "futureMethod",
            payload.clone(),
            Uploads::default(),
        )
        .await?;
    assert_eq!(result, json!({"echo":true}));
    let first = requests.recv().await.ok_or("json request")?;
    assert!(first.contains("futureMethod"));
    let body: Value = serde_json::from_slice(
        first
            .split("\r\n\r\n")
            .nth(1)
            .ok_or("json body")?
            .as_bytes(),
    )?;
    assert_eq!(body, payload);

    MediaService::new(context.clone(), 50.0, 500.0).upload("pic.png", b"png")?;
    let mut uploads = Uploads::default();
    let payload = collect_uploads(
        &json!({"media":"upload:pic.png","options":{"unicode":"Привет"}}),
        &context.media_dir,
        &mut uploads,
    )?;
    transport
        .call(&context, "sendFuture", payload, uploads)
        .await?;
    let multipart = requests.recv().await.ok_or("multipart request")?;
    assert!(multipart.contains("attach://f0") && multipart.contains("Привет"));
    assert_eq!(multipart.matches("filename=\"pic.png\"").count(), 1);

    let error = transport
        .call(&context, "badMethod", json!({}), Uploads::default())
        .await
        .unwrap_err();
    assert!(!error.to_string().contains(token));
    assert!(error.to_string().contains("<token>"));
    Ok(())
}

#[tokio::test]
async fn transport_distinguishes_refusal_rate_limit_unreachable_and_no_answer()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let (base, _requests) = server(vec![
        (400, json!({"ok":false,"error_code":400,"description":"Bad Request: refused"}).to_string()),
        (429, json!({"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":600}}).to_string()),
        (502, "gateway".to_owned()),
    ]).await;
    let transport = TelegramTransport::new(base, "1:x".to_owned());
    assert!(matches!(
        transport
            .call(&context, "sendMessage", json!({}), Uploads::default())
            .await,
        Err(TelegramError::Refused {
            code: Some(400),
            ..
        })
    ));
    assert!(matches!(
        transport
            .call(&context, "sendMessage", json!({}), Uploads::default())
            .await,
        Err(TelegramError::RateLimited { .. })
    ));
    assert!(matches!(
        transport
            .call(&context, "sendMessage", json!({}), Uploads::default())
            .await,
        Err(TelegramError::NoAnswer(_))
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let transport = TelegramTransport::new(format!("http://{address}"), "1:x".to_owned());
    assert!(matches!(
        transport
            .call(&context, "sendMessage", json!({}), Uploads::default())
            .await,
        Err(TelegramError::Unreachable(_))
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|error| panic!("accept: {error}"));
        let mut buffer = [0u8; 4096];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        socket
            .shutdown()
            .await
            .unwrap_or_else(|error| panic!("shutdown: {error}"));
    });
    let transport = TelegramTransport::new(format!("http://{address}"), "1:x".to_owned());
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        transport.call(&context, "sendMessage", json!({}), Uploads::default()),
    )
    .await?;
    assert!(matches!(outcome, Err(TelegramError::NoAnswer(_))));
    Ok(())
}

#[tokio::test]
async fn short_rate_limit_retries_but_server_failure_does_not()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let (base, mut requests) = server(vec![
        (429, json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":1}}).to_string()),
        (200, json!({"ok":true,"result":{"message_id":9}}).to_string()),
        (502, json!({"ok":false,"error_code":502,"description":"gateway"}).to_string()),
    ]).await;
    let transport = TelegramTransport::new(base, "1:x".to_owned());
    let result = transport
        .call(&context, "sendMessage", json!({}), Uploads::default())
        .await?;
    assert_eq!(result["message_id"], 9);
    assert!(matches!(
        transport
            .call(&context, "sendMessage", json!({}), Uploads::default())
            .await,
        Err(TelegramError::NoAnswer(_))
    ));
    let mut request_count = 0;
    while requests.try_recv().is_ok() {
        request_count += 1;
    }
    assert_eq!(request_count, 3);
    Ok(())
}

fn app_settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), TOKEN.to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@public".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

// The same post shape as BLOCKS_POST in the Python reference tests.
fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

async fn api_send(app: Router, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("host", "localhost")
        .header("content-type", "application/json")
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

fn sent_requests(requests: &mut mpsc::UnboundedReceiver<String>) -> usize {
    let mut sent = 0;
    while let Ok(request) = requests.try_recv() {
        if request
            .lines()
            .next()
            .is_some_and(|line| line.contains("/sendRichMessage "))
        {
            sent += 1;
        }
    }
    sent
}

#[tokio::test]
async fn short_429_is_retried_once_and_the_post_goes_out() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mut requests) = server(vec![
        (429, json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 2","parameters":{"retry_after":2}}).to_string()),
        (200, json!({"ok":true,"result":{"message_id":9,"chat":{"id":"@public"},"rich_message":{"blocks":[]}}}).to_string()),
    ])
    .await;
    let app = build_router(AppState::new(app_settings(&dir, &base)));
    let started = Instant::now();
    let (status, response) = tokio::time::timeout(
        Duration::from_secs(20),
        api_send(app, json!({"post":blocks_post()})),
    )
    .await
    .map_err(|_| "short 429 retry took longer than the advertised wait")?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["message"]["message_id"], 9);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "retry_after=2 was not observed: {:?}",
        started.elapsed()
    );
    assert_eq!(sent_requests(&mut requests), 2);
    Ok(())
}

#[tokio::test]
async fn long_429_is_reported_not_waited() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = server(vec![(
        429,
        json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 600","parameters":{"retry_after":600}}).to_string(),
    )])
    .await;
    let app = build_router(AppState::new(app_settings(&dir, &base)));
    let (status, response) = tokio::time::timeout(
        Duration::from_secs(10),
        api_send(app, json!({"post":blocks_post()})),
    )
    .await
    .map_err(|_| "a 600 second retry_after must be reported, not waited out")?;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{response}");
    assert!(
        response["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Too Many Requests"),
        "{response}"
    );
    assert_eq!(sent_requests(&mut requests), 1);
    Ok(())
}

fn secret_settings(dir: &TempDir) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), TOKEN.to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

#[test]
fn token_is_hidden_in_log_lines() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = secret_settings(&dir);
    let line = format!(
        "HTTP Request: {} {} \"{}\"",
        "POST", "https://api.telegram.org/bot123:ABC-def_9/sendMessage", "HTTP/1.1 200 OK"
    );
    let scrubbed = scrub(&settings, &line);
    assert!(!scrubbed.contains("ABC-def_9"), "{scrubbed}");
    assert!(scrubbed.contains("bot<secret>/sendMessage"), "{scrubbed}");
    Ok(())
}

#[test]
fn token_is_hidden_in_structured_log_values() -> Result<(), Box<dyn std::error::Error>> {
    // The Python reference passes an httpx.URL object as a log argument; Rust
    // log events carry values in JSON, so the token must be scrubbed there too.
    let dir = TempDir::new()?;
    let settings = secret_settings(&dir);
    let event = json!({
        "msg": "HTTP Request",
        "args": [
            "POST",
            "https://api.telegram.org/bot123:ABC-def_9/sendMessage",
            "HTTP/1.1",
            200
        ]
    });
    let scrubbed = scrub_all(&settings, &event);
    let text = scrubbed.to_string();
    assert!(!text.contains("ABC-def_9"), "{text}");
    assert!(
        scrubbed["args"][1]
            .as_str()
            .unwrap_or_default()
            .contains("bot<secret>/sendMessage"),
        "{scrubbed}"
    );
    Ok(())
}
