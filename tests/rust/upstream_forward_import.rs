#[path = "publication_safety/support.rs"]
#[allow(dead_code)]
mod support;

use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{accounts, import_forward, updates},
    spec::source::Spec,
    store::{auth, db, drafts},
};
use serde_json::{Value, json};
use support::*;
use tempfile::TempDir;

fn forward(uid: i64, mid: i64) -> Value {
    json!({"update_id":mid,"message":{"message_id":mid,"date":123456,"from":{"id":uid,"is_bot":false},"chat":{"id":uid,"type":"private"},"text":"Original text","forward_origin":{"type":"channel","chat":{"id":-100123,"username":"original","title":"Original channel"},"message_id":10}}})
}

fn full_drafts(context: &AccountContext) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    db::migrate(&mut db::Store::connect(context)?)?;
    drafts::list(context)?
        .iter()
        .map(|row| {
            drafts::get(context, row["id"].as_str().ok_or("draft id")?)?
                .ok_or_else(|| "draft missing".into())
        })
        .collect()
}

#[tokio::test]
async fn owner_can_forward_without_ticket_preserving_rich_content_and_safe_buttons()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, _| ok(json!(true))).await;
    let mut config = settings(&dir, &base);
    config.owner_telegram_id = 501;
    config.preview_chat = "999".to_owned();
    let state = AppState::new(config);
    let mut update = forward(501, 1);
    update["message"]
        .as_object_mut()
        .ok_or("message")?
        .remove("text");
    update["message"]["rich_message"] = json!({"is_rtl":true,"blocks":[{"type":"paragraph","text":["Keep ",{"type":"bold","text":"formatting"}]},{"type":"photo","photo":[{"file_id":"small","width":1,"height":1},{"file_id":"large","width":10,"height":10}]},{"type":"buttons","buttons":[{"text":"Private answer","callback_data":"other-bot"},{"text":"Link","url":"https://example.com"}]}]});
    update["message"]["reply_markup"] = json!({"inline_keyboard":[[{"text":"Login","login_url":{"url":"https://example.com"}},{"text":"Copy","copy_text":{"text":"hello"}}]]});
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("forward_import")
    );
    let saved = full_drafts(&state.account)?.remove(0);
    assert_eq!(saved["chat_ids"], Value::Null);
    let post = &saved["payload"];
    assert_eq!(post["rich_message"]["is_rtl"], true);
    assert_eq!(post["rich_message"]["blocks"][1]["photo"]["media"], "large");
    assert_eq!(
        post["rich_message"]["blocks"][2]["buttons"][0]["url"],
        "https://example.com"
    );
    assert_eq!(
        post["reply_markup"]["inline_keyboard"][0][0]["copy_text"]["text"],
        "hello"
    );
    assert!(!post.to_string().contains("other-bot"));
    assert_eq!(
        post["_studio"]["source"]["link"],
        "https://t.me/original/10"
    );
    let calls = sent_payloads(&mut requests, "setMessageReaction");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["chat_id"], 501);
    assert_eq!(calls[0]["reaction"][0]["emoji"], "✍");
    assert_eq!(sent_count(&mut requests, "getFile"), 0);
    Ok(())
}

