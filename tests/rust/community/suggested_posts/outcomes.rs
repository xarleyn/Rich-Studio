use axum::http::StatusCode;
use rich_studio::{
    AppState,
    account_context::AccountContext,
    build_router,
    services::{suggested_posts, updates},
    store::suggested_posts as suggestions,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tempfile::TempDir;

use crate::support::{
    direct_messages, dm_service, dm_suggestion, mock, request, request_with, services, settings,
    suggestion,
};

#[tokio::test]
async fn uncertain_decision_waits_for_monotonic_outcome_refusal_rolls_back_safely()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (_, service) = services(&settings, context.clone());
    let id = suggestion(&context, 1)?;
    mock.mode.store(2, Ordering::SeqCst);
    assert!(matches!(
        service.approve(&id, &Value::Null, "api").await,
        Err(rich_studio::errors::ServiceError::MayHaveGone(_))
    ));
    assert!(service.approve(&id, &Value::Null, "retry").await.is_err());
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    assert!(suggested_posts::on_service(&context,&json!({"chat":{"id":-100500},"suggested_post_paid":{"suggested_post_message":{"message_id":1}}})).await?);
    suggested_posts::on_message(&context,&json!({"chat":{"id":-100500},"message_id":1,"text":"edited","suggested_post_info":{"state":"pending"}})).await?;
    assert_eq!(
        suggestions::get(&context, &id)?.ok_or("suggestion")?["state"],
        "paid"
    );
    assert!(service.reopen(&id, "admin").await.is_err());
    let refused = suggestion(&context, 2)?;
    mock.mode.store(1, Ordering::SeqCst);
    assert!(
        service
            .decline(&refused, &json!("No"), "api")
            .await
            .is_err()
    );
    assert_eq!(
        suggestions::get(&context, &refused)?.ok_or("suggestion")?["state"],
        "pending"
    );
    mock.mode.store(4, Ordering::SeqCst);
    let copy = refused.clone();
    let service_copy = service.clone();
    let pending =
        tokio::spawn(async move { service_copy.approve(&copy, &Value::Null, "api").await });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    suggested_posts::on_service(&context,&json!({"chat":{"id":-100500},"suggested_post_refunded":{"suggested_post_message":{"message_id":2}}})).await?;
    mock.release.notify_one();
    assert!(pending.await?.is_err());
    assert_eq!(
        suggestions::get(&context, &refused)?.ok_or("suggestion")?["state"],
        "refunded"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn outcome_of_a_suggestion_the_studio_did_not_see() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, _mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let original = json!({
        "message_id":30,
        "date":1,
        "chat":direct_messages(),
        "text":"Старый пост",
        "from":{"id":6,"first_name":"Боб"}
    });
    assert_eq!(
        updates::handle_update(
            &state,
            &dm_service(
                31,
                "suggested_post_declined",
                json!({"suggested_post_message":original})
            )
        )
        .await?,
        Some("suggested_post_outcome")
    );
    let declined = request(
        app,
        "GET",
        "/api/suggested-posts?state=declined",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(declined[0]["text"], "Старый пост");
    // an ordinary message in the same chat is not a suggestion
    let plain =
        json!({"message":{"message_id":32,"date":1,"chat":direct_messages(),"text":"привет"}});
    assert_eq!(updates::handle_update(&state, &plain).await?, None);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_outcome_that_comes_while_approving_is_not_overwritten()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    updates::handle_update(
        &state,
        &dm_suggestion(
            60,
            json!({"state":"pending","price":{"currency":"XTR","amount":5}}),
        ),
    )
    .await?;
    mock.outcome_during
        .lock()
        .unwrap_or_else(|e| panic!("lock {e}"))
        .replace((
            "approveSuggestedPost".to_owned(),
            state.clone(),
            dm_service(
                61,
                "suggested_post_paid",
                json!({
                    "suggested_post_message":{"message_id":60,"date":1},
                    "currency":"XTR",
                    "star_amount":{"amount":5}
                }),
            ),
        ));
    let approved = request_with(
        app,
        "POST",
        "/api/suggested-posts/-1009:60/approve",
        json!({}),
        "owner-secret",
        false,
        None,
        Some("ui"),
    )
    .await;
    assert_eq!(approved.0, StatusCode::OK, "{}", approved.1);
    assert_eq!(approved.1["state"], "paid");
    assert_eq!(approved.1["actor"], "ui");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reopened_suggestion_takes_the_late_outcome() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    updates::handle_update(&state, &dm_suggestion(80, json!({"state":"pending"}))).await?;
    assert!(suggestions::claim(
        &state.account,
        "-1009:80",
        &["pending"],
        "declining"
    )?);
    let reopened = request_with(
        app,
        "POST",
        "/api/suggested-posts/-1009:80/reopen",
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
    assert_eq!(
        updates::handle_update(
            &state,
            &dm_service(
                81,
                "suggested_post_declined",
                json!({"suggested_post_message":{"message_id":80,"date":1}})
            )
        )
        .await?,
        Some("suggested_post_outcome")
    );
    assert_eq!(
        suggestions::get(&state.account, "-1009:80")?.ok_or("suggestion")?["state"],
        "declined"
    );
    server.abort();
    Ok(())
}
