use std::collections::BTreeSet;

use axum::http::StatusCode;
use rich_studio::AppState;
use rich_studio::build_router;
use rich_studio::services::updates;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, settings, state, telegram};

#[tokio::test]
async fn pin_after_send_and_pin_endpoints() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let mut post = blocks_post();
    post["pin"] = json!(true);
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let pin = telegram.last("pinChatMessage").ok_or("pin")?.body;
    assert_eq!(
        pin,
        json!({"chat_id":-1001,"message_id":reply.value["message"]["message_id"],"disable_notification":true})
    );
    assert_eq!(reply.value["post"]["pinned"], 1);
    assert_eq!(reply.value["post"]["chat_num"], -1001);
    let sent = telegram.last("sendRichMessage").ok_or("send")?;
    assert_eq!(
        sent.body
            .as_object()
            .map(|object| object.keys().cloned().collect::<BTreeSet<_>>()),
        Some(BTreeSet::from([
            "chat_id".to_owned(),
            "rich_message".to_owned()
        ]))
    );

    telegram.fail("pinChatMessage", "Bad Request: not enough rights");
    let mut failing = blocks_post();
    failing["pin"] = json!(true);
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":failing})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("закрепить")),
        "{}",
        reply.text
    );
    telegram.clear_fail();

    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]
        .clone();
    let id = sent["id"].as_str().ok_or("id")?;
    let pinned = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/pin"),
        Some(json!({"disable_notification":false})),
        &[],
    )
    .await;
    assert_eq!(pinned.value["pinned"], 1);
    assert_eq!(
        telegram.last("pinChatMessage").ok_or("pin")?.body["disable_notification"],
        false
    );
    let unpinned = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}/pin"),
        None,
        &[],
    )
    .await;
    assert_eq!(unpinned.value["pinned"], 0);
    assert_eq!(
        telegram.last("unpinChatMessage").ok_or("unpin")?.body["message_id"],
        sent["message_id"]
    );

    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/pin"),
            None,
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        telegram.last("pinChatMessage").ok_or("pin")?.body["disable_notification"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn button_endpoint_replaces_markup_and_checks_links() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let id = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]["id"]
        .as_str()
        .ok_or("id")?
        .to_owned();
    let keyboard = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let changed = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard,"callbacks":{"like":"Спасибо!"}})),
        &[],
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    assert_eq!(
        telegram
            .last("editMessageReplyMarkup")
            .ok_or("markup")?
            .body["reply_markup"],
        keyboard
    );
    assert_eq!(changed.value["payload"]["reply_markup"], keyboard);
    assert_eq!(
        changed.value["payload"]["callbacks"],
        json!({"like":"Спасибо!"})
    );
    assert_eq!(telegram.count("editMessageText"), 0);

    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/buttons"),
            Some(json!({"reply_markup":null})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let cleared = telegram
        .last("editMessageReplyMarkup")
        .ok_or("markup")?
        .body;
    assert!(cleared.get("reply_markup").is_none(), "{cleared}");

    let calls = telegram.calls().len();
    let bad = json!({"inline_keyboard":[[{"text":"Сайт","url":""}]]});
    let refused = call(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":bad})),
        &[],
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );
    assert!(refused.text.contains("другой тип"), "{}", refused.text);
    assert_eq!(telegram.calls().len(), calls);
    Ok(())
}

#[tokio::test]
async fn callback_clicks_are_answered_named_and_counted() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut post = blocks_post();
    post["reply_markup"] = button;
    post["callbacks"] = json!({"like":{"text":"Голос учтён","show_alert":true}});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await
    .value;
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    let mut update = json!({
        "update_id":1,
        "callback_query":{
            "id":"cq1",
            "data":"like",
            "from":{"id":5},
            "message":{"message_id":message_id,"chat":{"id":-1001}}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("callback")
    );
    assert_eq!(
        telegram.last("answerCallbackQuery").ok_or("answer")?.body,
        json!({"callback_query_id":"cq1","text":"Голос учтён","show_alert":true})
    );
    update["callback_query"]["id"] = json!("cq2");
    updates::handle_update(&state, &update).await?;
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    let stored = call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(stored["clicks"], json!({"like":2}));
    let listed = call(app, "GET", "/api/posts", None, &[]).await.value;
    let row = listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|post| post["id"] == id)
        .ok_or("listed")?;
    assert_eq!(row["clicks"], json!({"like":2}));
    assert_eq!(row["buttons"], json!({"like":"👍"}));

    updates::handle_update(
        &state,
        &json!({
            "update_id":2,
            "callback_query":{"id":"cq3","data":"x","message":{"message_id":1,"chat":{"id":-5}}}
        }),
    )
    .await?;
    assert_eq!(
        telegram.last("answerCallbackQuery").ok_or("answer")?.body,
        json!({"callback_query_id":"cq3"})
    );
    Ok(())
}
