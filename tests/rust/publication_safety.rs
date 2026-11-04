use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use rich_studio::store::posts;
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
