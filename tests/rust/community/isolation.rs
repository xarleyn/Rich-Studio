use axum::http::StatusCode;
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    services::accounts,
    store::{auth, channels, invite_links as link_store},
};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{mock, request, settings, suggestion};

#[tokio::test]
async fn public_invites_and_suggestions_cannot_read_or_mutate_other_accounts()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    for (uid, secret) in [(11, "account-a"), (22, "account-b")] {
        accounts::initialize(&config, uid)?;
        auth::save_account(&config, &json!({"id":uid,"first_name":"test"}))?;
        auth::add_account_session(&config, secret, uid, 86400)?;
    }
    let a = AccountContext::account(&config, 11, Role::Publish);
    let b = AccountContext::account(&config, 22, Role::Publish);
    channels::save(
        &a,
        &json!({"chat_id":"-100500","chat_num":-100500,"type":"channel","can_post":1}),
    )?;
    link_store::save(
        &a,
        "https://t.me/+only-a",
        "-100500",
        "a",
        &json!({"name":"Private"}),
    )?;
    let sid = suggestion(&a, 7)?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/invite-links",
            Value::Null,
            "account-b",
            true,
            None
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/suggested-posts",
            Value::Null,
            "account-b",
            true,
            None
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link":"https://t.me/+only-a"}),
            "account-b",
            true,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &format!("/api/suggested-posts/{sid}/approve"),
            json!({}),
            "account-b",
            true,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/invite-links",
            json!({"chat_id":-100500,"name":"Owned"}),
            "account-a",
            true,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("createChatInviteLink"), 1);
    assert_eq!(mock.count("getChatMember"), 1);
    assert!(link_store::listing(&b, None)?.is_empty());
    server.abort();
    Ok(())
}
