use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use rich_studio::account_context::AccountContext;
use rich_studio::store::posts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

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
