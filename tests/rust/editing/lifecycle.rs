use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::services::scheduler;
use rich_studio::store::posts;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, http, last_call, now, post, served};

#[tokio::test]
async fn reschedule_moves_a_waiting_post() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?;
    let at = now() + 7200;
    let (status, moved) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":at})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["scheduled_at"], json!(at));
    assert_eq!(moved["status"], "scheduled");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn missed_post_can_be_rescheduled_or_cancelled() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let mut records = Vec::new();
    for _ in 0..2 {
        let (status, record) = http(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"post":blocks_post(),"at":now() + 3600})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{record}");
        records.push(record);
    }
    let connection = rich_studio::store::Store::connect(&state.account)?;
    for record in &records {
        connection.execute(
            "update posts set status='missed', error=?, retry_at=1 where id=?",
            rusqlite::params![
                "студия не работала",
                record["id"].as_str().ok_or("post id")?
            ],
        )?;
    }
    let first = records[0]["id"].as_str().ok_or("post id")?;
    let second = records[1]["id"].as_str().ok_or("post id")?;
    let (status, moved) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{first}/reschedule"),
        Some(json!({"at":"2099-01-01T09:00:00+03:00"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["status"], "scheduled");
    assert!(moved["error"].is_null());
    assert!(moved["retry_at"].is_null());
    let (status, cancelled) = http(app, "DELETE", &format!("/api/posts/{second}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn sent_post_cannot_be_rescheduled_and_past_time_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() + 60})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("sent")
    );
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?;
    let (status, refused) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() - 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("прошло")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduling_to_several_channels_is_all_or_nothing() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    posts::list(&state.account)?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_second_post before insert on posts
         when (select count(*) from posts) >= 1
         begin select raise(abort, 'disk full'); end;",
    )?;
    let (status, failed) = http(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600,"chat_ids":["@a","@b"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{failed}");
    assert!(posts::list(&state.account)?.is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn missed_post_is_edited_before_it_is_scheduled_again() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let old = post("старый");
    let new = post("новый");
    let record = posts::add_scheduled(
        &state.account,
        &[json!("@chan")],
        &old,
        now() - 7200,
        None,
        "rich",
        "api",
    )?[0]
        .clone();
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set status='missed', error=? where id=?",
        rusqlite::params!["студия не работала", id],
    )?;
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["status"], "missed");
    assert_eq!(changed["version"], 1);
    let (status, versions) = http(
        app.clone(),
        "GET",
        &format!("/api/posts/{id}/versions"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions[0]["payload"]["rich_message"], old["rich_message"]);
    let (status, rescheduled) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() + 60})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rescheduled}");
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set scheduled_at=? where id=?",
        rusqlite::params![now() - 1, id],
    )?;
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = last_call(&mock, "sendRichMessage").ok_or("sendRichMessage")?;
    assert_eq!(sent["rich_message"], new["rich_message"]);
    server.abort();
    Ok(())
}
