use rich_studio::{
    AppState,
    errors::ServiceError,
    services::{bot_notifications, kinds},
    store::{bot_dialog as db, posts},
    telegram::{TelegramError, client::TelegramTransport},
};
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn publication_report_is_deduplicated_and_linked() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@test_channel")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let post = results[0]["post"].clone();
    assert_eq!(post["status"], "sent");
    assert_eq!(mock.count("sendMessage"), 0);
    bot_notifications::record(&state.account, &config, &post, None).await?;
    bot_notifications::deliver(&state).await?;
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    let notice = last_body(&mock, "sendMessage");
    assert!(
        notice["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Пост опубликован")
    );
    let link = notice["reply_markup"]["inline_keyboard"][0][0]["url"]
        .as_str()
        .unwrap_or_default();
    assert!(link.starts_with("https://t.me/test_channel/"), "{link}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn notification_preferences_filter_and_discard_historical_reports()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let publisher = publisher_for(&state, &state.account);
    let kind = kinds::get(Some(kinds::RICH))?;
    publisher
        .publish(&[json!("@old")], &blocks_post(), None, kind, "test")
        .await?;
    subscribe_with(&config, 11, "errors")?;
    publisher
        .publish(&[json!("@ok")], &blocks_post(), None, kind, "test")
        .await?;
    assert!(db::due(&state.account, 10)?.is_empty());
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"Bad Request: no posting rights"}),
    );
    let refused = publisher
        .publish(&[json!("@refused")], &blocks_post(), None, kind, "test")
        .await;
    assert!(matches!(
        refused,
        Err(ServiceError::Telegram(TelegramError::Refused { .. }))
    ));
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("no posting rights")
    );
    subscribe_with(&config, 11, "all")?;
    publisher
        .publish(&[json!("@queued")], &blocks_post(), None, kind, "test")
        .await?;
    db::disable(&config, 11)?;
    bot_notifications::deliver(&state).await?;
    subscribe_with(&config, 11, "all")?;
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn uncertain_publication_reports_may_have_gone_without_retrying()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":502,"description":"gateway down"}),
    );
    let result = publisher_for(&state, &state.account)
        .publish(
            &[json!("@maybe")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await;
    assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));
    bot_notifications::deliver(&state).await?;
    let text = last_body(&mock, "sendMessage")["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(text.contains("Пост мог уйти"), "{text}");
    assert!(text.contains("Проверьте канал перед повтором"), "{text}");
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn failed_private_report_never_changes_or_duplicates_publication()
-> Result<(), Box<dyn std::error::Error>> {
    for (mode, expected) in [(2, "off"), (1, "all")] {
        let dir = TempDir::new()?;
        let (base, mock, server) = mock().await?;
        let config = settings(&dir, &base);
        let state = AppState::new(config.clone());
        subscribe_with(&config, 11, "all")?;
        let (results, _) = publisher_for(&state, &state.account)
            .publish(
                &[json!("@ok")],
                &blocks_post(),
                None,
                kinds::get(Some(kinds::RICH))?,
                "test",
            )
            .await?;
        let id = results[0]["post"]["id"]
            .as_str()
            .ok_or("post id")?
            .to_owned();
        mock.mode.store(mode, Ordering::SeqCst);
        bot_notifications::deliver(&state).await?;
        bot_notifications::deliver(&state).await?;
        assert_eq!(
            posts::get(&state.account, &id)?.ok_or("post")?["status"],
            "sent"
        );
        assert_eq!(mock.count("sendMessage"), 1);
        assert_eq!(
            db::contact(&config, 11)?.ok_or("contact")?["mode"],
            expected
        );
        server.abort();
    }
    Ok(())
}

#[tokio::test]
async fn definite_private_report_failure_is_retried_only_after_release()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let event = db::due(&state.account, 10)?.remove(0);
    let key = event["event_key"].as_str().ok_or("key")?.to_owned();
    let offline = TelegramTransport::new("http://127.0.0.1:0".to_owned(), config.bot_token.clone());
    bot_notifications::deliver_one(&state.account, &config, &offline, &event).await?;
    let (state_name, retry_at): (String, i64) = rich_studio::store::Store::connect(&state.account)?
        .query_row(
            "select state,retry_at from bot_outbox where event_key=?",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    assert_eq!(state_name, "pending");
    assert!(retry_at > now());
    db::finish(&state.account, &key, "pending", 0)?;
    let due = db::due(&state.account, 10)?.remove(0);
    bot_notifications::deliver_one(&state.account, &config, &state.transport(), &due).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn post_send_warning_is_reported_without_leaking_the_token()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let mut post = blocks_post();
    post["pin"] = json!(true);
    mock.respond(
        "pinChatMessage",
        json!({"ok":false,"error_code":400,"description":format!("<bad> {}", config.bot_token)}),
    );
    let (results, warnings) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &post,
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert!(!warnings.is_empty());
    assert_eq!(results[0]["post"]["status"], "sent");
    bot_notifications::deliver(&state).await?;
    let text = last_body(&mock, "sendMessage")["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(text.contains("опубликован с предупреждением"), "{text}");
    assert!(text.contains("&lt;bad&gt;"), "{text}");
    assert!(!text.contains(&config.bot_token), "{text}");
    let body: String = rich_studio::store::Store::connect(&state.account)?.query_row(
        "select body from bot_outbox",
        [],
        |row| row.get(0),
    )?;
    assert!(!body.contains(&config.bot_token), "{body}");
    server.abort();
    Ok(())
}
