use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::{AppState, build_router, settings::Settings, store::posts};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Clone, Debug)]
struct Call {
    method: String,
    content_type: String,
    body: String,
}

enum Reply {
    Json(u16, Value),
    Drop,
}

fn ok(result: Value) -> Reply {
    Reply::Json(200, json!({"ok":true,"result":result}))
}

fn message(call: &Call, id: i64) -> Value {
    let payload: Value = serde_json::from_str(&call.body).unwrap_or_else(|_| json!({}));
    json!({"message_id":id,"chat":{"id":payload["chat_id"].clone()},"rich_message":{"blocks":[]}})
}

async fn mock<F>(handler: F) -> (String, mpsc::UnboundedReceiver<Call>)
where
    F: Fn(usize, &Call) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let (sender, receiver) = mpsc::unbounded_channel();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        let mut count = 0usize;
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            count += 1;
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            let request_line = request.lines().next().unwrap_or_default();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let method = path.rsplit('/').next().unwrap_or_default().to_owned();
            let content_type = request
                .lines()
                .take_while(|line| !line.is_empty())
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-type:")
                        .map(|value| value.trim().to_owned())
                })
                .unwrap_or_default();
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default()
                .to_owned();
            let call = Call {
                method,
                content_type,
                body,
            };
            let _ = sender.send(call.clone());
            match handler(count, &call) {
                Reply::Drop => {
                    let _ = socket.shutdown().await;
                }
                Reply::Json(status, value) => {
                    let body = value.to_string();
                    let reason = if status < 300 { "OK" } else { "ERROR" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    (format!("http://{address}"), receiver)
}

fn settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@public".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SCHEDULE_LATE_MIN".to_owned(), "0".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

fn add_media(state: &AppState, name: &str, data: &[u8]) -> TestResult {
    std::fs::create_dir_all(&state.account.media_dir)?;
    std::fs::write(state.account.media_dir.join(name), data)?;
    Ok(())
}

async fn request(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
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
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}")),
    )
}

fn field(body: &str, name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\"");
    let start = body.find(&marker)?;
    let rest = &body[start + marker.len()..];
    let value_start = rest.find("\r\n\r\n")? + 4;
    let rest = &rest[value_start..];
    let end = rest.find("\r\n--").unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

fn drain(receiver: &mut mpsc::UnboundedReceiver<Call>) -> Vec<Call> {
    let mut calls = Vec::new();
    while let Ok(call) = receiver.try_recv() {
        calls.push(call);
    }
    calls
}

