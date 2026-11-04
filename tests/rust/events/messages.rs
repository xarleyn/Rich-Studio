use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    errors::ServiceError,
    services::{bot_dialog, bot_notifications, kinds, updates},
    store::{auth, bot_dialog as db},
    telegram::{TelegramError, uploads::Uploads},
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn bot_sender_binding_and_unsubscribe_survive_reply_rate_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    assert!(!bot_dialog::handle(&state, &private(22, "/start")).await?);
    let mut forged = private(11, "/start");
    forged["message"]["chat"]["id"] = json!(22);
    assert!(!bot_dialog::handle(&state, &forged).await?);
    assert!(bot_dialog::handle(&state, &private(11, "/start")).await?);
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "all"
    );
    for _ in 0..12 {
        auth::take_budget(&state.settings, "bot-dialog:11", 12, 60)?;
    }
    assert!(bot_dialog::handle(&state, &private(11, "/stop")).await?);
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "off"
    );
    assert_eq!(mock.count("sendMessage"), 1);
    // Repeated /start preserves an explicit opt-out, including after a restart.
    db::touch(&state.settings, 11, "en", None, true)?;
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "off"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_start_help_menu_preferences_and_registered_commands()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    let state = AppState::new(config.clone());
    updates::handle_update(&state, &private_ru(11, "/help")).await?;
    // Help does not opt in and does not prevent a later /start.
    assert!(db::contact(&config, 11)?.is_none());
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("help notice")?
            .contains("/notifications")
    );
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start")).await?,
        Some("bot_command")
    );
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
    assert_eq!(
        last_body(&mock, "sendMessage")["reply_markup"]["inline_keyboard"][0][0]["url"],
        "https://studio.example"
    );
    for command in ["/status", "/studio", "/notifications", "/unknown", "Привет"] {
        assert_eq!(
            updates::handle_update(&state, &private_ru(11, command)).await?,
            Some("bot_command")
        );
    }
    let last = notice_texts(&mock).last().cloned().unwrap_or_default();
    assert!(!last.contains("<b>") || last.contains("Rich"));
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    updates::handle_update(&state, &private_ru(11, "/start")).await?;
    // /start never undoes an explicit opt-out.
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    updates::handle_update(&state, &private_ru(11, "/notifications errors")).await?;
    assert_eq!(
        db::contact(&config, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    // Migration/restart preserves the subscription.
    let restarted = AppState::new(config.clone());
    assert_eq!(
        db::contact(&restarted.settings, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    bot_dialog::register_commands(&state).await?;
    let commands: Vec<Value> = mock
        .calls()
        .into_iter()
        .filter(|(method, _)| method == "setMyCommands")
        .map(|(_, body)| body)
        .collect();
    let mut languages: Vec<&str> = commands
        .iter()
        .filter_map(|command| command["language_code"].as_str())
        .collect();
    languages.sort_unstable();
    assert_eq!(languages, ["", "en", "ru"]);
    assert!(
        commands
            .iter()
            .all(|command| command["scope"] == json!({"type":"all_private_chats"}))
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn private_dm_rejects_other_recipients_and_arbitrary_sends()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    let transport = state.transport();
    assert!(matches!(
        bot_dialog::send_private(&context, &config, &transport, 202, "secret", json!({})).await,
        Err(ServiceError::BadRequest(_))
    ));
    bot_dialog::send_private(&context, &config, &transport, 101, "hello", json!({})).await?;
    assert!(matches!(
        transport
            .call(
                &context,
                "sendMessage",
                json!({"chat_id":101,"text":"arbitrary"}),
                Uploads::default()
            )
            .await,
        Err(TelegramError::Policy(_))
    ));
    assert_eq!(mock.count("sendMessage"), 1);
    assert_eq!(last_body(&mock, "sendMessage")["chat_id"], 101);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reply_transport_failure_restores_account_policy() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    let transport = state.transport();
    mock.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        bot_dialog::send_private(&context, &config, &transport, 101, "hello", json!({})).await,
        Err(ServiceError::Telegram(TelegramError::NoAnswer(_)))
    ));
    assert!(matches!(
        transport
            .call(
                &context,
                "sendMessage",
                json!({"chat_id":101,"text":"leak"}),
                Uploads::default()
            )
            .await,
        Err(TelegramError::Policy(_))
    ));
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn english_bot_commands_and_reports() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    updates::handle_update(&state, &private_in(11, "/start", "en-US")).await?;
    assert!(notice_texts(&mock).last().ok_or("welcome")?.contains("Hi!"));
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("report")?
            .contains("Post published")
    );
    server.abort();
    Ok(())
}
