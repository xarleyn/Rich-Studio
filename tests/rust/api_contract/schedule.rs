use axum::http::StatusCode;
use rich_studio::build_router;
use rich_studio::services::scheduler;
use rich_studio::store::db::Store;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, merged, run_due, set_scheduled_at, state, telegram,
};

#[tokio::test]
async fn schedule_time_and_state_transitions_are_guarded() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"post":blocks_post(),"at":scheduler::now() - 3600})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    for at in ["2030-01-01T09:00:00", "завтра"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/schedule",
                Some(json!({"post":blocks_post(),"at":at})),
                &[],
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{at}"
        );
    }
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":"2030-01-01T09:00:00+03:00"})),
        &[],
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    assert_eq!(scheduled.value["scheduled_at"], 1893477600i64);
    let id = scheduled.value["id"].as_str().ok_or("id")?.to_owned();
    let cancelled = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(cancelled.value["status"], "cancelled");
    assert_eq!(telegram.count("deleteMessage"), 0);

    let future = call(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":scheduler::now() + 3600})),
        &[],
    )
    .await
    .value;
    let id = future["id"].as_str().ok_or("id")?.to_owned();
    Store::connect(&state.account)?
        .execute("update posts set status='deleted' where id=?", [&id])?;
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/posts/{id}"),
            None,
            &[]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["status"],
        "deleted"
    );
    Store::connect(&state.account)?.execute(
        "update posts set status='failed', error='x' where id=?",
        [&id],
    )?;
    assert_eq!(
        call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/edit"),
            Some(blocks_post()),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let removed = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(removed.value["status"], "removed");
    assert_eq!(
        call(app, "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn schedule_to_several_channels_reports_each_one() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b"],"at":scheduler::now() + 999})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let all = reply.value["all"].as_array().ok_or("all")?;
    assert_eq!(
        all.iter()
            .map(|post| post["chat_id"].clone())
            .collect::<Vec<_>>(),
        [json!("@a"), json!("@b")]
    );
    Ok(())
}

#[tokio::test]
async fn scheduled_post_sends_its_poll_after() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let at = scheduler::now() + 600;
    let scheduled = call(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll})),"at":at})),
        &[],
    )
    .await
    .value;
    let id = scheduled["id"].as_str().ok_or("id")?.to_owned();
    set_scheduled_at(&state, &id, scheduler::now() - 1);
    run_due(&state, scheduler::now()).await;
    let calls = telegram
        .calls()
        .into_iter()
        .filter(|call| ["sendRichMessage", "sendPoll"].contains(&call.method.as_str()))
        .map(|call| call.method)
        .collect::<Vec<_>>();
    assert_eq!(calls[calls.len() - 2..], ["sendRichMessage", "sendPoll"]);
    Ok(())
}

#[tokio::test]
async fn scheduled_poll_is_sent_by_the_scheduler() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/poll",
        Some(json!({"poll":{"question":"?","options":["a","b"]},"at":scheduler::now() + 999})),
        &[],
    )
    .await
    .value;
    let id = scheduled["scheduled"][0]["id"]
        .as_str()
        .ok_or("scheduled poll id")?
        .to_owned();
    set_scheduled_at(&state, &id, scheduler::now() - 1);
    run_due(&state, scheduler::now()).await;
    let stored = call(app, "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(stored["status"], "sent");
    assert!(stored["poll_id"].as_str().is_some());
    assert_eq!(
        telegram.last("sendPoll").ok_or("sendPoll")?.body["question"],
        "?"
    );
    Ok(())
}
