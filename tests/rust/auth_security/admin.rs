use axum::http::StatusCode;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, cookie, fake_server, mcp_call_as, post_actors, request, settings,
};

#[tokio::test]
async fn revoked_token_and_its_sessions_stop_working() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "gone", "rs_test_gone", "read")?;
    let app = build_router(AppState::new(config.clone()));
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"rs_test_gone"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session = cookie(&headers);
    let read = [("authorization", "Bearer rs_test_gone")];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::OK
    );
    assert!(auth::revoke_token(&config, "gone")?);
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn raw_api_is_admin_only_and_can_be_switched_off() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    auth::add_token(&config, "a", "rs_test_a", "admin")?;
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_p")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_a")]
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut disabled = config;
    disabled.raw_api = false;
    assert_eq!(
        request(
            build_router(AppState::new(disabled)),
            "POST",
            "/api/raw/getMe",
            json!({}),
            &[("authorization", "Bearer rs_test_a")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn backup_download_is_admin_only() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("authorization", "Bearer rs_test_p")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn mcp_tools_check_token_level() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "agent", "rs_test_agent", "draft")?;
    auth::add_token(&config, "reader", "rs_test_reader", "read")?;
    auth::add_token(&config, "pub", "rs_test_pub", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    let draft = [("authorization", "Bearer rs_test_agent")];
    let read = [("authorization", "Bearer rs_test_reader")];
    let publish = [("authorization", "Bearer rs_test_pub")];
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft,
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let (result, text) = mcp_call_as(
        app.clone(),
        &draft,
        "publish_post",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("publish"));
    assert!(fake.calls().is_empty());
    let (result, _) = mcp_call_as(app.clone(), &read, "list_drafts", json!({})).await;
    assert_eq!(result["isError"], false);
    let (result, text) = mcp_call_as(
        app.clone(),
        &read,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("draft"));
    let (result, _) = mcp_call_as(
        app.clone(),
        &publish,
        "publish_post",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], false);
    assert_eq!(post_actors(&config)?, vec!["mcp@pub"]);
    Ok(())
}
