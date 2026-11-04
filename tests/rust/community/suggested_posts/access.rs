use axum::http::StatusCode;
use rich_studio::{
    AppState, build_router,
    services::updates,
    store::{auth, suggested_posts as suggestions},
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{
    dm_service, dm_suggestion, dm_suggestion_text, mcp_call, mcp_list, mock, request, request_with,
    settings, suggestion,
};

#[tokio::test]
async fn guarded_http_and_mcp_enforce_roles_and_idempotency_for_suggestion_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "reader", "reader-secret", "read")?;
    auth::add_token(&config, "publisher", "publish-secret", "publish")?;
    let state = AppState::new(config.clone());
    let id = suggestion(&state.account, 5)?;
    let path = format!("/api/suggested-posts/{id}/approve");
    let app = build_router(state.clone());
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "reader-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"approve_suggested_post","arguments":{"suggestion_id":id}}});
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/mcp",
            body.clone(),
            "reader-secret",
            false,
            None
        )
        .await
        .1["result"]["isError"],
        true
    );
    assert_eq!(mock.count("approveSuggestedPost"), 0);
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "publish-secret",
            false,
            Some("decision-one")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "publish-secret",
            false,
            Some("decision-one")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    assert_eq!(
        suggestions::get(&state.account, &id)?.ok_or("suggestion")?["actor"],
        "api@publisher"
    );
    let uncertain = suggestion(&state.account, 6)?;
    suggestions::claim(&state.account, &uncertain, &["pending"], "approving")?;
    let reopen = format!("/api/suggested-posts/{uncertain}/reopen");
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &reopen,
            json!({}),
            "publish-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(app, "POST", &reopen, json!({}), "owner-secret", false, None)
            .await
            .0,
        StatusCode::OK
    );
    let mut off = config;
    off.mcp_publish = false;
    assert_eq!(
        request(
            build_router(AppState::new(off)),
            "POST",
            "/mcp",
            body,
            "owner-secret",
            false,
            None
        )
        .await
        .1["result"]["isError"],
        true
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn suggested_post_is_listed_approved_and_survives_a_replayed_update()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let paid = json!({"state":"pending","price":{"currency":"XTR","amount":100}});
    assert_eq!(
        updates::handle_update(&state, &dm_suggestion(10, paid.clone())).await?,
        Some("suggested_post")
    );
    let listed = request(
        app.clone(),
        "GET",
        "/api/suggested-posts?state=pending",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    let listed = listed.as_array().ok_or("listing")?;
    assert_eq!(listed.len(), 1);
    let s = &listed[0];
    assert_eq!(s["id"], "-1009:10");
    assert_eq!(s["text"], "Мой пост для канала");
    assert_eq!(s["price_amount"], 100);
    assert_eq!(s["author"], "Аня (@anya)");
    assert_eq!(s["channel_title"], "Тест");
    assert_eq!(s["topic_id"], 77);
    // the author edits it: the same row, new text
    let edited = dm_suggestion_text(10, paid.clone(), "Исправленный пост");
    assert_eq!(
        updates::handle_update(&state, &json!({"edited_message":edited["message"]})).await?,
        Some("suggested_post")
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
    assert_eq!(all[0]["text"], "Исправленный пост");
    let far = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:10/approve",
        json!({"send_date":auth::now() + 2_700_000}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(far.0, StatusCode::BAD_REQUEST);
    assert!(
        far.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("30 дней")
    );
    let at = auth::now() + 3600;
    let approved = request_with(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:10/approve",
        json!({"send_date":at}),
        "owner-secret",
        false,
        None,
        Some("ui"),
    )
    .await;
    assert_eq!(approved.0, StatusCode::OK, "{}", approved.1);
    assert_eq!(approved.1["state"], "approved");
    assert_eq!(approved.1["actor"], "ui");
    assert_eq!(
        mock.last("approveSuggestedPost"),
        json!({"chat_id":-1009,"message_id":10,"send_date":at})
    );
    // the same update delivered again (a restart before the offset was saved) does not undo the decision
    updates::handle_update(&state, &dm_suggestion(10, paid)).await?;
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
    let again = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:10/approve",
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
            .contains("решение")
    );
    // Telegram reports the payment afterwards
    assert_eq!(
        updates::handle_update(
            &state,
            &dm_service(
                11,
                "suggested_post_paid",
                json!({
                    "suggested_post_message":{"message_id":10,"date":1},
                    "currency":"XTR",
                    "star_amount":{"amount":85}
                })
            )
        )
        .await?,
        Some("suggested_post_outcome")
    );
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
async fn suggested_posts_need_publish_and_work_through_mcp()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "d", "draft-secret", "draft")?;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    updates::handle_update(&state, &dm_suggestion(40, json!({"state":"pending"}))).await?;
    let listed = mcp_list(
        &mcp_call(
            app.clone(),
            "list_suggested_posts",
            json!({}),
            "owner-secret",
        )
        .await,
    );
    let ids = listed
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["-1009:40"]);
    let approved = mcp_call(
        app.clone(),
        "approve_suggested_post",
        json!({"suggestion_id":"-1009:40"}),
        "owner-secret",
    )
    .await;
    assert_eq!(approved["isError"], false);
    assert_eq!(approved["structuredContent"]["state"], "approved");
    assert!(mock.last("approveSuggestedPost").get("send_date").is_none());
    assert_eq!(
        request(
            app,
            "POST",
            "/api/suggested-posts/-1009:40/decline",
            json!({}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    server.abort();
    Ok(())
}
