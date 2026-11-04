use axum::http::StatusCode;
use rich_studio::{
    AppState,
    account_context::AccountContext,
    build_router,
    services::{invite_links, updates},
    store::auth,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{mcp_call, mcp_list, member, mock, request, services, settings};

#[tokio::test]
async fn invites_preserve_omitted_limits_explicit_clear_counters_and_revocation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (links, _) = services(&settings, context.clone());
    let expires = auth::now() + 86400;
    let created = links
        .create(
            &json!({"chat_id":-100500,"name":" Campaign ","member_limit":5,"expire_date":expires}),
            "api@test",
        )
        .await?;
    assert_eq!(created["name"], "Campaign");
    assert_eq!(created["actor"], "api@test");
    let link = created["invite_link"].as_str().ok_or("link")?;
    assert!(invite_links::on_member(&context,&json!({"invite_link":{"invite_link":link},"old_chat_member":{"status":"left"},"new_chat_member":{"status":"restricted","is_member":true}})).await?);
    assert!(!invite_links::on_member(&context,&json!({"invite_link":{"invite_link":link},"old_chat_member":{"status":"member"},"new_chat_member":{"status":"administrator"}})).await?);
    invite_links::on_join_request(&context, &json!({"invite_link":{"invite_link":link}})).await?;
    links
        .edit(&json!({"invite_link":link,"name":"Renamed"}))
        .await?;
    let edit = mock
        .calls()
        .into_iter()
        .find(|(method, _)| method == "editChatInviteLink")
        .ok_or("edit")?
        .1;
    assert_eq!(edit["member_limit"], 5);
    assert_eq!(edit["expire_date"], expires);
    links.edit(&json!({"invite_link":link,"member_limit":0,"expire_date":0,"creates_join_request":false})).await?;
    let edit = mock
        .calls()
        .into_iter()
        .rev()
        .find(|(method, _)| method == "editChatInviteLink")
        .ok_or("edit")?
        .1;
    assert!(edit.get("member_limit").is_none());
    assert!(edit.get("expire_date").is_none());
    links.revoke(link).await?;
    let again = links.revoke(link).await?;
    assert_eq!(again["joined"], 1);
    assert_eq!(again["requests"], 1);
    assert_eq!(again["actor"], "api@test");
    assert_eq!(mock.count("revokeChatInviteLink"), 1);
    assert!(
        links
            .edit(&json!({"invite_link":link,"name":"bad"}))
            .await
            .is_err()
    );
    assert!(links.revoke("https://t.me/+unknown").await.is_err());
    let subscription = links
        .create(
            &json!({"chat_id":-100500,"subscription_price":500,"name":"Paid"}),
            "api",
        )
        .await?;
    assert_eq!(subscription["kind"], "subscription");
    assert_eq!(subscription["subscription_period"], 2592000);
    assert!(
        links
            .create(
                &json!({"chat_id":-100500,"subscription_price":500,"member_limit":2}),
                "api"
            )
            .await
            .is_err()
    );
    assert_eq!(mock.count("createChatSubscriptionInviteLink"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn named_link_counts_joins_and_filters_listing_by_chat()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"chat_id": "@chan", "name": "Хабр", "member_limit": 100}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["name"], "Хабр");
    assert_eq!(link["member_limit"], 100);
    assert_eq!(link["joined"], 0);
    assert_eq!(
        mock.last("createChatInviteLink"),
        json!({"chat_id": "@chan", "name": "Хабр", "member_limit": 100})
    );
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        updates::handle_update(&state, &member(&url, "left", "member", json!({}))).await?,
        Some("join")
    );
    assert_eq!(
        updates::handle_update(
            &state,
            &member(&url, "kicked", "restricted", json!({"is_member": true}))
        )
        .await?,
        Some("join")
    );
    // an admin promoted, a link the studio did not make: nothing to count
    assert_eq!(
        updates::handle_update(&state, &member(&url, "member", "administrator", json!({}))).await?,
        None
    );
    assert_eq!(
        updates::handle_update(
            &state,
            &member("https://t.me/+other", "left", "member", json!({}))
        )
        .await?,
        None
    );
    let listed = request(
        app.clone(),
        "GET",
        "/api/invite-links?chat_id=%40chan",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    let listed = listed.as_array().ok_or("listing")?;
    let counts = listed
        .iter()
        .map(|link| {
            (
                link["name"].as_str().unwrap_or_default(),
                link["joined"].as_i64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, [("Хабр", 2)]);
    assert_eq!(
        request(
            app,
            "GET",
            "/api/invite-links?chat_id=%40other",
            Value::Null,
            "owner-secret",
            false,
            None
        )
        .await
        .1,
        json!([])
    );
    assert!(updates::ALLOWED_UPDATES.contains(&"chat_member"));
    assert!(updates::ALLOWED_UPDATES.contains(&"chat_join_request"));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn join_request_link_keeps_expiry_and_requests_across_edits_and_revoke()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let expires = auth::now() + 86400;
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"name": "Заявки", "creates_join_request": true, "expire_date": expires}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["creates_join_request"], 1);
    assert_eq!(link["expire_date"], expires);
    let sent = mock.last("createChatInviteLink");
    assert_eq!(sent["chat_id"], "@test_channel");
    assert_eq!(sent["creates_join_request"], true);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"chat_join_request": {"chat": {"id": -1001}, "from": {"id": 8}, "invite_link": {"invite_link": url.clone()}}})
        )
        .await?,
        Some("join_request")
    );
    let conflicting = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"creates_join_request": true, "member_limit": 5}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(conflicting.0, StatusCode::BAD_REQUEST);
    assert!(
        conflicting.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("заяв")
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({"name": "x".repeat(33)}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // Telegram drops what an edit leaves out, so a rename repeats the expiry and the join requests
    let edited = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "name": "Заявки 2"}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{}", edited.1);
    assert_eq!(edited.1["name"], "Заявки 2");
    assert_eq!(edited.1["requests"], 1);
    assert_eq!(edited.1["expire_date"], expires);
    assert_eq!(edited.1["creates_join_request"], 1);
    let sent = mock.last("editChatInviteLink");
    assert_eq!(sent["expire_date"], expires);
    assert_eq!(sent["creates_join_request"], true);
    let later = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "expire_date": expires + 60}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(later["name"], "Заявки 2");
    assert_eq!(later["expire_date"], expires + 60);
    assert_eq!(later["creates_join_request"], 1);
    let off = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "expire_date": 0, "creates_join_request": false}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(off["name"], "Заявки 2");
    assert!(off["expire_date"].is_null());
    assert_eq!(off["creates_join_request"], 0);
    assert!(mock.last("editChatInviteLink").get("expire_date").is_none());
    let revoked = request(
        app.clone(),
        "POST",
        "/api/invite-links/revoke",
        json!({"invite_link": url.clone()}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(revoked["is_revoked"], 1);
    assert_eq!(revoked["name"], "Заявки 2");
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": url}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link": "https://t.me/+nope"}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn subscription_link_rejects_limits_and_changes_only_its_name()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"name": "Клуб", "subscription_price": 50}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["kind"], "subscription");
    assert_eq!(link["subscription_price"], 50);
    let body = mock.last("createChatSubscriptionInviteLink");
    assert_eq!(body["subscription_period"], 2592000);
    assert_eq!(body["subscription_price"], 50);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({"subscription_price": 50, "member_limit": 3}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": url.clone(), "name": "Клуб+", "member_limit": 3}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let edited = request(
        app,
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url, "name": "Клуб+"}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{}", edited.1);
    assert_eq!(edited.1["name"], "Клуб+");
    assert_eq!(mock.last("editChatSubscriptionInviteLink")["name"], "Клуб+");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn invite_link_mutations_require_publish_while_lists_stay_readable()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "d", "draft-secret", "draft")?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": "x"}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link": "x"}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/invite-links",
            Value::Null,
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(mock.calls().is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn invite_links_work_through_mcp_create_edit_list_and_revoke()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let created = mcp_call(
        app.clone(),
        "create_invite_link",
        json!({"chat_id": "@chan", "name": "Telegram-чат"}),
        "owner-secret",
    )
    .await;
    assert_eq!(created["isError"], false);
    let link = created["structuredContent"].clone();
    assert!(
        link["invite_link"]
            .as_str()
            .ok_or("link")?
            .starts_with("https://t.me/+")
    );
    let listed =
        mcp_list(&mcp_call(app.clone(), "list_invite_links", json!({}), "owner-secret").await);
    let names = listed
        .iter()
        .map(|item| item["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Telegram-чат"]);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    let limited = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "member_limit": 20}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(limited["name"], "Telegram-чат");
    assert_eq!(limited["member_limit"], 20);
    let renamed = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "name": "Чат"}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(renamed["name"], "Чат");
    assert_eq!(renamed["member_limit"], 20);
    let unnamed = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "name": "", "member_limit": 0}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert!(unnamed["name"].is_null() || unnamed["name"] == json!(""));
    assert!(unnamed["member_limit"].is_null() || unnamed["member_limit"] == json!(0));
    let revoked = mcp_call(
        app,
        "revoke_invite_link",
        json!({"invite_link": url}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(revoked["is_revoked"], 1);
    assert_eq!(mock.count("revokeChatInviteLink"), 1);
    server.abort();
    Ok(())
}
