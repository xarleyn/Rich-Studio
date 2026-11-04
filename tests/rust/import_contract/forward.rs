use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{accounts, import_forward, imports, updates},
    spec::source::Spec,
    store::{auth, db, drafts, import_inbox},
};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn forward_requires_account_local_ticket_and_is_single_use() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.preview_chat = "777".into();
    settings.bot_username = "fixture_bot".into();
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let mut update = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Forwarded","forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    assert!(
        import_inbox::get(
            &AccountContext::account(&settings, 999, Role::Owner),
            id,
            auth::now()
        )?
        .is_none()
    );
    let arm = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(import_forward::receive(&settings, &arm, &spec).await?);
    update["message"]["from"]["id"] = json!(888);
    update["message"]["chat"]["id"] = json!(888);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    update["message"]["from"]["id"] = json!(777);
    update["message"]["chat"]["id"] = json!(777);
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    let result = import_forward::get(&context, id)?;
    assert_eq!(result["state"], "done");
    assert_eq!(drafts::list(&context)?.len(), 1);
    let other = AccountContext::account(&settings, 999, Role::Owner);
    db::migrate(&mut db::Store::connect(&other)?)?;
    assert!(drafts::list(&other)?.is_empty());
    assert_eq!(result["result"]["source"]["provider"], "forward");
    Ok(())
}

#[tokio::test]
async fn wrong_forward_consumes_ticket_with_error_without_draft() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.updates_on = true;
    settings.bot_token = "fixture".into();
    settings.preview_chat = "777".into();
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    assert!(import_inbox::arm(&context, id, 777, auth::now())?);
    assert!(import_forward::receive(&settings,&json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Wrong","forward_origin":{"type":"channel","chat":{"username":"different"},"message_id":203}}}),&spec).await?);
    assert_eq!(import_forward::get(&context, id)?["state"], "failed");
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[test]
fn forward_rich_actions_removed_unknown_fields_and_largest_photo_preserved() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let result = imports::from_forward(
        &context,
        &spec,
        &json!({"rich_message":{"blocks":[{"type":"buttons","buttons":[{"type":"button","callback_data":"evil"}]}]}}),
        &link()?,
        vec!["Origin not verified".into()],
    )?;
    assert_eq!(result["warnings"][0], "Origin not verified");
    let saved = drafts::get(&context, result["draft"]["id"].as_str().ok_or("draft id")?)?
        .ok_or("saved draft")?;
    assert_eq!(
        saved["payload"]["_studio"]["import_warnings"][0],
        "Origin not verified"
    );
    assert_eq!(
        result["draft"]["payload"]["rich_message"]["blocks"][0]["type"],
        "paragraph"
    );
    assert!(!result.to_string().contains("evil"));
    let photo = imports::from_forward(
        &context,
        &spec,
        &json!({"photo":[{"file_id":"small","width":1,"height":1},{"file_id":"large","width":10,"height":10}],"caption":"Caption"}),
        &link()?,
        vec![],
    )?;
    assert_eq!(
        photo["draft"]["payload"]["rich_message"]["blocks"][0]["photo"]["media"],
        "large"
    );
    Ok(())
}

#[tokio::test]
async fn forward_ticket_is_account_local_and_other_user_cannot_arm_it() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.public = true;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.bot_username = "fixture_bot".into();
    for uid in [111, 222] {
        accounts::initialize(&settings, uid)?;
        auth::save_account(&settings, &json!({"id":uid,"first_name":"User"}))?;
    }
    let spec = Spec::load_initial(&settings);
    let one = AccountContext::account(&settings, 111, Role::Owner);
    let two = AccountContext::account(&settings, 222, Role::Owner);
    let ticket = import_forward::begin(&settings, &one, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let foreign_arm = json!({"message":{"from":{"id":222},"chat":{"id":222,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(!import_forward::receive(&settings, &foreign_arm, &spec).await?);
    let update = json!({"message":{"from":{"id":111},"chat":{"id":111,"type":"private"},"text":"Forwarded","forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    let own_arm = json!({"message":{"from":{"id":111},"chat":{"id":111,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(import_forward::receive(&settings, &own_arm, &spec).await?);
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    assert_eq!(import_forward::get(&one, id)?["state"], "done");
    assert_eq!(drafts::list(&one)?.len(), 1);
    db::migrate(&mut db::Store::connect(&two)?)?;
    assert!(drafts::list(&two)?.is_empty());
    assert!(import_inbox::get(&two, id, auth::now())?.is_none());
    let root = AccountContext::root(&settings);
    db::migrate(&mut db::Store::connect(&root)?)?;
    assert!(drafts::list(&root)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn forward_database_limit_finishes_ticket_with_error() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.public = true;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.bot_username = "fixture_bot".into();
    settings.account_database_mb = 1.0;
    accounts::initialize(&settings, 777)?;
    auth::save_account(&settings, &json!({"id":777,"first_name":"User"}))?;
    let context = AccountContext::account(&settings, 777, Role::Owner);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    assert!(import_inbox::arm(&context, id, 777, auth::now())?);
    fill_account_database(&context, 2)?;
    let update = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"x".repeat(90_000),"forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    let result = import_forward::get(&context, id)?;
    assert_eq!(result["state"], "failed");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Лимит базы"),
        "{result}"
    );
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn forward_update_only_acknowledges_in_own_private_chat() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mock, server) = bot_mock().await?;
    let mut settings = settings(&dir)?;
    settings.api_base = base;
    settings.bot_token = "1:fixture".into();
    settings.preview_chat = "777".into();
    settings.bot_username = "fixture".into();
    settings.updates_on = true;
    let state = AppState::new(settings.clone());
    let ticket =
        import_forward::begin(&settings, &AccountContext::root(&settings), &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let arm = json!({"message":{"message_id":1,"from":{"id":777},"chat":{"id":777,"type":"private"},"text":format!("/start import_{id}")}});
    assert_eq!(updates::handle_update(&state, &arm).await?, Some("import"));
    let forward = json!({"message":{"message_id":2,"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Hello","forward_origin":{"type":"hidden_user"}}});
    assert_eq!(
        updates::handle_update(&state, &forward).await?,
        Some("import")
    );
    let calls = mock.all();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|(method, body)| method == "sendMessage" && body["chat_id"] == json!(777)),
        "{calls:?}"
    );
    assert!(
        calls[0].1["text"]
            .as_str()
            .unwrap_or_default()
            .contains("перешлите"),
        "{calls:?}"
    );
    assert!(
        calls[1].1["text"]
            .as_str()
            .unwrap_or_default()
            .contains("обработана"),
        "{calls:?}"
    );
    server.abort();
    Ok(())
}
