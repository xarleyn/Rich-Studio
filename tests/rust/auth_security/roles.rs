use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{TestResult, blocks_post, fake_server, post_actors, request, settings};

#[tokio::test]
async fn unsafe_origins_unknown_hosts_and_mcp_publish_off_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.mcp_publish = false;
    let app = build_router(AppState::new(config));
    let owner = [("x-api-key", "owner-secret")];
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({}),
            &[
                ("x-api-key", "owner-secret"),
                ("origin", "http://localhost:9999")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let request_bad_host = Request::builder()
        .uri("/healthz")
        .header("host", "attacker.example")
        .body(Body::empty())?;
    assert_eq!(
        app.clone().oneshot(request_bad_host).await?.status(),
        StatusCode::BAD_REQUEST
    );
    let (_, tools, _) = request(
        app.clone(),
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &owner,
    )
    .await;
    assert!(
        !tools["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "publish_post")
    );
    let (_, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish_post","arguments":{}}}), &owner).await;
    assert_eq!(result["result"]["isError"], true);
    Ok(())
}

#[tokio::test]
async fn open_studio_stays_open_until_a_key_or_token_exists() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::OK
    );
    let (status, session, _) = request(app.clone(), "GET", "/api/session", Value::Null, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session, json!({"name":null,"level":"admin","auth":false}));
    auth::add_token(&config, "agent", "rs_test_agent", "read")?;
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/healthz", Value::Null, &[]).await.0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn token_levels_gate_reads_drafts_publishing_and_x_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    for (name, secret, level) in [
        ("r", "rs_test_r", "read"),
        ("d", "rs_test_d", "draft"),
        ("p", "rs_test_p", "publish"),
    ] {
        auth::add_token(&config, name, secret, level)?;
    }
    let app = build_router(AppState::new(config));
    let read = [("authorization", "Bearer rs_test_r")];
    let draft = [("authorization", "Bearer rs_test_d")];
    let publish = [("authorization", "Bearer rs_test_p")];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &read)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"x","post":blocks_post()}),
            &read
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"x","post":blocks_post()}),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, denied, _) = request(
        app.clone(),
        "POST",
        "/api/send",
        json!({"post":blocks_post()}),
        &draft,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        denied["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("publish")
    );
    assert!(fake.calls().is_empty());
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &publish
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", "rs_test_r")]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", "wrong")]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn every_route_that_posts_to_a_channel_requires_publish() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "d", "rs_test_d", "draft")?;
    let app = build_router(AppState::new(config));
    let draft = [("authorization", "Bearer rs_test_d")];
    for (method, path) in [
        ("POST", "/api/send"),
        ("POST", "/api/edit"),
        ("POST", "/api/schedule"),
        ("POST", "/api/poll"),
        ("POST", "/api/posts/x/stop-poll"),
        ("POST", "/api/posts/x/pin"),
        ("DELETE", "/api/posts/x/pin"),
        ("POST", "/api/posts/x/buttons"),
        ("POST", "/api/posts/x/edit"),
        ("POST", "/api/posts/x/reschedule"),
        ("DELETE", "/api/posts/x"),
    ] {
        let body = if method == "POST" {
            json!({})
        } else {
            Value::Null
        };
        assert_eq!(
            request(app.clone(), method, path, body, &draft).await.0,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn preview_to_another_chat_requires_publish() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "d", "rs_test_d", "draft")?;
    auth::add_token(&config, "p", "rs_test_p", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    let draft = [("authorization", "Bearer rs_test_d")];
    let publish = [("authorization", "Bearer rs_test_p")];
    let send = json!({"post":blocks_post()});
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            send.clone(),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":" 777"}),
            &draft
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"@chan"}),
            &draft
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"@chan"}),
            &publish
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut no_preview = config;
    no_preview.preview_chat.clear();
    let without_preview = build_router(AppState::new(no_preview));
    assert_eq!(
        request(
            without_preview,
            "POST",
            "/api/preview-send",
            json!({"post":blocks_post(),"chat_id":"777"}),
            &draft
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn actor_is_the_token_name_and_optional_client() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, _fake) = fake_server().await;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    config.api_base = origin;
    auth::add_token(&config, "claude-laptop", "rs_test_claude", "publish")?;
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &[
                ("authorization", "Bearer rs_test_claude"),
                ("x-studio-client", "mcp:claude")
            ]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &[("authorization", "Bearer rs_test_claude")]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post_actors(&config)?,
        vec!["claude-laptop", "mcp:claude@claude-laptop"]
    );
    Ok(())
}
