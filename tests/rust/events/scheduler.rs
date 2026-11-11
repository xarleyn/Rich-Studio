use rich_studio::{
    AppState,
    services::{bot_notifications, in_flight, scheduler as scheduler_service},
    store::{bot_dialog as db, editing, posts, scheduler as scheduler_store},
};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn scheduler_reports_sent_missed_retry_and_terminal_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.schedule_late_min = 1.0;
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let sent = stored(
        &state.account,
        &json!("@sent"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    let missed = stored(
        &state.account,
        &json!("@missed"),
        "scheduled",
        None,
        Some(now() - 1000),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    assert_eq!(
        posts::get(&state.account, sent["id"].as_str().ok_or("sent id")?)?.ok_or("sent")?["status"],
        "sent"
    );
    assert_eq!(
        posts::get(&state.account, missed["id"].as_str().ok_or("missed id")?)?.ok_or("missed")?["status"],
        "missed"
    );
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 2);
    let retry = stored(
        &state.account,
        &json!("@retry"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("задерживается")
    );
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set retry_at=0 where id=?",
        [retry["id"].as_str().ok_or("retry id")?],
    )?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"forbidden"}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Не удалось")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduled_gateway_failure_reports_uncertain_delivery()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    subscribe_with(&state.settings, 11, "all")?;
    stored(
        &state.account,
        &json!("@channel"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":502,"description":"Bad Gateway"}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    let events = db::due(&state.account, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["body"]["event"], "uncertain");
    assert!(scheduler_store::due(&state.account, now() + 3600)?.is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn crash_recovery_and_expired_comment_are_reported() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let uncertain = stored(&state.account, &json!("@uncertain"), "sending", None, None);
    let uncertain_id = uncertain["id"].as_str().ok_or("uncertain id")?.to_owned();
    let recovered = in_flight::recover_ids(&state.account)?;
    assert_eq!(recovered, std::slice::from_ref(&uncertain_id));
    assert_eq!(
        posts::get(&state.account, &uncertain_id)?.ok_or("post")?["status"],
        "failed"
    );
    publisher_for(&state, &state.account)
        .notify(&uncertain_id, Some("uncertain"), None)
        .await;
    let comment = stored(&state.account, &json!("@comment"), "sent", Some(42), None);
    let comment_id = comment["id"].as_str().ok_or("comment id")?;
    posts::update_delivery(&state.account, comment_id, None, None, Some("waiting"))?;
    rich_studio::store::Store::connect(&state.account)?
        .execute("update posts set sent_at=1 where id=?", [comment_id])?;
    bot_notifications::expire_comments(&state.account, &config, 100, "Пересылка не пришла").await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts.iter().any(|text| text.contains("мог уйти")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("комментарий")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn delayed_retry_notice_is_discarded_after_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    let retry = db::due(&state.account, 10)?.remove(0);
    assert_eq!(retry["body"]["event"], "retry");
    rich_studio::store::Store::connect(&state.account)?
        .execute("update posts set retry_at=0 where id=?", [&id])?;
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("опубликован"), "{texts:?}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn new_schedule_can_report_a_new_missed_deadline() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "scheduled",
        None,
        Some(100),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    for at in [100, 200] {
        if at != 100 {
            assert!(editing::reschedule(&state.account, &id, at)?);
        }
        assert!(scheduler_store::claim(&state.account, &id, at)?);
        assert!(scheduler_store::missed(&state.account, &id, "missed")?);
        let missed = posts::get(&state.account, &id)?.ok_or("missed post")?;
        bot_notifications::record(&state.account, &config, &missed, None).await?;
        bot_notifications::record(&state.account, &config, &missed, None).await?;
        bot_notifications::deliver(&state).await?;
    }
    assert_eq!(mock.count("sendMessage"), 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn rescheduled_post_discards_the_old_missed_report() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "missed",
        None,
        Some(100),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    bot_notifications::record(&state.account, &config, &post, None).await?;
    assert!(editing::reschedule(&state.account, &id, 200)?);
    assert!(scheduler_store::claim(&state.account, &id, 200)?);
    assert!(scheduler_store::missed(
        &state.account,
        &id,
        "new deadline"
    )?);
    let current = posts::get(&state.account, &id)?.ok_or("rescheduled post")?;
    bot_notifications::record(&state.account, &config, &current, None).await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("new deadline"), "{texts:?}");
    server.abort();
    Ok(())
}
