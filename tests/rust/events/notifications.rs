use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{accounts, bot_notifications, kinds, updates},
    store::{auth, bot_dialog as db, meta},
    telegram::client::TelegramTransport,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn notification_revision_scope_and_attempt_claim_prevent_duplicates()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let context = AccountContext::root(&config);
    let transport = TelegramTransport::new(base, "1:fixture".to_owned());
    db::touch(&config, 11, "en", None, true)?;
    let post = add(
        &context,
        -100500,
        1,
        json!({"rich_message":{"markdown":"<b>safe</b>"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    let old = db::due(&context, 10)?.remove(0);
    db::disable(&config, 11)?;
    db::touch(&config, 11, "en", Some("all"), false)?;
    bot_notifications::deliver_one(&context, &config, &transport, &old).await?;
    assert_eq!(mock.count("sendMessage"), 0);
    let post = add(
        &context,
        -100500,
        2,
        json!({"rich_message":{"markdown":"Title"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    let event = db::due(&context, 10)?.remove(0);
    mock.mode.store(1, Ordering::SeqCst);
    bot_notifications::deliver_one(&context, &config, &transport, &event).await?;
    bot_notifications::deliver_one(&context, &config, &transport, &event).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(db::due(&context, 10)?.is_empty());
    let outbox_state: String = rich_studio::store::Store::connect(&context)?.query_row(
        "select state from bot_outbox where event_key=?",
        [event["event_key"].as_str().ok_or("key")?],
        |row| row.get(0),
    )?;
    assert_eq!(outbox_state, "attempted");
    let other = AccountContext::account(&config, 22, Role::Publish);
    accounts::initialize(&config, 22)?;
    db::enqueue(&other, "tampered", 11, "error", &event["body"])?;
    let tampered = db::due(&other, 10)?.remove(0);
    bot_notifications::deliver_one(&other, &config, &transport, &tampered).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn notifications_retry_only_known_non_delivery_and_blocked_recipient_opts_out()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let context = AccountContext::root(&config);
    db::touch(&config, 11, "en", Some("errors"), false)?;
    let mut post = add(
        &context,
        -100500,
        1,
        json!({"rich_message":{"markdown":"A"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    assert!(db::due(&context, 10)?.is_empty());
    post["status"] = json!("failed");
    post["error"] = json!("fixture refusal");
    bot_notifications::record(&context, &config, &post, None).await?;
    let event = db::due(&context, 10)?.remove(0);
    let unreachable =
        TelegramTransport::new("http://127.0.0.1:0".to_owned(), "1:fixture".to_owned());
    bot_notifications::deliver_one(&context, &config, &unreachable, &event).await?;
    let (state, retry_at): (String, i64) = rich_studio::store::Store::connect(&context)?
        .query_row(
            "select state,retry_at from bot_outbox where event_key=?",
            [event["event_key"].as_str().ok_or("key")?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    assert_eq!(state, "pending");
    assert!(retry_at >= auth::now() + 59);
    assert!(db::due(&context, 10)?.is_empty());
    db::finish(
        &context,
        event["event_key"].as_str().ok_or("key")?,
        "pending",
        0,
    )?;
    let due = db::due(&context, 10)?.remove(0);
    mock.mode.store(2, Ordering::SeqCst);
    bot_notifications::deliver_one(
        &context,
        &config,
        &TelegramTransport::new(base, "1:fixture".to_owned()),
        &due,
    )
    .await?;
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(db::due(&context, 10)?.is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn leader_delivers_reports_even_when_updates_are_disabled_and_joins_on_stop()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.background = true;
    config.updates_on = false;
    config.spec_pinned = true;
    config.backup_keep = 0;
    config.shutdown_grace = 3.0;
    let state = AppState::new(config);
    db::touch(&state.settings, 11, "en", None, true)?;
    let post = add(
        &state.account,
        -100500,
        8,
        json!({"rich_message":{"markdown":"Published"}}),
    );
    bot_notifications::record(&state.account, &state.settings, &post, None).await?;
    let loops = rich_studio::services::loops::Loops::start(state.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if mock.count("sendMessage") == 1 {
                break;
            }
            mock.changed.notified().await;
        }
    })
    .await?;
    loops.request_stop();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !state.loop_status.read().await.stopping {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        meta::lease_holder(&state.account, "loops")?,
        Some(state.loop_status.read().await.owner.clone())
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), loops.stop()).await??;
    assert_eq!(mock.count("setMyCommands"), 3);
    assert_eq!(mock.count("getUpdates"), 0);
    assert_eq!(state.loop_status.read().await.role, "off");
    assert!(meta::lease_holder(&state.account, "loops")?.is_none());
    assert!(db::due(&state.account, 10)?.is_empty());
    let calls = mock.calls().len();
    tokio::task::yield_now().await;
    assert_eq!(mock.calls().len(), calls);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn public_notifications_are_scoped_and_private() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    let state = AppState::new(config.clone());
    for uid in [101, 202] {
        auth::save_account(&config, &json!({"id":uid,"first_name":"Person"}))?;
        accounts::initialize(&config, uid)?;
        subscribe_with(&config, uid, "all")?;
        let context = AccountContext::account(&config, uid, Role::Publish);
        let post = stored(
            &context,
            &json!(format!("@channel_{uid}")),
            "sent",
            Some(10),
            None,
        );
        bot_notifications::record(&context, &config, &post, None).await?;
    }
    // A corrupted destination cannot leak this account's report.
    let context = AccountContext::account(&config, 101, Role::Publish);
    db::enqueue(
        &context,
        "forged",
        202,
        "success",
        &json!({"title":"private"}),
    )?;
    bot_notifications::deliver(&state).await?;
    let notices = notices(&mock);
    let mut chats = notices
        .iter()
        .map(|body| body["chat_id"].as_i64().ok_or("chat"))
        .collect::<Result<Vec<_>, _>>()?;
    chats.sort_unstable();
    assert_eq!(chats, [101, 202]);
    for body in &notices {
        let text = body["text"].as_str().unwrap_or_default();
        assert!(
            text.contains(&format!("@channel_{}", body["chat_id"])),
            "{text}"
        );
        assert!(!text.contains("private"), "{text}");
    }
    server.abort();
    Ok(())
}

#[tokio::test]
async fn unsubscribe_then_resubscribe_does_not_replay_old_reports()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@old")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    updates::handle_update(&state, &private_ru(11, "/notifications all")).await?;
    mock.clear_calls();
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@new")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("@new"), "{texts:?}");
    server.abort();
    Ok(())
}