#[tokio::test]
async fn public_forwards_are_scoped_to_registered_sender_and_unknown_users_are_ignored()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let mut config = settings(&dir, &base);
    config.public = true;
    config.preview_chat.clear();
    for uid in [111, 222] {
        accounts::initialize(&config, uid)?;
        auth::save_account(&config, &json!({"id":uid,"first_name":"User"}))?;
    }
    let state = AppState::new(config.clone());
    let spec = Spec::load_initial(&config);
    assert!(import_forward::receive_automatic(&state, &forward(111, 1), &spec).await?);
    assert!(!import_forward::receive_automatic(&state, &forward(333, 1), &spec).await?);
    assert_eq!(
        full_drafts(&AccountContext::account(&config, 111, Role::Owner))?.len(),
        1
    );
    assert!(full_drafts(&AccountContext::account(&config, 222, Role::Owner))?.is_empty());
    assert!(full_drafts(&AccountContext::root(&config))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn album_items_combine_but_a_persons_intervening_edit_is_never_overwritten()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let mut config = settings(&dir, &base);
    config.preview_chat = "777".to_owned();
    let state = AppState::new(config.clone());
    let spec = Spec::load_initial(&config);
    let mut first = forward(777, 1);
    first["message"]["media_group_id"] = json!("album");
    first["message"]["photo"] = json!([{"file_id":"photo","width":10,"height":10}]);
    first["message"]["show_caption_above_media"] = json!(true);
    assert!(import_forward::receive_automatic(&state, &first, &spec).await?);
    let mut second = forward(777, 2);
    second["message"]["media_group_id"] = json!("album");
    second["message"]["video"] = json!({"file_id":"video"});
    second["message"]
        .as_object_mut()
        .ok_or("message")?
        .remove("text");
    assert!(import_forward::receive_automatic(&state, &second, &spec).await?);
    let saved = full_drafts(&state.account)?;
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0]["payload"]["rich_message"]["blocks"][0]["type"],
        "paragraph"
    );
    assert_eq!(
        saved[0]["payload"]["rich_message"]["blocks"][1]["type"],
        "collage"
    );
    assert_eq!(
        saved[0]["payload"]["rich_message"]["blocks"][1]["blocks"]
            .as_array()
            .ok_or("album")?
            .len(),
        2
    );
    let id = saved[0]["id"].as_str().ok_or("id")?;
    drafts::save(
        &state.account,
        Some(id),
        "My edit",
        &json!({"rich_message":{"markdown":"Do not overwrite"}}),
        saved[0]["version"].as_i64(),
        None,
        None,
    )?;
    let mut third = first.clone();
    third["message"]["message_id"] = json!(3);
    third["message"]["photo"][0]["file_id"] = json!("third");
    assert!(import_forward::receive_automatic(&state, &third, &spec).await?);
    assert_eq!(full_drafts(&state.account)?.len(), 2);
    assert_eq!(
        drafts::get(&state.account, id)?.ok_or("edited")?["payload"]["rich_message"]["markdown"],
        "Do not overwrite"
    );
    assert!(import_forward::receive_automatic(&state, &third, &spec).await?);
    assert_eq!(full_drafts(&state.account)?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn protected_or_bot_buttons_only_forward_creates_no_empty_draft()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, _| ok(json!(true))).await;
    let mut config = settings(&dir, &base);
    config.preview_chat = "777".to_owned();
    let spec = Spec::load_initial(&config);
    let state = AppState::new(config);
    let mut protected = forward(777, 1);
    protected["message"]["has_protected_content"] = json!(true);
    assert!(import_forward::receive_automatic(&state, &protected, &spec).await?);
    let mut buttons = forward(777, 2);
    buttons["message"]["rich_message"] =
        json!({"blocks":[{"type":"buttons","buttons":[{"text":"Pay","pay":true}]}]});
    assert!(import_forward::receive_automatic(&state, &buttons, &spec).await?);
    assert!(full_drafts(&state.account)?.is_empty());
    let replies = sent_payloads(&mut requests, "sendMessage");
    assert_eq!(replies.len(), 2);
    assert!(replies.iter().all(|reply| {
        reply["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Черновик не создан"))
    }));
    Ok(())
}

#[tokio::test]
async fn forwarded_copies_keep_caption_entities_and_ignore_groups_bots_and_other_senders()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let mut config = settings(&dir, &base);
    config.preview_chat = "777".to_owned();
    let spec = Spec::load_initial(&config);
    let state = AppState::new(config);
    let mut copy = forward(777, 1);
    copy["message"]
        .as_object_mut()
        .ok_or("message")?
        .remove("forward_origin");
    copy["message"]["rich_message"] =
        json!({"blocks":[{"type":"paragraph","text":"Copied rich post"}]});
    assert!(import_forward::receive_automatic(&state, &copy, &spec).await?);
    let mut caption = forward(777, 5);
    caption["message"]
        .as_object_mut()
        .ok_or("message")?
        .remove("text");
    caption["message"]["photo"] = json!([{"file_id":"photo","width":10,"height":10}]);
    caption["message"]["caption"] = json!("Bold");
    caption["message"]["caption_entities"] = json!([{"type":"bold","offset":0,"length":4}]);
    assert!(import_forward::receive_automatic(&state, &caption, &spec).await?);
    let saved = full_drafts(&state.account)?;
    assert!(saved.iter().any(
        |draft| draft["payload"]["rich_message"]["blocks"][1]["text"]
            == json!([{"type":"bold","text":"Bold"}])
    ));
    for mut invalid in [forward(777, 2), forward(777, 3), forward(888, 4)] {
        if invalid["message"]["message_id"] == 2 {
            invalid["message"]["chat"]["type"] = json!("supergroup");
        }
        if invalid["message"]["message_id"] == 3 {
            invalid["message"]["from"]["is_bot"] = json!(true);
        }
        assert!(!import_forward::receive_automatic(&state, &invalid, &spec).await?);
    }
    assert_eq!(full_drafts(&state.account)?.len(), 2);
    Ok(())
}
