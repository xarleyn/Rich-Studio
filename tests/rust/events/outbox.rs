use rich_studio::{
    AppState,
    services::{bot_notifications, kinds, updates},
    store::bot_dialog as db,
};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn outbox_failure_cannot_change_a_successful_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger fail_outbox before insert on bot_outbox begin select raise(abort, 'disk full'); end;",
    )?;
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(results[0]["post"]["status"], "sent");
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn partial_publication_reports_each_channel_once() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"refused"}),
    );
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@bad"), json!("@good")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(
        results
            .iter()
            .map(|result| result["ok"].clone())
            .collect::<Vec<_>>(),
        [json!(false), json!(true)]
    );
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts
            .iter()
            .any(|text| text.contains("@bad") && text.contains("Не удалось")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("@good") && text.contains("опубликован")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn outbox_claim_survives_restart_and_is_atomic() -> Result<(), Box<dyn std::error::Error>> {
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
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let context = state.account.clone();
        let key = key.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            db::claim(&context, &key)
        }));
    }
    let mut claimed = Vec::new();
    for task in tasks {
        claimed.push(task.await??);
    }
    claimed.sort_unstable();
    assert_eq!(claimed, [false, true]);
    let restarted = AppState::new(config.clone());
    bot_notifications::deliver(&restarted).await?;
    assert_eq!(mock.count("sendMessage"), 0);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn first_comment_failure_is_reported_in_real_time() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let mut post = blocks_post();
    post["comment"] = json!({"rich_message":{"markdown":"first"}});
    mock.respond(
        "sendRichMessage",
        json!({"ok":true,"result":{"message_id":99,"chat":{"id":-1001}}}),
    );
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@channel")],
            &post,
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let saved = results[0]["post"].clone();
    assert_eq!(saved["comment_status"], "waiting");
    let update = json!({"message":{
        "message_id":999,
        "chat":{"id":-500,"type":"supergroup"},
        "is_automatic_forward":true,
        "forward_origin":{
            "type":"channel",
            "chat":{"id":saved["chat_num"]},
            "message_id":saved["message_id"]
        }
    }});
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"discussion unavailable"}),
    );
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("comment")
    );
    bot_notifications::deliver(&state).await?;
    assert!(
        notice_texts(&mock)
            .iter()
            .any(|text| text.contains("первый комментарий"))
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn after_send_failure_keeps_the_publication_and_warns()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger fail_delivery before update of chat_num on posts \
         begin select raise(abort, 'disk full'); end;",
    )?;
    let (results, warnings) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@one"), json!("@two")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result["ok"] == true));
    assert!(!warnings.is_empty());
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts.iter().all(|text| text.contains("с предупреждением")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}
