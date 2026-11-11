use axum::http::StatusCode;
use rich_studio::services::channels::ChannelService;
use rich_studio::services::updates;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{blocks_post, chat_server, detail, fake_server, http_request, settings};

#[tokio::test]
async fn topics_from_updates_follow_creation_and_rename() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir, &chat_server().await));
    let service = ChannelService::new(
        state.account.clone(),
        state.transport(),
        state.settings.default_chat.clone(),
    );
    service.check(json!("-1002627012139")).await?;
    service.set_topics("-1002627012139", vec![json!({"id":7,"name":"Анонсы"})])?;
    let created = json!({
        "update_id": 1,
        "message": {
            "message_id": 50,
            "message_thread_id": 50,
            "chat": {"id": -1002627012139i64, "type": "supergroup"},
            "forum_topic_created": {"name": "Черновики", "icon_color": 1},
        },
    });
    assert_eq!(
        updates::handle_update(&state, &created).await?,
        Some("topic")
    );
    let renamed = json!({
        "update_id": 2,
        "message": {
            "message_id": 50,
            "message_thread_id": 50,
            "chat": {"id": -1002627012139i64, "type": "supergroup"},
            "forum_topic_created": null,
            "forum_topic_edited": {"name": "Идеи"},
        },
    });
    updates::handle_update(&state, &renamed).await?;
    let channel = service
        .list()?
        .into_iter()
        .find(|channel| channel["chat_id"] == "-1002627012139")
        .ok_or("channel with topics")?;
    assert_eq!(
        channel["topics"],
        json!([{"id":7,"name":"Анонсы"},{"id":50,"name":"Идеи"}])
    );
    Ok(())
}

#[tokio::test]
async fn draft_keeps_its_channels_and_publishing_uses_them()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, telegram) = fake_server().await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, draft) = http_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":blocks_post(),"chat_ids":["@chan_a"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["chat_ids"], json!(["@chan_a"]));
    let id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let (status, saved) = http_request(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":blocks_post(),"version":draft["version"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["chat_ids"], json!(["@chan_a"]));
    let (_, listed) = http_request(app.clone(), "GET", "/api/drafts", None, &[]).await;
    assert_eq!(listed[0]["chat_ids"], json!(["@chan_a"]));
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"draft_id":id})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?["body"]["chat_id"],
        "@chan_a"
    );
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"draft_id":id,"chat_id":"@chan_b"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?["body"]["chat_id"],
        "@chan_b"
    );
    let (status, cleared) = http_request(
        app,
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":blocks_post(),"chat_ids":[]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(cleared["chat_ids"].is_null());
    Ok(())
}

#[tokio::test]
async fn topic_goes_to_telegram_as_message_thread_id() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, telegram) = fake_server().await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut post = blocks_post();
    post["options"] = json!({"message_thread_id":7});
    let (status, _) = http_request(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":post,"chat_id":"-1002627012139"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent = telegram.last("sendRichMessage").ok_or("send")?;
    assert_eq!(sent["body"]["message_thread_id"], 7);
    assert_eq!(sent["body"]["chat_id"], -1002627012139i64);
    Ok(())
}

#[tokio::test]
async fn deleted_draft_needs_an_explicit_channel() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, telegram) = fake_server().await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (_, draft) = http_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":blocks_post(),"chat_ids":["@chan_a"]})),
        &[],
    )
    .await;
    let id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let (status, _) = http_request(
        app.clone(),
        "DELETE",
        &format!("/api/drafts/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"draft_id":id})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&error).contains("chat_id"));
    let (status, _) = http_request(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"draft_id":id,"chat_id":"@chan_b"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?["body"]["chat_id"],
        "@chan_b"
    );
    Ok(())
}

#[tokio::test]
async fn topic_needs_one_forum_chat() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, telegram) = fake_server().await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut post = blocks_post();
    post["options"] = json!({"message_thread_id":7});
    let (status, error) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post,"chat_ids":["@a","@b"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&error).contains("несколько"));
    telegram.queue(
        "getChat",
        vec![json!({"ok":true,"result":{"id":-100555,"type":"channel","title":"Канал"}})],
    );
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/channels",
        Some(json!({"chat_id":"-100555"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post,"chat_id":"-100555","dry_run":true})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&error).contains("не форум"));
    let (status, error) = http_request(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":post,"chat_id":"-100555","dry_run":true,"at":"2099-01-01T09:00:00+03:00"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&error).contains("не форум"));
    assert!(telegram.calls_to("sendRichMessage").is_empty());
    Ok(())
}
