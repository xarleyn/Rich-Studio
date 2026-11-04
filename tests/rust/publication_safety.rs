use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::services::{scheduler, updates};
use rich_studio::settings::Settings;
use rich_studio::store::{bot_dialog, editing, posts, scheduler as scheduler_store};
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

fn post() -> Value {
    json!({"rich_message":{"markdown":"Привет, ё 🚀","future":{"big":9007199254740993u64}},"options":{"future_option":{"keep":true}}})
}

fn context(dir: &TempDir) -> AccountContext {
    AccountContext::root(&settings(dir, ""))
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
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

enum Reply {
    Json(u16, Value),
    Drop,
}

fn message(payload: &Value, id: i64) -> Value {
    json!({"message_id":id,"chat":{"id":payload["chat_id"].clone()},"rich_message":{"blocks":[]}})
}

async fn mock<F>(handler: F) -> (String, mpsc::UnboundedReceiver<(String, Value)>)
where
    F: Fn(usize, &str, &Value) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let (send, receive) = mpsc::unbounded_channel();
    let handler = std::sync::Arc::new(handler);
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
                .and_then(|body| serde_json::from_str(body).ok())
                .unwrap_or_else(|| json!({}));
            let _ = send.send((method.clone(), payload.clone()));
            match handler(count, &method, &payload) {
                Reply::Drop => {
                    let _ = socket.shutdown().await;
                }
                Reply::Json(status, value) => {
                    let body = value.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} ERROR\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    (format!("http://{address}"), receive)
}

fn ok(result: Value) -> Reply {
    Reply::Json(200, json!({"ok":true,"result":result}))
}

fn refused(description: &str) -> Reply {
    Reply::Json(
        400,
        json!({"ok":false,"error_code":400,"description":description}),
    )
}

async fn request(
    app: Router,
    uri: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
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

fn sent_count(requests: &mut mpsc::UnboundedReceiver<(String, Value)>, method: &str) -> usize {
    let mut count = 0;
    while let Ok((name, _)) = requests.try_recv() {
        if name == method {
            count += 1;
        }
    }
    count
}

fn sent_payloads(
    requests: &mut mpsc::UnboundedReceiver<(String, Value)>,
    method: &str,
) -> Vec<Value> {
    let mut payloads = Vec::new();
    while let Ok((name, payload)) = requests.try_recv() {
        if name == method {
            payloads.push(payload);
        }
    }
    payloads
}

fn last_payload(
    requests: &mut mpsc::UnboundedReceiver<(String, Value)>,
    method: &str,
) -> Option<Value> {
    sent_payloads(requests, method).pop()
}

async fn request_as(
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}"))
    };
    (status, value)
}

async fn upload(
    app: Router,
    filename: &str,
    data: &[u8],
    content_type: &str,
) -> (StatusCode, Value) {
    let boundary = "studio-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .header("host", "localhost")
        .method("POST")
        .uri("/api/media")
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
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("upload body: {error}"));
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("upload json {bytes:?}: {error}"));
    (status, value)
}

fn set_scheduled_at(dir: &TempDir, id: &str, at: i64) {
    let connection = rich_studio::store::db::Store::connect(&context(dir))
        .unwrap_or_else(|error| panic!("db: {error}"));
    connection
        .execute(
            "update posts set scheduled_at=? where id=?",
            rusqlite::params![at, id],
        )
        .unwrap_or_else(|error| panic!("schedule update: {error}"));
}

async fn schedule(app: Router, body: Value) -> (StatusCode, Value) {
    request(app, "/api/schedule", body, &[]).await
}

fn callback_update(chat: i64, message_id: i64, data: &str) -> Value {
    json!({"update_id":1,"callback_query":{"id":"cq","data":data,"message":{"message_id":message_id,"chat":{"id":chat}}}})
}

