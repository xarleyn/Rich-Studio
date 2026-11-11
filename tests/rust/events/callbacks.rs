use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{accounts, updates},
    store::{auth, bot_dialog as db, meta},
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn preview_callback_keeps_user_meaning_and_newest_twenty_answers()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    // Lexicographic order differs from insertion order around message 9/10.
    for message in 1..=21 {
        updates::remember_preview(
            &state.account,
            &json!(11),
            message,
            &json!({"rsbot:stop":"content answer"}),
        )
        .await?;
    }
    let saved: Value = serde_json::from_str(
        &meta::get(&state.account, updates::PREVIEW_KEY)?.ok_or("callbacks")?,
    )?;
    assert_eq!(saved.as_object().ok_or("object")?.len(), 20);
    assert!(saved.get("11:1").is_none());
    assert!(saved.get("11:21").is_some());
    assert_eq!(
        updates::handle_update(&state, &callback(1, 11, 21, "rsbot:stop")).await?,
        Some("callback")
    );
    assert_eq!(mock.count("sendMessage"), 0);
    assert!(db::contact(&state.settings, 11)?.is_none());
    assert_eq!(mock.calls()[0].1["text"], "content answer");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_callback_preferences_and_reply_budget() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let update = callback(1, 11, 1, "rsbot:notifications errors");
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("bot_command")
    );
    assert_eq!(
        db::contact(&config, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    assert_eq!(
        last_body(&mock, "answerCallbackQuery")["callback_query_id"],
        "cq1"
    );
    for _ in 0..20 {
        updates::handle_update(&state, &private_ru(11, "/help")).await?;
    }
    assert_eq!(mock.count("sendMessage"), 12);
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    assert_eq!(mock.count("sendMessage"), 12);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn preview_never_queues_a_publication_notice() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::services::posts::from_state(&state)
        .await
        .preview(&blocks_post(), &Value::Null)
        .await?;
    assert!(db::due(&state.account, 10)?.is_empty());
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn preview_callbacks_take_priority_over_bot_commands()
-> Result<(), Box<dyn std::error::Error>> {
    for public in [false, true] {
        for data in ["rsbot:help", "rsbot:notifications off"] {
            let dir = TempDir::new()?;
            let (base, mock, server) = mock().await?;
            let mut config = settings(&dir, &base);
            config.public = public;
            let state = AppState::new(config.clone());
            subscribe_with(&config, 11, "all")?;
            if public {
                auth::save_account(&config, &json!({"id":11,"first_name":"Person"}))?;
                accounts::initialize(&config, 11)?;
            }
            let context = if public {
                AccountContext::account(&config, 11, Role::Publish)
            } else {
                AccountContext::root(&config)
            };
            let mut callbacks = serde_json::Map::new();
            callbacks.insert(data.to_owned(), json!({"text":"custom preview answer"}));
            updates::remember_preview(&context, &json!(11), 42, &Value::Object(callbacks)).await?;
            assert_eq!(
                updates::handle_update(&state, &callback(1, 11, 42, data)).await?,
                Some("callback")
            );
            assert_eq!(
                last_body(&mock, "answerCallbackQuery")["text"],
                "custom preview answer"
            );
            assert_eq!(mock.count("sendMessage"), 0);
            assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
            server.abort();
        }
    }
    Ok(())
}
