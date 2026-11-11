use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::services::scheduler;
use rich_studio::store::posts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn partial_delivery_continues_and_total_failure_is_not_http_success()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage"
            if payload["chat_id"]
                .as_str()
                .is_some_and(|chat| chat.starts_with("@bad")) =>
        {
            refused("chat not found")
        }
        "sendRichMessage" => ok(message(payload, 12)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, result) = request(
        app.clone(),
        "/api/send",
        json!({"post":post(),"chat_ids":["@bad","@good"]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["results"][0]["ok"], false);
    assert_eq!(result["results"][1]["ok"], true);
    let (status, error) = request(
        app,
        "/api/send",
        json!({"post":post(),"chat_ids":["@bad","@bad2"]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(error.to_string().contains("ни в один канал"));
    Ok(())
}

#[tokio::test]
async fn confirmed_broadcast_continues_when_cleanup_fails_and_keeps_recovery_history()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, payload| ok(message(payload, 12))).await;
    let state = AppState::new(settings(&dir, &base));
    let mut db = rich_studio::store::Store::connect(&state.account)?;
    rich_studio::store::db::migrate(&mut db)?;
    for (chat, status) in [("@one", "failed"), ("@two", "sending")] {
        posts::add(
            &state.account,
            posts::NewPost {
                chat_id: &json!(chat),
                payload: &post(),
                draft_id: Some("draft"),
                status,
                message_id: None,
                scheduled_at: None,
                error: None,
                kind: "rich",
                actor: "api",
            },
        )?;
    }
    rich_studio::store::scheduler::recover(&state.account, &[])?;
    db.execute_batch(
        "create trigger refuse_cleanup before delete on posts when old.chat_id='@one'
        begin select raise(abort, 'cleanup unavailable'); end;",
    )?;
    let (status, response) = request(
        build_router(state.clone()),
        "/api/send",
        json!({"post":post(),"draft_id":"draft","chat_ids":["@one","@two"]}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 2);
    assert!(response["warnings"].to_string().contains("@one"));
    assert!(
        response["warnings"]
            .to_string()
            .contains("cleanup unavailable")
    );
    assert!(
        posts::list(&state.account)?
            .iter()
            .any(|row| row["chat_id"] == "@two" && row["status"] == "failed")
    );
    Ok(())
}

#[tokio::test]
async fn response_keeps_confirmation_when_post_or_channel_cannot_be_read()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, payload| ok(message(payload, 12))).await;
    let state = AppState::new(settings(&dir, &base));
    let mut db = rich_studio::store::Store::connect(&state.account)?;
    rich_studio::store::db::migrate(&mut db)?;
    db.execute_batch(
        "create trigger corrupt_metadata after update of chat_num on posts begin
         update posts set payload='invalid JSON' where id=new.id;
         insert into channels(chat_id,created_at,topics) values(new.chat_id,0,'invalid JSON');
         end;",
    )?;
    let (status, response) = request(
        build_router(state),
        "/api/send",
        json!({"post":post(),"chat_id":"@one"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["post"]["status"], "sent");
    assert_eq!(response["post"]["message_id"], 12);
    assert_eq!(
        response["results"][0]["chat_title"],
        serde_json::Value::Null
    );
    let warnings = response["warnings"].to_string();
    assert!(
        warnings.contains("перечитать отправленный пост"),
        "{warnings}"
    );
    assert!(warnings.contains("название канала"), "{warnings}");
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn post_is_sent_before_pin_comment_and_poll_after_warnings()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 13)),
        "sendPoll" => refused("poll refused"),
        "pinChatMessage" => refused("pin refused"),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut content = post();
    content["pin"] = json!(true);
    content["comment"] = json!({"markdown":"comment"});
    content["poll_after"] = json!({"question":"Выбор","options":["да","нет"]});
    let (status, result) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["post"]["status"], "sent");
    assert_eq!(result["post"]["comment_status"], "skipped");
    assert!(result["warnings"].to_string().contains("pin refused"));
    assert!(result["warnings"].to_string().contains("poll refused"));
    let rows = posts::list(&context(&dir))?;
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "poll" && row["status"] == "failed")
    );
    Ok(())
}

#[tokio::test]
async fn dry_run_creates_nothing_and_preview_never_uses_the_default_channel()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 14)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, report) = request(
        app.clone(),
        "/api/send",
        json!({"post":post(),"chat_ids":["@a","-100"],"dry_run":true}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["requests"][1]["params"]["chat_id"], -100);
    let (status, _) = request(app, "/api/preview-send", json!({"post":post()}), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(requests.try_recv().is_err());
    assert!(posts::list(&context(&dir))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn actor_is_sanitized_and_survives_scheduling() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    for headers in [
        &[("x-studio-client", "mcp:claude-code")][..],
        &[][..],
        &[("x-studio-client", "<script>")][..],
    ] {
        let (status, _) = request(app.clone(), "/api/send", json!({"post":post()}), headers).await;
        assert_eq!(status, StatusCode::OK);
    }
    let mut actors = posts::list(&context(&dir))?
        .into_iter()
        .map(|row| row["actor"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    actors.sort();
    assert_eq!(actors, ["api", "api", "mcp:claude-code"]);

    let (status, record) = request(
        app,
        "/api/schedule",
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"}),
        &[("x-studio-client", "ui")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("scheduled id")?.to_owned();
    set_scheduled_at(&dir, &id, scheduler::now() - 5);
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            scheduler::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = posts::get(&context(&dir), &id)?.ok_or("scheduled post")?;
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["actor"], "ui");
    Ok(())
}

#[tokio::test]
async fn dry_run_reports_requests_after_send_and_validates_uploads_and_schedule()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 14)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let button = json!({"inline_keyboard":[[{"text":"a","url":"https://t.me"}]]});
    let mut content = post();
    content["pin"] = json!(true);
    content["reply_markup"] = button;
    let (status, report) = request(
        app.clone(),
        "/api/send",
        json!({"post":content,"chat_ids":["@a","-100"],"dry_run":true}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["after_send"], json!(["pinChatMessage"]));
    assert_eq!(report["requests"][0]["params"]["chat_id"], json!("@a"));
    assert_eq!(report["requests"][1]["params"]["chat_id"], json!(-100));
    assert_eq!(report["requests"][0]["method"], "sendRichMessage");
    assert_eq!(
        report["requests"][0]["params"]["rich_message"],
        content["rich_message"]
    );

    let bad = json!({"post":{"rich_message":{"blocks":[{"type":"heading","text":"x","size":"big"}]}},"dry_run":true});
    assert_eq!(
        request(app.clone(), "/api/send", bad, &[]).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let missing = json!({"post":{"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":"upload:nope.jpg"}}]}},"dry_run":true});
    let (status, error) = request(app.clone(), "/api/send", missing, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("nope.jpg"), "{error}");

    let (status, report) = schedule(
        app,
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00","dry_run":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["at"], 4070930400i64);
    assert!(requests.try_recv().is_err());
    assert!(posts::list(&context(&dir))?.is_empty());
    Ok(())
}