#[tokio::test]
async fn done_key_replays_the_first_response_and_preserves_actor_and_unknown_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let body = json!({"post":post()});
    let headers = [("idempotency-key", "same"), ("x-studio-client", "mcp:test")];
    let (first_status, first) = request(app.clone(), "/api/send", body.clone(), &headers).await;
    let (second_status, second) = request(app, "/api/send", body, &headers).await;
    assert_eq!(first_status, StatusCode::OK);
    assert_eq!((second_status, second), (StatusCode::OK, first.clone()));
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    assert_eq!(first["post"]["actor"], "mcp:test");
    assert_eq!(
        first["post"]["payload"]["options"]["future_option"],
        json!({"keep":true})
    );
    assert_eq!(
        first["post"]["payload"]["rich_message"]["future"]["big"],
        9007199254740993u64
    );
    assert_eq!(posts::list(&context(&dir))?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn response_lost_after_accept_is_uncertain_and_never_resent()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|count, method, payload| match method {
        "sendRichMessage" if count == 1 => Reply::Drop,
        "sendRichMessage" => ok(message(payload, 11)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    for _ in 0..2 {
        let (status, error) = request(
            app.clone(),
            "/api/send",
            json!({"post":post()}),
            &[("idempotency-key", "lost")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(error.to_string().contains("мог уйти"));
    }
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    let rows = posts::list(&context(&dir))?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "failed");
    assert!(
        rows[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("мог уйти")
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_full_after_telegram_accept_keeps_the_key_and_restart_marker()
-> Result<(), Box<dyn std::error::Error>> {
    use rich_studio::{
        account_context::Role,
        errors::{ServiceError, StoreError},
        services::{idempotency::Idempotency, in_flight, kinds, publisher::Publisher},
        store::{
            channels,
            db::{self, Store},
        },
        telegram::client::TelegramTransport,
    };
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "getMe" => ok(json!({"id":55,"is_bot":true})),
        "getChat" => ok(json!({"id":-10011,"type":"channel"})),
        "getChatMember" => ok(json!({"status":"administrator","can_post_messages":true})),
        "sendRichMessage" => ok(message(payload, 20)),
        _ => ok(json!(true)),
    })
    .await;
    let mut settings = settings(&dir, &base);
    settings.account_database_mb = 1.0;
    let context = AccountContext::account(&settings, 12345, Role::Publish);
    let mut connection = Store::connect(&context)?;
    db::migrate(&mut connection)?;
    channels::save(
        &context,
        &json!({
            "chat_id":"-10011","chat_num":-10011,"type":"channel",
            "can_post":1,"rights":{"can_post_messages":true},"topics":[]
        }),
    )?;
    // The pre-send row fits. Only recording Telegram's confirmed acceptance must
    // allocate beyond the real SQLite quota and return SQLITE_FULL.
    connection.execute_batch(
        "create table acceptance_audit(payload blob);
         create trigger full_after_accept before update of status on posts
         when NEW.status='sent' begin
           insert into acceptance_audit values(zeroblob(2097152));
         end;",
    )?;
    drop(connection);
    let publisher = Publisher::new(
        context.clone(),
        TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone()),
        false,
    );
    let idempotency = Idempotency::new(context.clone());
    let body = post();
    let first = idempotency
        .run(Some("full-after-accept"), "send", &body, "test", || async {
            let (results, _) = publisher
                .publish(&[json!(-10011)], &body, None, &kinds::KINDS[0], "test")
                .await?;
            Ok(json!(results))
        })
        .await;
    assert!(
        matches!(first, Err(ServiceError::Store(StoreError::StorageFull))),
        "{first:?}"
    );
    let rows = posts::list(&context)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "sending");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Store::connect(&context)?.execute_batch("drop trigger full_after_accept")?;
    // Reopen the durable key and recover as a restarted process would. Neither
    // operation may cause a second Telegram accept, even once the quota is free.
    let retry = Idempotency::new(context.clone())
        .run(Some("full-after-accept"), "send", &body, "test", || async {
            panic!("an uncertain send must never invoke its action again")
        })
        .await;
    assert!(
        matches!(retry, Err(ServiceError::MayHaveGone(_))),
        "{retry:?}"
    );
    assert_eq!(in_flight::recover(&context)?, 1);
    assert_eq!(posts::list(&context)?[0]["status"], "failed");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn partial_delivery_continues_and_total_failure_is_not_http_success()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage"
            if payload["chat_id"]
                .as_str()
                .is_some_and(|chat| chat.starts_with("@bad")) =>
        {
            refused("chat not found")
        }
        "sendRichMessage" => ok(message(payload, 12)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, result) = request(
        app.clone(),
        "/api/send",
        json!({"post":post(),"chat_ids":["@bad","@good"]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["results"][0]["ok"], false);
    assert_eq!(result["results"][1]["ok"], true);
    let (status, error) = request(
        app,
        "/api/send",
        json!({"post":post(),"chat_ids":["@bad","@bad2"]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("ни в один канал"));
    Ok(())
}

#[tokio::test]
async fn post_is_sent_before_pin_comment_and_poll_after_warnings()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 13)),
        "sendPoll" => refused("poll refused"),
        "pinChatMessage" => refused("pin refused"),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut content = post();
    content["pin"] = json!(true);
    content["comment"] = json!({"markdown":"comment"});
    content["poll_after"] = json!({"question":"Выбор","options":["да","нет"]});
    let (status, result) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["post"]["status"], "sent");
    assert_eq!(result["post"]["comment_status"], "skipped");
    assert!(result["warnings"].to_string().contains("pin refused"));
    assert!(result["warnings"].to_string().contains("poll refused"));
    let rows = posts::list(&context(&dir))?;
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "poll" && row["status"] == "failed")
    );
    Ok(())
}

