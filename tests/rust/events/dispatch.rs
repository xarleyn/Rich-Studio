use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{accounts, import_forward, updates},
    store::{auth, bot_dialog as db, import_inbox, posts},
};
use serde_json::json;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn public_events_check_current_membership_and_update_only_own_storage()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    for uid in [11, 22] {
        accounts::initialize(&config, uid)?;
        auth::save_account(&config, &json!({"id":uid,"first_name":"test"}))?;
    }
    let a = AccountContext::account(&config, 11, Role::Publish);
    let b = AccountContext::account(&config, 22, Role::Publish);
    link(&a, -100500)?;
    link(&b, -100500)?;
    let pa = add(&a, -100500, 4, json!({"rich_message":{"markdown":"A"}}));
    let pb = add(&b, -100500, 4, json!({"rich_message":{"markdown":"B"}}));
    for (context, post) in [(&a, &pa), (&b, &pb)] {
        posts::update_delivery(
            context,
            post["id"].as_str().ok_or("id")?,
            Some(-100500),
            Some("poll-shared"),
            None,
        )?;
    }
    let state = AppState::new(config);
    updates::handle_update(&state,&json!({"message_reaction_count":{"chat":{"id":-100500},"message_id":4,"reactions":[{"type":{"type":"emoji","emoji":"🔥"},"total_count":3}]}})).await?;
    updates::handle_update(&state,&json!({"poll":{"id":"poll-shared","total_voter_count":8,"options":[{"text":"yes","voter_count":8}]}})).await?;
    assert_eq!(
        posts::get(&a, pa["id"].as_str().ok_or("id")?)?.ok_or("post")?["reactions"]["🔥"],
        3
    );
    assert_eq!(
        posts::get(&a, pa["id"].as_str().ok_or("id")?)?.ok_or("post")?["poll_results"]["total"],
        8
    );
    let bpost = posts::get(&b, pb["id"].as_str().ok_or("id")?)?.ok_or("post")?;
    assert!(bpost["reactions"].is_null());
    assert!(bpost["poll_results"].is_null());
    assert_eq!(mock.count("getChatMember"), 4);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn first_comment_checks_link_and_uncertain_send_is_never_repeated()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    accounts::initialize(&config, 11)?;
    auth::save_account(&config, &json!({"id":11,"first_name":"test"}))?;
    let context = AccountContext::account(&config, 11, Role::Publish);
    link(&context, -100500)?;
    let post = add(
        &context,
        -100500,
        6,
        json!({"rich_message":{"markdown":"main"},"comment":{"markdown":"first"}}),
    );
    let id = post["id"].as_str().ok_or("id")?;
    posts::update_delivery(&context, id, Some(-100500), None, Some("waiting"))?;
    let state = AppState::new(config);
    let event = json!({"update_id":1,"message":{"message_id":60,"chat":{"id":-100600},"is_automatic_forward":true,"forward_origin":{"type":"channel","chat":{"id":-100500},"message_id":6}}});
    mock.mode.store(1, Ordering::SeqCst);
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("comment")
    );
    let saved = posts::get(&context, id)?.ok_or("post")?;
    assert_eq!(saved["comment_status"], "failed");
    assert!(saved["error"].as_str().ok_or("error")?.contains("мог уйти"));
    updates::handle_update(&state, &event).await?;
    assert_eq!(mock.count("sendRichMessage"), 1);
    let second = add(
        &context,
        -100500,
        8,
        json!({"rich_message":{"markdown":"main"},"comment":{"markdown":"first"}}),
    );
    let sid = second["id"].as_str().ok_or("id")?;
    posts::update_delivery(&context, sid, Some(-100500), None, Some("waiting"))?;
    mock.mode.store(3, Ordering::SeqCst);
    let mut event = event;
    event["message"]["forward_origin"]["message_id"] = json!(8);
    updates::handle_update(&state, &event).await?;
    assert_eq!(mock.count("sendRichMessage"), 1);
    assert_eq!(
        posts::get(&context, sid)?.ok_or("post")?["comment_status"],
        "failed"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_commands_are_ignored_outside_own_private_chat()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let mut supergroup = private_ru(11, "/start");
    supergroup["message"]["chat"] = json!({"id":-1009,"type":"supergroup"});
    let mut other_private = private_ru(11, "/start");
    other_private["message"]["chat"] = json!({"id":888,"type":"private"});
    let mut bot = private_ru(11, "/start");
    bot["message"]["from"] = json!({"id":11,"is_bot":true});
    let mention = private_ru(11, "/start@SomeOtherBot");
    let mut forwarded = private_ru(11, "/start");
    forwarded["message"]["forward_origin"] = json!({"type":"hidden_user"});
    for update in [supergroup, other_private, bot, mention, forwarded] {
        assert!(updates::handle_update(&state, &update).await?.is_none());
    }
    assert_eq!(mock.count("sendMessage"), 0);
    assert!(db::contact(&config, 11)?.is_none());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_owner_only_in_private_mode_and_username_mentions()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    assert!(
        updates::handle_update(&state, &private_ru(888, "/start"))
            .await?
            .is_none()
    );
    assert!(db::contact(&config, 888)?.is_none());
    assert_eq!(mock.count("sendMessage"), 0);
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start@FiXtUrE")).await?,
        Some("bot_command")
    );
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn public_bot_commands_do_not_create_or_read_another_account()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    let state = AppState::new(config.clone());
    stored(&state.account, &json!("@operator"), "sent", Some(1), None);
    assert_eq!(
        updates::handle_update(&state, &private_ru(101, "/start")).await?,
        Some("bot_command")
    );
    updates::handle_update(&state, &private_ru(101, "/status 777")).await?;
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("status notice")?
            .contains("Сначала")
    );
    assert!(auth::account_ids(&config)?.is_empty());
    assert!(!config.account_dir(101).exists());
    for uid in [101, 202] {
        auth::save_account(&config, &json!({"id":uid,"first_name":"Person"}))?;
        accounts::initialize(&config, uid)?;
        let context = AccountContext::account(&config, uid, Role::Publish);
        stored(&context, &json!("@own"), "failed", None, None);
        if uid == 202 {
            stored(&context, &json!("@secret"), "failed", None, None);
        }
    }
    // Another person's id in the text never selects their database.
    updates::handle_update(&state, &private_ru(101, "/status 202")).await?;
    let text = notice_texts(&mock).last().cloned().unwrap_or_default();
    assert!(text.contains("Требуют проверки: 1"), "{text}");
    assert!(text.contains("Опубликовано: 0"), "{text}");
    assert!(AccountContext::root(&config).account_id.is_none());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn import_start_is_not_replaced_by_the_welcome_message()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let ticket = import_inbox::create(&state.account, "https://t.me/test_channel/10", 11, now())?;
    let token = ticket["id"].as_str().ok_or("ticket id")?.to_owned();
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, &format!("/start import_{token}"))).await?,
        Some("import")
    );
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("import notice")?
            .contains("перешлите")
    );
    assert_eq!(
        import_forward::get(&state.account, &token)?["state"],
        "armed"
    );
    assert!(db::contact(&config, 11)?.is_none());
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start import_missing")).await?,
        Some("bot_command")
    );
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("expired notice")?
            .contains("устарел")
    );
    server.abort();
    Ok(())
}
