use axum::http::StatusCode;
use rich_studio::{
    AppState,
    account_context::AccountContext,
    build_router,
    services::updates,
    store::{auth, suggested_posts as suggestions},
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::{
    dm_service, dm_suggestion, mock, request, request_with, services, settings, suggestion,
};

#[tokio::test]
async fn suggestion_claim_serializes_concurrent_decisions_and_cancellation_keeps_claim()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (_, service) = services(&settings, context.clone());
    let id = suggestion(&context, 1)?;
    mock.mode.store(3, Ordering::SeqCst);
    let first_service = service.clone();
    let first_id = id.clone();
    let first = tokio::spawn(async move {
        first_service
            .approve(&first_id, &Value::Null, "first")
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    assert!(
        service
            .decline(&id, &json!("second"), "second")
            .await
            .is_err()
    );
    assert_eq!(mock.count("declineSuggestedPost"), 0);
    mock.release.notify_one();
    assert_eq!(first.await??["state"], "approved");
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    let cancel_id = suggestion(&context, 2)?;
    let copy = cancel_id.clone();
    let service_copy = service.clone();
    let canceled =
        tokio::spawn(async move { service_copy.approve(&copy, &Value::Null, "canceled").await });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    canceled.abort();
    let _ = canceled.await;
    mock.release.notify_one();
    assert_eq!(
        suggestions::get(&context, &cancel_id)?.ok_or("suggestion")?["state"],
        "approving"
    );
    assert!(
        service
            .approve(&cancel_id, &Value::Null, "retry")
            .await
            .is_err()
    );
    assert_eq!(mock.count("approveSuggestedPost"), 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_suggested_post_decision_goes_to_telegram_once() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    updates::handle_update(&state, &dm_suggestion(30, json!({"state":"pending"}))).await?;
    // Telegram refused: nothing changed, the suggestion waits again
    mock.mode.store(1, Ordering::SeqCst);
    let refused = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:30/approve",
        json!({}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY);
    let all = request(
        app.clone(),
        "GET",
        "/api/suggested-posts",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(all[0]["state"], "pending");
    mock.mode.store(0, Ordering::SeqCst);
    mock.calls
        .lock()
        .unwrap_or_else(|e| panic!("lock {e}"))
        .clear();
    // no answer: the decision may have gone, so it waits for Telegram's outcome
    mock.mode.store(2, Ordering::SeqCst);
    let uncertain = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:30/approve",
        json!({}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(uncertain.0, StatusCode::BAD_GATEWAY);
    assert!(
        uncertain.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("могло дойти")
    );
    let open = request(
        app.clone(),
        "GET",
        "/api/suggested-posts?state=open",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(open[0]["state"], "approving");
    // no repeat after "may have gone": not a retry, not a decline
    for path in ["approve", "decline"] {
        let again = request(
            app.clone(),
            "POST",
            &format!("/api/suggested-posts/-1009:30/{path}"),
            json!({}),
            "owner-secret",
            false,
            None,
        )
        .await;
        assert_eq!(again.0, StatusCode::BAD_REQUEST);
        assert!(
            again.1["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("уже отправлено")
        );
    }
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    // the outcome settles it
    assert_eq!(
        updates::handle_update(
            &state,
            &dm_service(
                31,
                "suggested_post_approved",
                json!({"suggested_post_message":{"message_id":30,"date":1}})
            )
        )
        .await?,
        Some("suggested_post_outcome")
    );
    let all = request(
        app.clone(),
        "GET",
        "/api/suggested-posts",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(all[0]["state"], "approved");
    // two clicks at once: only one takes the suggestion
    updates::handle_update(&state, &dm_suggestion(40, json!({"state":"pending"}))).await?;
    let id = "-1009:40";
    assert!(suggestions::claim(
        &state.account,
        id,
        &["pending", "failed"],
        "approving"
    )?);
    assert!(!suggestions::claim(
        &state.account,
        id,
        &["pending", "failed"],
        "approving"
    )?);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn failed_approval_can_be_tried_again_and_late_copies_do_not_go_back()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    updates::handle_update(
        &state,
        &dm_suggestion(
            50,
            json!({"state":"pending","price":{"currency":"XTR","amount":5}}),
        ),
    )
    .await?;
    updates::handle_update(
        &state,
        &dm_service(
            51,
            "suggested_post_approval_failed",
            json!({
                "suggested_post_message":{"message_id":50,"date":1},
                "price":{"currency":"XTR","amount":5}
            }),
        ),
    )
    .await?;
    let open = request(
        app.clone(),
        "GET",
        "/api/suggested-posts?state=open",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(open[0]["state"], "failed");
    let approved = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:50/approve",
        json!({}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(approved.0, StatusCode::OK, "{}", approved.1);
    assert_eq!(approved.1["state"], "approved");
    updates::handle_update(
        &state,
        &dm_service(
            52,
            "suggested_post_paid",
            json!({
                "suggested_post_message":{"message_id":50,"date":1},
                "currency":"XTR",
                "star_amount":{"amount":5}
            }),
        ),
    )
    .await?;
    // an edit that still says approved comes late: the payment stays recorded
    let late = dm_suggestion(50, json!({"state":"approved"}));
    updates::handle_update(&state, &json!({"edited_message":late["message"]})).await?;
    let all = request(
        app,
        "GET",
        "/api/suggested-posts",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(all[0]["state"], "paid");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_suggested_post_decision_without_an_answer_can_be_made_again()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "agent", "agent-secret", "publish")?;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    updates::handle_update(&state, &dm_suggestion(70, json!({"state":"pending"}))).await?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/suggested-posts/-1009:70/reopen",
            Value::Null,
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    mock.mode.store(2, Ordering::SeqCst);
    let key = Some("approve-70");
    let uncertain = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:70/approve",
        json!({}),
        "owner-secret",
        false,
        key,
    )
    .await;
    assert_eq!(uncertain.0, StatusCode::BAD_GATEWAY);
    assert!(
        uncertain.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("могло дойти")
    );
    mock.mode.store(0, Ordering::SeqCst);
    let sent_before = mock.count("approveSuggestedPost");
    let reopened = request_with(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:70/reopen",
        Value::Null,
        "owner-secret",
        false,
        None,
        Some("ui"),
    )
    .await;
    assert_eq!(reopened.0, StatusCode::OK, "{}", reopened.1);
    assert_eq!(reopened.1["state"], "pending");
    assert_eq!(reopened.1["actor"], "ui");
    // the same key is still the decision that got no answer: nothing goes to Telegram
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/suggested-posts/-1009:70/approve",
            json!({}),
            "owner-secret",
            false,
            key
        )
        .await
        .0,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(mock.count("approveSuggestedPost"), sent_before);
    let approved = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:70/approve",
        json!({}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(approved.0, StatusCode::OK, "{}", approved.1);
    assert_eq!(approved.1["state"], "approved");
    assert_eq!(mock.last("approveSuggestedPost")["message_id"], 70);
    // decided now: no reopening
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/suggested-posts/-1009:70/reopen",
            Value::Null,
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // an agent cannot decide again by itself: the person checks the channel's direct messages first
    suggestions::claim(&state.account, "-1009:70", &["approved"], "approving")?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/suggested-posts/-1009:70/reopen",
            Value::Null,
            "agent-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let tools = request(
        app,
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    let tools = tools["result"]["tools"].as_array().ok_or("tools")?;
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"].as_str().unwrap_or_default().contains("reopen"))
    );
    server.abort();
    Ok(())
}