#[tokio::test]
async fn dry_run_creates_nothing_and_preview_never_uses_the_default_channel()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 14)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, report) = request(
        app.clone(),
        "/api/send",
        json!({"post":post(),"chat_ids":["@a","-100"],"dry_run":true}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["requests"][1]["params"]["chat_id"], -100);
    let (status, _) = request(app, "/api/preview-send", json!({"post":post()}), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(requests.try_recv().is_err());
    assert!(posts::list(&context(&dir))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn poll_media_and_schedule_use_their_registry_and_idempotency()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendPoll" | "sendLivePhoto" | "sendPaidMedia" => ok(message(payload, 15)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let poll = json!({"poll":{"question":"Да?","options":["да","нет"],"future":17}});
    for _ in 0..2 {
        let (status, _) = request(
            app.clone(),
            "/api/poll",
            poll.clone(),
            &[("idempotency-key", "poll")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let live = json!({"kind":"live_photo","params":{"live_photo":"upload:clip.mp4","photo":"upload:still.jpg","caption":"Live"}});
    let (status, _) = request(app.clone(), "/api/media-post", live, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let paid = json!({"kind":"paid_media","params":{"star_count":5,"media":[{"type":"photo","media":"upload:pic.png"}],"future":"keep"}});
    let (status, _) = request(app.clone(), "/api/media-post", paid, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let schedule = json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"});
    let (first_status, first) = request(
        app.clone(),
        "/api/schedule",
        schedule.clone(),
        &[("idempotency-key", "schedule")],
    )
    .await;
    let (second_status, second) = request(
        app,
        "/api/schedule",
        schedule,
        &[("idempotency-key", "schedule")],
    )
    .await;
    assert_eq!(
        (first_status, second_status),
        (StatusCode::OK, StatusCode::OK)
    );
    assert_eq!(first, second);
    assert_eq!(first["status"], "scheduled");
    assert_eq!(first["scheduled_at"], 4070930400i64);
    assert_eq!(sent_count(&mut requests, "sendPoll"), 1);
    assert_eq!(posts::list(&context(&dir))?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn publishing_mcp_tools_have_public_annotations_and_dry_run()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, list) = request(
        app.clone(),
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for name in [
        "publish_post",
        "send_poll",
        "schedule_post",
        "send_media_post",
    ] {
        let tool = list["result"]["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
            .cloned();
        assert_eq!(
            tool.as_ref()
                .and_then(|tool| tool["annotations"]["destructiveHint"].as_bool()),
            Some(true),
            "{name}"
        );
        assert_eq!(
            tool.as_ref()
                .and_then(|tool| tool["annotations"]["openWorldHint"].as_bool()),
            Some(true),
            "{name}"
        );
    }
    let (status, result) = request(app, "/mcp", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish_post","arguments":{"post":post(),"dry_run":true}}}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["structuredContent"]["dry_run"], true);
    assert!(requests.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn poll_buttons_are_sent_with_send_poll() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendPoll" => ok(message(payload, 15)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let at = 4070930400_i64;
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let poll = json!({"poll":{"question":"Как?","options":["Да","Нет"]},"chat_id":"@chan","at":at});
    let (status, created) = request(app.clone(), "/api/poll", poll, &[]).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["scheduled"][0]["id"]
        .as_str()
        .ok_or("scheduled poll id")?
        .to_owned();
    let buttons_path = format!("/api/posts/{id}/buttons");
    let (status, changed) =
        request(app, &buttons_path, json!({"reply_markup":keyboard}), &[]).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["poll"]["reply_markup"], keyboard);
    assert!(changed["payload"].get("reply_markup").is_none());

    let context = context(&dir);
    let connection = rich_studio::store::db::Store::connect(&context)?;
    connection.execute(
        "update posts set scheduled_at=?1 where id=?2",
        rusqlite::params![at - 1, id],
    )?;
    drop(connection);
    assert_eq!(
        rich_studio::services::scheduler::run_due(
            &state,
            state.account.clone(),
            at,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let mut sent = None;
    while let Ok((method, payload)) = requests.try_recv() {
        if method == "sendPoll" {
            sent = Some(payload);
        }
    }
    let sent = sent.ok_or("sendPoll request")?;
    assert_eq!(sent["reply_markup"], keyboard);
    Ok(())
}

#[tokio::test]
async fn http_keys_reject_another_body_and_long_keys_while_unkeyed_requests_all_send()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    for _ in 0..2 {
        let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), &[]).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 2);

    let headers = [("idempotency-key", "post-2")];
    let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    let other = json!({"post":{"rich_message":{"html":"<p>другой</p>"}}});
    let (status, error) = request(app.clone(), "/api/send", other, &headers).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(error.to_string().contains("Idempotency-Key"), "{error}");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);

    let long = "k".repeat(201);
    let (status, _) = request(
        app,
        "/api/send",
        json!({"post":post()}),
        &[("idempotency-key", long.as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
    assert_eq!(posts::list(&context(&dir))?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn refused_send_releases_the_key_but_both_attempts_reach_telegram()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler = Arc::clone(&attempts);
    let (base, mut requests) = mock(move |_, method, payload| match method {
        "sendRichMessage" if handler.fetch_add(1, Ordering::SeqCst) == 0 => {
            refused("Bad Request: chat not found")
        }
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let headers = [("idempotency-key", "post-3")];
    let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = request(app, "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 2);
    Ok(())
}

#[tokio::test]
async fn pending_key_left_by_a_crash_conflicts_then_becomes_may_have_gone()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let headers = [("idempotency-key", "post-6")];
    let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    let connection = rich_studio::store::db::Store::connect(&context(&dir))?;
    connection.execute(
        "update idempotency set status='pending', response=null where key='post-6'",
        [],
    )?;
    let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::CONFLICT);
    connection.execute(
        "update idempotency set created_at=created_at-3600 where key='post-6'",
        [],
    )?;
    let (status, error) = request(app, "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("мог уйти"), "{error}");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn actor_is_sanitized_and_survives_scheduling() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    for headers in [
        &[("x-studio-client", "mcp:claude-code")][..],
        &[][..],
        &[("x-studio-client", "<script>")][..],
    ] {
        let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), headers).await;
        assert_eq!(status, StatusCode::OK);
    }
    let mut actors = posts::list(&context(&dir))?
        .into_iter()
        .map(|row| row["actor"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    actors.sort();
    assert_eq!(actors, ["api", "api", "mcp:claude-code"]);

    let (status, record) = request(
        app,
        "/api/schedule",
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"}),
        &[("x-studio-client", "ui")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("scheduled id")?.to_owned();
    set_scheduled_at(&dir, &id, scheduler::now() - 5);
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            scheduler::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = posts::get(&context(&dir), &id)?.ok_or("scheduled post")?;
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["actor"], "ui");
    Ok(())
}

#[tokio::test]
async fn dry_run_reports_requests_after_send_and_validates_uploads_and_schedule()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 14)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let button = json!({"inline_keyboard":[[{"text":"a","url":"https://t.me"}]]});
    let mut content = post();
    content["pin"] = json!(true);
    content["reply_markup"] = button;
    let (status, report) = request(
        app.clone(),
        "/api/send",
        json!({"post":content,"chat_ids":["@a","-100"],"dry_run":true}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["after_send"], json!(["pinChatMessage"]));
    assert_eq!(report["requests"][0]["params"]["chat_id"], json!("@a"));
    assert_eq!(report["requests"][1]["params"]["chat_id"], json!(-100));
    assert_eq!(report["requests"][0]["method"], "sendRichMessage");
    assert_eq!(
        report["requests"][0]["params"]["rich_message"],
        content["rich_message"]
    );

    let bad = json!({"post":{"rich_message":{"blocks":[{"type":"heading","text":"x","size":"big"}]}},"dry_run":true});
    assert_eq!(
        request(app.clone(), "/api/send", bad, &[]).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let missing = json!({"post":{"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":"upload:nope.jpg"}}]}},"dry_run":true});
    let (status, error) = request(app.clone(), "/api/send", missing, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("nope.jpg"), "{error}");

    let (status, report) = schedule(
        app,
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00","dry_run":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["at"], 4070930400i64);
    assert!(requests.try_recv().is_err());
    assert!(posts::list(&context(&dir))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn editing_a_sent_post_keeps_callbacks_and_comment_and_answers_clicks()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" | "editMessageText" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    content["comment"] = json!({"html":"<p>Ссылки</p>"});
    let (status, sent) = request(
        app.clone(),
        "/api/send",
        json!({"post":content,"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let new = json!({"rich_message":{"markdown":"# Исправлено"},"reply_markup":button});
    let (status, changed) = request(app, &format!("/api/posts/{id}/edit"), new.clone(), &[]).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["rich_message"], new["rich_message"]);
    assert_eq!(changed["payload"]["callbacks"], json!({"like":"Спасибо"}));
    assert_eq!(
        changed["payload"]["comment"],
        json!({"html":"<p>Ссылки</p>"})
    );

    let click = callback_update(
        -1001,
        changed["message_id"].as_i64().ok_or("message id")?,
        "like",
    );
    assert_eq!(
        updates::handle_update(&state, &click).await?,
        Some("callback")
    );
    let answer = last_payload(&mut requests, "answerCallbackQuery").ok_or("answerCallbackQuery")?;
    assert_eq!(answer["text"], "Спасибо");
    Ok(())
}

#[tokio::test]
async fn repeated_edits_are_not_modified_and_only_real_changes_add_a_version()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        "editMessageText" | "editMessageReplyMarkup" => Reply::Json(
            400,
            json!({"ok":false,"error_code":400,"description":"Bad Request: message is not modified: specified new message content and reply markup are exactly the same"}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    let (status, sent) = request(app.clone(), "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, _) = request(
        app.clone(),
        &format!("/api/posts/{id}/edit"),
        content.clone(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request_as(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}/versions"),
            None,
            &[]
        )
        .await
        .1,
        json!([])
    );
    let only_answers = json!({"rich_message":content["rich_message"],"reply_markup":button,"callbacks":{"like":"Голос учтён"}});
    let (status, changed) = request(
        app.clone(),
        &format!("/api/posts/{id}/edit"),
        only_answers,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        changed["payload"]["callbacks"],
        json!({"like":"Голос учтён"})
    );
    assert_eq!(
        request_as(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}/versions"),
            None,
            &[]
        )
        .await
        .1
        .as_array()
        .map(Vec::len),
        Some(1)
    );

    let mut bare = post();
    bare["reply_markup"] = button.clone();
    let (status, sent) = request(app.clone(), "/api/send", json!({"post":bare}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let bare_id = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, buttons) = request(
        app,
        &format!("/api/posts/{bare_id}/buttons"),
        json!({"reply_markup":button,"callbacks":{"like":"Спасибо"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{buttons}");
    assert_eq!(buttons["payload"]["callbacks"], json!({"like":"Спасибо"}));
    Ok(())
}

#[tokio::test]
async fn edit_is_refused_once_the_scheduler_has_claimed_the_post()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, record) = schedule(
        app.clone(),
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let context = context(&dir);
    assert!(scheduler_store::claim(&context, &id, 4070930401)?);
    let (status, error) = request(
        app,
        &format!("/api/posts/{id}/edit"),
        json!({"rich_message":{"markdown":"новое"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("sending"), "{error}");
    let old = record["payload"].clone();
    assert!(!editing::replace_scheduled(
        &context,
        &id,
        &json!({"rich_message":{"markdown":"новое"}}),
        &old,
        "api",
        record["version"].as_i64().unwrap_or_default(),
    )?);
    assert_eq!(
        posts::get(&context, &id)?.ok_or("post")?["payload"]["rich_message"],
        old["rich_message"]
    );
    assert!(editing::versions(&context, &id)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn far_past_post_is_missed_unless_the_late_limit_is_off()
-> Result<(), Box<dyn std::error::Error>> {
    for (late_limit, expected) in [(60.0, "missed"), (0.0, "sent")] {
        let dir = TempDir::new()?;
        let (base, mut requests) = mock(|_, method, payload| match method {
            "sendRichMessage" => ok(message(payload, 10)),
            _ => ok(json!(true)),
        })
        .await;
        let mut config = settings(&dir, &base);
        config.schedule_late_min = late_limit;
        let state = AppState::new(config);
        let app = build_router(state.clone());
        let (status, record) =
            schedule(app, json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"})).await;
        assert_eq!(status, StatusCode::OK);
        let id = record["id"].as_str().ok_or("post id")?;
        let now = scheduler::now();
        set_scheduled_at(&dir, id, now - 3 * 3600);
        assert_eq!(
            scheduler::run_due(
                &state,
                state.account.clone(),
                now,
                Arc::new(AtomicBool::new(false)),
            )
            .await?,
            1
        );
        let row = posts::get(&context(&dir), id)?.ok_or("post")?;
        assert_eq!(row["status"], expected);
        if expected == "missed" {
            assert!(
                row["error"].as_str().unwrap_or_default().contains("3 ч"),
                "{row}"
            );
            assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
        } else {
            assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn due_posts_are_sent_in_queue_order_with_increasing_message_ids()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|count, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 100 + count as i64)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let now = scheduler::now();
    let mut ids = Vec::new();
    for (markdown, at) in [("второй", now - 60), ("первый", now - 120)] {
        let (status, record) = schedule(
            app.clone(),
            json!({"post":{"rich_message":{"markdown":markdown}},"at":"2099-01-01T09:00:00+03:00"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = record["id"].as_str().ok_or("post id")?.to_owned();
        set_scheduled_at(&dir, &id, at);
        ids.push(id);
    }
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        2
    );
    let sent = sent_payloads(&mut requests, "sendRichMessage");
    assert_eq!(
        sent.iter()
            .map(|payload| payload["rich_message"]["markdown"].clone())
            .collect::<Vec<_>>(),
        [json!("первый"), json!("второй")]
    );
    let first = posts::get(&context(&dir), &ids[1])?.ok_or("first post")?;
    let second = posts::get(&context(&dir), &ids[0])?.ok_or("second post")?;
    assert!(
        first["message_id"].as_i64().unwrap_or_default()
            < second["message_id"].as_i64().unwrap_or_default()
    );
    Ok(())
}

#[tokio::test]
async fn scheduler_sends_the_content_as_it_is_now() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let at = 4070930400_i64;
    let (status, record) = schedule(app.clone(), json!({"post":post(),"at":at})).await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let stale = scheduler_store::due(&context(&dir), at)?;
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["payload"]["rich_message"], post()["rich_message"]);
    let (status, changed) = request(
        app,
        &format!("/api/posts/{id}/edit"),
        json!({"rich_message":{"markdown":"свежий текст"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            at,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = sent_payloads(&mut requests, "sendRichMessage");
    assert_eq!(sent[0]["rich_message"], json!({"markdown":"свежий текст"}));
    Ok(())
}

#[tokio::test]
async fn unreachable_telegram_is_retried_not_failed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let (status, record) =
        schedule(app, json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"})).await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let now = scheduler::now();
    set_scheduled_at(&dir, &id, now - 5);
    let down = AppState::new(settings(&dir, "http://127.0.0.1:9"));
    assert_eq!(
        scheduler::run_due(
            &down,
            down.account.clone(),
            now,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let row = posts::get(&context(&dir), &id)?.ok_or("post")?;
    assert_eq!(row["status"], "scheduled");
    assert!(row["retry_at"].as_i64().unwrap_or_default() > now);
    assert!(
        row["error"]
            .as_str()
            .unwrap_or_default()
            .contains("нет связи"),
        "{row}"
    );
    assert_eq!(
        scheduler::run_due(
            &down,
            down.account.clone(),
            now + 1,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        0
    );
    assert_eq!(
        posts::get(&context(&dir), &id)?.ok_or("post")?["status"],
        "scheduled"
    );
    let connection = rich_studio::store::db::Store::connect(&context(&dir))?;
    connection.execute(
        "update posts set retry_at=? where id=?",
        rusqlite::params![now - 1, id],
    )?;
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now + 2,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let row = posts::get(&context(&dir), &id)?.ok_or("post")?;
    assert_eq!(row["status"], "sent");
    assert!(row["error"].is_null());
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn long_flood_wait_is_retried_later() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, _| match method {
        "sendRichMessage" => Reply::Json(
            429,
            json!({"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":120}}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let (status, record) =
        schedule(app, json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"})).await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let now = scheduler::now();
    set_scheduled_at(&dir, &id, now - 5);
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let row = posts::get(&context(&dir), &id)?.ok_or("post")?;
    assert_eq!(row["status"], "scheduled");
    assert!(row["retry_at"].as_i64().unwrap_or_default() >= now + 100);
    Ok(())
}

#[tokio::test]
async fn gateway_error_means_may_have_gone_and_the_key_is_kept()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, _| match method {
        "sendRichMessage" => Reply::Json(
            502,
            json!({"ok":false,"error_code":502,"description":"Bad Gateway"}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let headers = [("idempotency-key", "k-502")];
    for _ in 0..2 {
        let (status, error) =
            request(app.clone(), "/api/send", json!({"post":post()}), &headers).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(error.to_string().contains("мог уйти"), "{error}");
    }
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn error_code_500_in_json_is_also_uncertain() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, _| match method {
        "sendRichMessage" => Reply::Json(
            200,
            json!({"ok":false,"error_code":500,"description":"Internal Server Error"}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, error) = request(app, "/api/send", json!({"post":post()}), &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("мог уйти"), "{error}");
    Ok(())
}

#[tokio::test]
async fn gateway_error_on_check_is_a_clear_502_with_a_check_hint()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, _| match method {
        "getMe" => Reply::Json(
            502,
            json!({"ok":false,"error_code":502,"description":"bad gateway"}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, error) = request_as(app, "GET", "/api/check", None, &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("проверьте"), "{error}");
    Ok(())
}

#[tokio::test]
async fn unreachable_telegram_on_send_frees_the_key() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let headers = [("idempotency-key", "k-down")];
    let down = build_router(AppState::new(settings(&dir, "http://127.0.0.1:9")));
    let (status, error) = request(down, "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("не отправлен"), "{error}");
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, _) = request(app, "/api/send", json!({"post":post()}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn first_comment_with_updates_off_is_reported_not_left_waiting()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, result) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["post"]["comment_status"], "skipped");
    assert!(
        result["warnings"].to_string().contains("TG_UPDATES"),
        "{result}"
    );
    Ok(())
}

#[tokio::test]
async fn network_error_on_first_comment_is_recorded() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler = Arc::clone(&attempts);
    let (base, _) = mock(move |_, method, payload| match method {
        "sendRichMessage" if handler.fetch_add(1, Ordering::SeqCst) == 0 => {
            ok(message(payload, 10))
        }
        "sendRichMessage" => Reply::Drop,
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, sent) = request(
        app,
        "/api/send",
        json!({"post":content,"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let message_id = sent["message"]["message_id"].as_i64().ok_or("message id")?;
    let event = json!({"update_id":9,"message":{"message_id":77,"chat":{"id":-2002},"is_automatic_forward":true,"forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":message_id}}});
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("comment")
    );
    let post = posts::get(&context(&dir), id)?.ok_or("post")?;
    assert_eq!(post["comment_status"], "failed");
    assert!(
        post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("мог уйти"),
        "{post}"
    );
    Ok(())
}

#[tokio::test]
async fn comment_waiting_for_more_than_a_day_expires() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let app = build_router(AppState::new(config));
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, sent) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let context = context(&dir);
    let now = scheduler::now();
    assert!(
        bot_dialog::expire_comments(&context, now - updates::COMMENT_WAIT, "старое")?.is_empty()
    );
    rich_studio::store::db::Store::connect(&context)?.execute(
        "update posts set sent_at=? where id=?",
        rusqlite::params![now - updates::COMMENT_WAIT - 10, id],
    )?;
    assert_eq!(
        bot_dialog::expire_comments(&context, now - updates::COMMENT_WAIT, "не пришла")?,
        std::slice::from_ref(&id)
    );
    assert_eq!(
        posts::get(&context, &id)?.ok_or("post")?["comment_status"],
        "failed"
    );
    Ok(())
}

#[tokio::test]
async fn media_used_by_waiting_post_or_draft_is_kept_and_only_exact_references_count()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, _) = upload(app.clone(), "cat.png", b"\x89PNG", "image/png").await;
    assert_eq!(status, StatusCode::OK);
    let photo = json!({"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":"upload:cat.png"}}]}});
    let (status, _) = schedule(
        app.clone(),
        json!({"post":photo,"at":"2099-01-01T09:00:00+03:00","chat_ids":["@test_channel"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Кот","post":photo})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, refused) =
        request_as(app.clone(), "DELETE", "/api/media/cat.png", None, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        refused["detail"]["used_by"],
        json!(["отложенный пост в @test_channel", "черновик «Кот»"])
    );
    let (_, files) = request_as(app.clone(), "GET", "/api/media", None, &[]).await;
    assert!(
        files
            .as_array()
            .into_iter()
            .flatten()
            .any(|file| file["name"] == "cat.png"),
        "{files}"
    );
    let (status, _) = request_as(
        app.clone(),
        "DELETE",
        "/api/media/cat.png?force=true",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, files) = request_as(app.clone(), "GET", "/api/media", None, &[]).await;
    assert_eq!(files, json!([]));

    upload(app.clone(), "dog.png", b"\x89PNG", "image/png").await;
    let (status, _) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Другое","post":{"rich_message":{"markdown":"upload:dog.png.bak"}}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request_as(app, "DELETE", "/api/media/dog.png", None, &[])
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn stale_and_deleted_draft_saves_are_refused_with_current_state()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, draft) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":{"v":1}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let version = draft["version"].as_i64().ok_or("version")?;
    let (status, tab1) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":2},"version":version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tab1["version"], version + 1);
    let (status, stale) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":3},"version":version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["detail"]["current"]["payload"], json!({"v":2}));
    let (status, mine) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":3}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mine["payload"], json!({"v":3}));
    assert_eq!(
        mine["version"],
        tab1["version"].as_i64().unwrap_or_default() + 1
    );

    let (status, gone) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":{"v":1}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let gone_id = gone["id"].as_str().ok_or("draft id")?.to_owned();
    let gone_version = gone["version"].as_i64().unwrap_or_default();
    assert_eq!(
        request_as(
            app.clone(),
            "DELETE",
            &format!("/api/drafts/{gone_id}"),
            None,
            &[],
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, conflict) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{gone_id}"),
        Some(json!({"title":"A","post":{"v":2},"version":gone_version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(conflict["detail"]["current"].is_null());
    assert_eq!(
        request_as(app, "GET", &format!("/api/drafts/{gone_id}"), None, &[])
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn preview_buttons_answer_like_in_the_channel_and_old_clicks_still_count()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 20)),
        "answerCallbackQuery" => refused(
            "Bad Request: query is too old and response timeout expired or query ID is invalid",
        ),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.preview_chat = "777".to_owned();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    let (status, preview) = request(
        app.clone(),
        "/api/preview-send",
        json!({"post":content}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let message_id = preview["message_id"].as_i64().ok_or("message id")?;
    assert_eq!(
        updates::handle_update(&state, &callback_update(777, message_id, "like")).await?,
        Some("callback")
    );
    let answer = last_payload(&mut requests, "answerCallbackQuery").ok_or("answerCallbackQuery")?;
    assert_eq!(answer["text"], "Спасибо");

    let (status, sent) = request(
        app,
        "/api/send",
        json!({"post":{"rich_message":{"markdown":"клик"},"reply_markup":button},"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let mid = posts::get(&context(&dir), id)?.ok_or("post")?["message_id"]
        .as_i64()
        .ok_or("message id")?;
    assert_eq!(
        updates::handle_update(&state, &callback_update(-1001, mid, "like")).await?,
        Some("callback")
    );
    let post = posts::get(&context(&dir), id)?.ok_or("post")?;
    assert_eq!(post["clicks"], json!({"like":1}));
    Ok(())
}
