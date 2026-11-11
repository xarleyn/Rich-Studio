//! Account database quota: growth, MCP bypass and post history.

use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::errors::StoreError;
use rich_studio::services::accounts;
use rich_studio::store::db::{self, Store};
use rich_studio::store::{drafts, editing};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, call, fake_server, login_as, mcp_call, public_settings, save_sent_post,
};

#[tokio::test]
async fn public_database_growth_is_bounded_and_accounts_are_independent() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let app = build_router(AppState::new(config.clone()));
    let who = login_as(&app, 101).await;
    let payload = json!({"rich_message":{"html":"x".repeat(200_000)}});
    let mut statuses = Vec::new();
    for index in 0..8 {
        let reply = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"title":index.to_string(),"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await;
        statuses.push(reply.status);
    }
    assert!(
        statuses.contains(&StatusCode::PAYLOAD_TOO_LARGE),
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|status| [StatusCode::OK, StatusCode::PAYLOAD_TOO_LARGE].contains(status)),
        "{statuses:?}"
    );
    let context = AccountContext::account(&config, 101, Role::Publish);
    let connection = Store::connect(&context)?;
    let pages: i64 = connection.query_row("pragma page_count", [], |row| row.get(0))?;
    let page_size: i64 = connection.query_row("pragma page_size", [], |row| row.get(0))?;
    assert!(pages * page_size <= 1024 * 1024, "{pages} x {page_size}");
    let other = login_as(&app, 202).await;
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", other.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn mcp_cannot_bypass_public_database_quota() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let app = build_router(AppState::new(config));
    let who = login_as(&app, 101).await;
    let payload = json!({"rich_message":{"html":"x".repeat(200_000)}});
    for _ in 0..8 {
        let _ = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await;
    }
    let result = mcp_call(
        app.clone(),
        &who,
        "save_draft",
        json!({"title":"Over quota","post":payload}),
    )
    .await;
    assert_eq!(result["isError"], true, "{result}");
    assert!(result.to_string().contains("Лимит базы"), "{result}");
    let listed = call(
        app.clone(),
        "GET",
        "/api/drafts",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    for draft in listed.as_array().cloned().unwrap_or_default() {
        let id = draft["id"].as_str().unwrap_or_default();
        assert_eq!(
            call(
                app.clone(),
                "DELETE",
                &format!("/api/drafts/{id}"),
                None,
                &[("cookie", who.as_str())],
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"post":payload})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn database_quota_also_limits_post_history_and_preserves_operator() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    let post = save_sent_post(&context, -1001, 77);
    let id = post["id"].as_str().unwrap_or_default();
    let large = json!({"html":"x".repeat(200_000)});
    let mut full = false;
    for _ in 0..8 {
        let taken = editing::claim_version(&context, id, None)?
            .unwrap_or_else(|| panic!("post version is missing"));
        match editing::replace_sent(
            &context,
            id,
            &json!({"rich_message":{"markdown":"next"}}),
            &large,
            "ui",
            taken,
        ) {
            Ok(_) => {}
            Err(StoreError::StorageFull) => {
                full = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert!(full, "post history did not hit the account quota");
    let root = AccountContext::root(&config);
    let mut root_connection = Store::connect(&root)?;
    db::migrate(&mut root_connection)?;
    drop(root_connection);
    for index in 0..8 {
        drafts::save(&root, None, "Operator", &large, None, None, None)?
            .unwrap_or_else(|| panic!("operator draft {index} is missing"));
    }
    Ok(())
}
