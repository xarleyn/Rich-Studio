use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use rich_studio::AppState;
use rich_studio::build_router;
use rich_studio::services::updates;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, settings, state, telegram};

#[tokio::test]
async fn first_comment_replies_to_the_forward_once() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let mut post = blocks_post();
    post["comment"] = json!({"html":"<p>Ссылки из эфира</p>"});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await
    .value;
    assert_eq!(reply["post"]["comment_status"], "waiting");
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    let forward = json!({
        "update_id":3,
        "message":{
            "message_id":77,
            "chat":{"id":-2002,"type":"supergroup"},
            "is_automatic_forward":true,
            "forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":message_id}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &forward).await?,
        Some("comment")
    );
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("comment")?.body,
        json!({
            "chat_id":-2002,
            "rich_message":{"html":"<p>Ссылки из эфира</p>"},
            "reply_parameters":{"message_id":77}
        })
    );
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[],)
            .await
            .value["comment_status"],
        "sent"
    );
    let calls = telegram.calls().len();
    updates::handle_update(&state, &forward).await?;
    assert_eq!(telegram.calls().len(), calls);
    Ok(())
}

#[tokio::test]
async fn invalid_first_comment_is_refused_before_send() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let mut post = blocks_post();
    post["comment"] = json!({"html":"a","markdown":"b"});
    let reply = call(app, "POST", "/api/send", Some(json!({"post":post})), &[]).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn reaction_counts_store_emoji_custom_and_paid() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let mut config = settings(&dir, &base, &[]);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let message_id = reply["message"]["message_id"].as_i64().ok_or("message")?;
    updates::handle_update(
        &state,
        &json!({
            "update_id":4,
            "message_reaction_count":{
                "chat":{"id":-1001},
                "message_id":message_id,
                "date":0,
                "reactions":[
                    {"type":{"type":"emoji","emoji":"👍"},"total_count":5},
                    {"type":{"type":"custom_emoji","custom_emoji_id":"123"},"total_count":1},
                    {"type":{"type":"paid"},"total_count":2}
                ]
            }
        }),
    )
    .await?;
    let id = reply["post"]["id"].as_str().ok_or("id")?;
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["reactions"],
        json!({"👍":5,"custom:123":1,"paid":2})
    );
    Ok(())
}

#[tokio::test]
async fn updates_status_is_off_by_default() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let status = call(app, "GET", "/api/updates", None, &[]).await.value;
    assert_eq!(status["enabled"], false);
    assert_eq!(status["state"], "off");
    Ok(())
}

#[tokio::test]
async fn updates_offset_survives_restart() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[("TG_UPDATES", "on")]);
    rich_studio::store::meta::set(&state.account, updates::OFFSET_KEY, "500")?;
    updates::poll_once(&state).await?;
    let first = telegram
        .calls_to("getUpdates")
        .first()
        .cloned()
        .ok_or("getUpdates")?;
    assert_eq!(first.body["offset"], 500);
    Ok(())
}

#[tokio::test]
async fn updates_loop_handles_updates_and_reports_conflict() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[("TG_UPDATES", "on")]);
    telegram.queue(
        "getUpdates",
        vec![
            json!({"ok":true,"result":[
                {"update_id":10,"poll":{"id":"nope"}},
                {"update_id":11,"callback_query":{"id":"c","data":"d"}}
            ]}),
            json!({"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}),
        ],
    );
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(updates::run(state.clone(), stopping.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if state.updates_status.read().await.state == "conflict" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    stopping.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(30), task).await???;
    let calls = telegram.calls_to("getUpdates");
    assert!(calls.len() >= 2, "{calls:?}");
    assert!(
        calls[0].body["allowed_updates"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|value| value == "message_reaction_count"),
        "{}",
        calls[0].body
    );
    assert_eq!(calls[1].body["offset"], 12);
    assert_eq!(state.updates_status.read().await.state, "conflict");
    assert_eq!(state.updates_status.read().await.handled, 2);
    Ok(())
}
