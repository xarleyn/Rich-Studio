//! Sending, previews, scheduling and poll validation.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::services::scheduler;
use rich_studio::store::posts::{self, NewPost};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, fake_server, result_oks, settings};

#[tokio::test]
async fn preview_never_falls_back_to_the_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = settings(&dir, &base);
    config.preview_chat.clear();
    let app = build_router(AppState::new(config));
    let reply = call(
        app,
        "POST",
        "/api/preview-send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(
        reply.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("TG_PREVIEW_CHAT")
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn scheduled_post_stays_sent_when_pin_hits_network_error() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("pinChatMessage", 1);
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let mut content = blocks_post();
    content["pin"] = json!(true);
    let reply = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":content,"at":scheduler::now()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let id = reply.json()["id"]
        .as_str()
        .unwrap_or_else(|| panic!("scheduled id: {}", reply.text()))
        .to_owned();
    let sent = scheduler::run_due(
        &state,
        state.account.clone(),
        scheduler::now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    assert_eq!(sent, 1);
    let record = posts::get(&state.account, &id)?.unwrap_or_else(|| panic!("post {id}"));
    assert_eq!(record["status"], "sent", "{record}");
    assert!(record["message_id"].as_i64().is_some(), "{record}");
    assert!(
        record["error"]
            .as_str()
            .unwrap_or_default()
            .contains("закрепить"),
        "{record}"
    );
    Ok(())
}

#[tokio::test]
async fn network_error_in_one_channel_does_not_stop_the_rest() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("sendRichMessage", 2);
    let app = build_router(AppState::new(settings(&dir, &base)));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b","@c"]})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(result_oks(&reply.json()), vec![true, false, true]);
    assert_eq!(fake.count("sendRichMessage"), 3);
    Ok(())
}

#[tokio::test]
async fn single_channel_network_error_is_a_clear_502() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    fake.drop_call("sendRichMessage", 1);
    let app = build_router(AppState::new(settings(&dir, &base)));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text());
    assert!(
        reply.json()["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("мог уйти")
    );
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn scheduler_reports_missing_chat_without_scaring() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = settings(&dir, &base);
    config.default_chat.clear();
    let state = AppState::new(config);
    let context = state.account.clone();
    let chat = json!("");
    let record = posts::add(
        &context,
        NewPost {
            chat_id: &chat,
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(scheduler::now()),
            error: None,
            kind: "rich",
            actor: "ui",
        },
    )?;
    let id = record["id"].as_str().unwrap_or_default().to_owned();
    assert_eq!(
        scheduler::run_due(
            &state,
            context.clone(),
            scheduler::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let record = posts::get(&context, &id)?.unwrap_or_else(|| panic!("post {id}"));
    assert_eq!(record["status"], "failed", "{record}");
    let error = record["error"].as_str().unwrap_or_default();
    assert!(error.contains("TG_DEFAULT_CHAT"), "{error}");
    assert!(!error.contains("мог уйти"), "{error}");
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn poll_is_checked_like_a_poll_after_a_post() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let poll = json!({"question":"Какой?","options":(0..13).map(|i| format!("вариант {i}")).collect::<Vec<_>>()});
    for body in [
        json!({"poll":poll}),
        json!({"poll":poll,"at":scheduler::now()+3600}),
    ] {
        let reply = call(app.clone(), "POST", "/api/poll", Some(body), &[]).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text()
        );
        assert!(
            reply.text().contains("больше 12 вариантов"),
            "{}",
            reply.text()
        );
    }
    assert!(posts::list(&state.account)?.is_empty());
    assert_eq!(fake.count("sendPoll"), 0);
    Ok(())
}
