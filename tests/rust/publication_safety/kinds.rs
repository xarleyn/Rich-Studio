use axum::http::StatusCode;
use rich_studio::store::posts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn poll_media_and_schedule_use_their_registry_and_idempotency()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendPoll" | "sendLivePhoto" | "sendPaidMedia" => ok(message(payload, 15)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let poll = json!({"poll":{"question":"Да?","options":["да","нет"],"future":17}});
    for _ in 0..2 {
        let (status, _) = request(
            app.clone(),
            "/api/poll",
            poll.clone(),
            &[("idempotency-key", "poll")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let live = json!({"kind":"live_photo","params":{"live_photo":"upload:clip.mp4","photo":"upload:still.jpg","caption":"Live"}});
    let (status, _) = request(app.clone(), "/api/media-post", live, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let paid = json!({"kind":"paid_media","params":{"star_count":5,"media":[{"type":"photo","media":"upload:pic.png"}],"future":"keep"}});
    let (status, _) = request(app.clone(), "/api/media-post", paid, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let schedule = json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"});
    let (first_status, first) = request(
        app.clone(),
        "/api/schedule",
        schedule.clone(),
        &[("idempotency-key", "schedule")],
    )
    .await;
    let (second_status, second) = request(
        app,
        "/api/schedule",
        schedule,
        &[("idempotency-key", "schedule")],
    )
    .await;
    assert_eq!(
        (first_status, second_status),
        (StatusCode::OK, StatusCode::OK)
    );
    assert_eq!(first, second);
    assert_eq!(first["status"], "scheduled");
    assert_eq!(first["scheduled_at"], 4070930400i64);
    assert_eq!(sent_count(&mut requests, "sendPoll"), 1);
    assert_eq!(posts::list(&context(&dir))?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn publishing_mcp_tools_have_public_annotations_and_dry_run()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, list) = request(
        app.clone(),
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for name in [
        "publish_post",
        "send_poll",
        "schedule_post",
        "send_media_post",
    ] {
        let tool = list["result"]["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
            .cloned();
        assert_eq!(
            tool.as_ref()
                .and_then(|tool| tool["annotations"]["destructiveHint"].as_bool()),
            Some(true),
            "{name}"
        );
        assert_eq!(
            tool.as_ref()
                .and_then(|tool| tool["annotations"]["openWorldHint"].as_bool()),
            Some(true),
            "{name}"
        );
    }
    let (status, result) = request(app, "/mcp", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish_post","arguments":{"post":post(),"dry_run":true}}}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["structuredContent"]["dry_run"], true);
    assert!(requests.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn poll_buttons_are_sent_with_send_poll() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendPoll" => ok(message(payload, 15)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let at = 4070930400_i64;
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let poll = json!({"poll":{"question":"Как?","options":["Да","Нет"]},"chat_id":"@chan","at":at});
    let (status, created) = request(app.clone(), "/api/poll", poll, &[]).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["scheduled"][0]["id"]
        .as_str()
        .ok_or("scheduled poll id")?
        .to_owned();
    let buttons_path = format!("/api/posts/{id}/buttons");
    let (status, changed) =
        request(app, &buttons_path, json!({"reply_markup":keyboard}), &[]).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["poll"]["reply_markup"], keyboard);
    assert!(changed["payload"].get("reply_markup").is_none());

    let context = context(&dir);
    let connection = rich_studio::store::db::Store::connect(&context)?;
    connection.execute(
        "update posts set scheduled_at=?1 where id=?2",
        rusqlite::params![at - 1, id],
    )?;
    drop(connection);
    assert_eq!(
        rich_studio::services::scheduler::run_due(
            &state,
            state.account.clone(),
            at,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let mut sent = None;
    while let Ok((method, payload)) = requests.try_recv() {
        if method == "sendPoll" {
            sent = Some(payload);
        }
    }
    let sent = sent.ok_or("sendPoll request")?;
    assert_eq!(sent["reply_markup"], keyboard);
    Ok(())
}