#[tokio::test]
async fn live_photo_goes_with_both_files() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mut receiver) = mock(|_, call| match call.method.as_str() {
        "sendLivePhoto" => ok(message(call, 21)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    add_media(&state, "clip.mp4", b"clip")?;
    add_media(&state, "still.jpg", b"still")?;
    let app = build_router(state);
    let body = json!({
        "kind":"live_photo",
        "params":{"live_photo":"upload:clip.mp4","photo":"upload:still.jpg","caption":"Закат"},
        "pin":true
    });
    let (status, result) = request(app.clone(), "POST", "/api/media-post", Some(body), &[]).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let record = &result["results"][0]["post"];
    assert_eq!(record["kind"], "live_photo");
    assert_eq!(record["status"], "sent");
    assert_eq!(record["message_id"], 21);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();

    let (status, listed) = request(app.clone(), "GET", "/api/posts", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed
        .as_array()
        .ok_or("post list")?
        .iter()
        .find(|post| post["id"] == id.as_str())
        .ok_or("listed post")?;
    assert_eq!(listed["title"], "Закат");
    assert_eq!(listed["thumb"], "/media/still.jpg");

    let (status, error) = request(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(json!({"rich_message":{"markdown":"x"}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(error.to_string().contains("Live Photo"));

    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, changed) = request(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["live_photo"]["reply_markup"], keyboard);
    assert!(changed["payload"].get("reply_markup").is_none());

    let calls = drain(&mut receiver);
    let sent = calls
        .iter()
        .find(|call| call.method == "sendLivePhoto")
        .ok_or("sendLivePhoto request")?;
    assert!(sent.content_type.starts_with("multipart/form-data"));
    assert_eq!(
        field(&sent.body, "live_photo").as_deref(),
        Some("attach://f0")
    );
    assert_eq!(field(&sent.body, "photo").as_deref(), Some("attach://f1"));
    assert_eq!(field(&sent.body, "f0").as_deref(), Some("clip"));
    assert_eq!(field(&sent.body, "f1").as_deref(), Some("still"));
    assert_eq!(field(&sent.body, "caption").as_deref(), Some("Закат"));
    let pin = calls
        .iter()
        .find(|call| call.method == "pinChatMessage")
        .ok_or("pinChatMessage request")?;
    let pin: Value = serde_json::from_str(&pin.body)?;
    assert_eq!(pin["message_id"], 21);
    let buttons = calls
        .iter()
        .find(|call| call.method == "editMessageReplyMarkup")
        .ok_or("editMessageReplyMarkup request")?;
    assert!(buttons.body.contains("callback_data") || buttons.body.contains("inline_keyboard"));
    Ok(())
}

#[tokio::test]
async fn paid_media_is_checked_and_can_wait() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mut receiver) = mock(|_, call| match call.method.as_str() {
        "sendPaidMedia" => ok(message(call, 22)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    add_media(&state, "pic.jpg", b"pic")?;
    let app = build_router(state);
    let at = 4070930400_i64;
    let bad = json!({"kind":"paid_media","params":{"star_count":5,"media":[]}});
    let (status, error) = request(app.clone(), "POST", "/api/media-post", Some(bad), &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(error.to_string().contains("от 1 до 10"));

    let missing =
        json!({"kind":"paid_media","params":{"media":[{"type":"photo","media":"upload:pic.jpg"}]}});
    let (status, error) = request(app.clone(), "POST", "/api/media-post", Some(missing), &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(error.to_string().contains("star_count"));

    for stars in [0, 25001] {
        let price = json!({"kind":"paid_media","params":{"star_count":stars,"media":[{"type":"photo","media":"upload:pic.jpg"}]},"at":at});
        let (status, error) =
            request(app.clone(), "POST", "/api/media-post", Some(price), &[]).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert!(error.to_string().contains("25000"));
    }
    let (status, error) = request(
        app.clone(),
        "POST",
        "/api/media-post",
        Some(json!({"kind":"voice","params":{}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");

    let params = json!({"star_count":5,"media":[{"type":"photo","media":"upload:pic.jpg"}],"caption":"Закрытый кадр"});
    let scheduled = json!({"kind":"paid_media","params":params,"at":at});
    let (status, result) =
        request(app.clone(), "POST", "/api/media-post", Some(scheduled), &[]).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["scheduled"][0]["kind"], "paid_media");
    assert!(drain(&mut receiver).is_empty());

    let (status, error) = request(app.clone(), "DELETE", "/api/media/pic.jpg", None, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");

    let (status, result) = request(
        app,
        "POST",
        "/api/media-post",
        Some(json!({"kind":"paid_media","params":{"star_count":5,"media":[{"type":"photo","media":"upload:pic.jpg"}],"caption":"Закрытый кадр"}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["results"][0]["ok"], true);
    let calls = drain(&mut receiver);
    let sent = calls
        .iter()
        .find(|call| call.method == "sendPaidMedia")
        .ok_or("sendPaidMedia request")?;
    assert_eq!(field(&sent.body, "star_count").as_deref(), Some("5"));
    assert!(
        field(&sent.body, "media")
            .as_deref()
            .unwrap_or_default()
            .contains("attach://f0")
    );
    Ok(())
}

#[tokio::test]
async fn media_post_needs_publish() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mut receiver) = mock(|_, _| ok(json!(true))).await;
    let cfg = settings(&dir, &base);
    rich_studio::store::auth::add_token(&cfg, "draft", "draft-secret", "draft")?;
    let app = build_router(AppState::new(cfg));
    let (status, error) = request(
        app,
        "POST",
        "/api/media-post",
        Some(json!({"kind":"live_photo","params":{}})),
        &[("authorization", "Bearer draft-secret")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert!(receiver.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn media_post_through_mcp_is_idempotent() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mut receiver) = mock(|_, call| match call.method.as_str() {
        "sendLivePhoto" => ok(message(call, 23)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    add_media(&state, "clip.mp4", b"clip")?;
    add_media(&state, "still.jpg", b"still")?;
    let app = build_router(state);
    let arguments = json!({
        "kind":"live_photo",
        "params":{"live_photo":"upload:clip.mp4","photo":"upload:still.jpg"}
    });
    for id in [1, 2] {
        let (status, result) = request(
            app.clone(),
            "POST",
            "/mcp",
            Some(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"send_media_post","arguments":arguments}})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["result"]["isError"], false, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["results"][0]["ok"], true,
            "{result}"
        );
    }
    let calls = drain(&mut receiver);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.method == "sendLivePhoto")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_message_refused_by_every_chat_is_an_error_not_sent() -> TestResult {
    let dir = TempDir::new()?;
    let fail = Arc::new(AtomicBool::new(true));
    let fail_handler = fail.clone();
    let (base, mut receiver) = mock(move |_, call| {
        if fail_handler.load(Ordering::SeqCst)
            && (call.method == "sendPoll" || call.method == "sendLivePhoto")
        {
            return Reply::Json(
                400,
                json!({"ok":false,"error_code":400,"description":"Bad Request: chat not found"}),
            );
        }
        match call.method.as_str() {
            "sendPoll" | "sendLivePhoto" => ok(message(call, 31)),
            _ => ok(json!(true)),
        }
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    add_media(&state, "still.jpg", b"still")?;
    let app = build_router(state.clone());
    let poll =
        json!({"poll":{"question":"Когда?","options":["Утром","Вечером"]},"chat_ids":["@a","@b"]});
    let (status, error) = request(app.clone(), "POST", "/api/poll", Some(poll.clone()), &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
    assert!(error.to_string().contains("ни в один канал"));
    assert!(error.to_string().contains("@a: Bad Request"));

    let media = json!({"kind":"live_photo","params":{"live_photo":"upload:still.jpg","photo":"upload:still.jpg"},"chat_id":"@a"});
    let (status, error) = request(app.clone(), "POST", "/api/media-post", Some(media), &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(error.to_string().contains("chat not found"));

    let listed = posts::list(&state.account)?;
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|post| post["status"] == "failed"));

    fail.store(false, Ordering::SeqCst);
    let (status, result) = request(app, "POST", "/api/poll", Some(poll), &[]).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["results"][0]["ok"], true);
    assert_eq!(result["results"][1]["ok"], true);
    drain(&mut receiver);
    Ok(())
}

#[tokio::test]
async fn no_answer_from_telegram_is_not_called_not_sent() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _receiver) = mock(|_, call| {
        if call.method == "sendPoll" {
            Reply::Drop
        } else {
            ok(json!(true))
        }
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state);
    let poll =
        json!({"poll":{"question":"Когда?","options":["Утром","Вечером"]},"chat_ids":["@a","@b"]});
    let (status, error) = request(app, "POST", "/api/poll", Some(poll), &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
    let message = error["detail"]["message"].as_str().ok_or("message")?;
    assert!(message.contains("могло уйти"), "{message}");
    assert!(!message.contains("не отправлено"), "{message}");
    Ok(())
}
