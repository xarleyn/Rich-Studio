use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::services::scheduler;
use rich_studio::store::{posts, scheduler as scheduler_store};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn far_past_post_is_missed_unless_the_late_limit_is_off()
-> Result<(), Box<dyn std::error::Error>> {
    for (late_limit, expected) in [(60.0, "missed"), (0.0, "sent")] {
        let dir = TempDir::new()?;
        let (base, mut requests) = mock(|_, method, payload| match method {
            "sendRichMessage" => ok(message(payload, 10)),
            _ => ok(json!(true)),
        })
        .await;
        let mut config = settings(&dir, &base);
        config.schedule_late_min = late_limit;
        let state = AppState::new(config);
        let app = build_router(state.clone());
        let (status, record) =
            schedule(app, json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"})).await;
        assert_eq!(status, StatusCode::OK);
        let id = record["id"].as_str().ok_or("post id")?;
        let now = scheduler::now();
        set_scheduled_at(&dir, id, now - 3 * 3600);
        assert_eq!(
            scheduler::run_due(
                &state,
                state.account.clone(),
                now,
                Arc::new(AtomicBool::new(false)),
            )
            .await?,
            1
        );
        let row = posts::get(&context(&dir), id)?.ok_or("post")?;
        assert_eq!(row["status"], expected);
        if expected == "missed" {
            assert!(
                row["error"].as_str().unwrap_or_default().contains("3 ч"),
                "{row}"
            );
            assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
        } else {
            assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn due_posts_are_sent_in_queue_order_with_increasing_message_ids()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|count, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 100 + count as i64)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let now = scheduler::now();
    let mut ids = Vec::new();
    for (markdown, at) in [("второй", now - 60), ("первый", now - 120)] {
        let (status, record) = schedule(
            app.clone(),
            json!({"post":{"rich_message":{"markdown":markdown}},"at":"2099-01-01T09:00:00+03:00"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = record["id"].as_str().ok_or("post id")?.to_owned();
        set_scheduled_at(&dir, &id, at);
        ids.push(id);
    }
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        2
    );
    let sent = sent_payloads(&mut requests, "sendRichMessage");
    assert_eq!(
        sent.iter()
            .map(|payload| payload["rich_message"]["markdown"].clone())
            .collect::<Vec<_>>(),
        [json!("первый"), json!("второй")]
    );
    let first = posts::get(&context(&dir), &ids[1])?.ok_or("first post")?;
    let second = posts::get(&context(&dir), &ids[0])?.ok_or("second post")?;
    assert!(
        first["message_id"].as_i64().unwrap_or_default()
            < second["message_id"].as_i64().unwrap_or_default()
    );
    Ok(())
}

#[tokio::test]
async fn scheduler_sends_the_content_as_it_is_now() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let at = 4070930400_i64;
    let (status, record) = schedule(app.clone(), json!({"post":post(),"at":at})).await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let stale = scheduler_store::due(&context(&dir), at)?;
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["payload"]["rich_message"], post()["rich_message"]);
    let (status, changed) = request(
        app,
        &format!("/api/posts/{id}/edit"),
        json!({"rich_message":{"markdown":"свежий текст"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            at,
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = sent_payloads(&mut requests, "sendRichMessage");
    assert_eq!(sent[0]["rich_message"], json!({"markdown":"свежий текст"}));
    Ok(())
}
