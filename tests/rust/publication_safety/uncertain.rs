use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::services::scheduler;
use rich_studio::store::posts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

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
