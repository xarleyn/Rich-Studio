use axum::http::StatusCode;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, cookie, request, settings};

#[tokio::test]
async fn configured_key_is_enforced_and_legacy_key_session_dies_on_rotation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"name":null,"level":"admin"}));
    let session = cookie(&headers);
    assert!(!session.contains("owner-secret"));
    assert!(headers["set-cookie"].to_str()?.contains("HttpOnly"));
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
        StatusCode::OK
    );
    config.api_key = "rotated".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(config)),
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
async fn editor_session_cookie_is_httponly_and_logout_ends_it() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, false);
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":"nope"}),
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let raw = headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .ok_or("set-cookie")?;
    assert!(raw.contains("rs_session="));
    assert!(!raw.contains("owner-secret"));
    assert!(raw.contains("HttpOnly"));
    assert!(raw.contains("SameSite=Strict"));
    assert!(!raw.contains("Secure"));
    let session = cookie(&headers);
    let (status, who, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &session)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["level"], "admin");
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            "/api/session",
            Value::Null,
            &[("cookie", &session)]
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
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app,
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("Secure")
    );
    Ok(())
}

#[tokio::test]
async fn key_session_ends_when_key_changes_or_is_removed() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key = "old".to_owned();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":"old"}),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, _, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"old"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session = cookie(&headers);
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
        StatusCode::OK
    );
    let mut rotated = config.clone();
    rotated.api_key = "new".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(rotated.clone())),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    auth::add_token(&config, "t", "rs_test_t", "read")?;
    let mut removed = rotated;
    removed.api_key.clear();
    assert_eq!(
        request(
            build_router(AppState::new(removed)),
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
async fn session_of_an_unknown_token_is_not_admin() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "t", "rs_test_t", "read")?;
    auth::add_session(&config, "orphan", Some("ghost"), 3600, None, None)?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", "rs_session=orphan")]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn non_ascii_key_is_refused_and_accepted_when_current() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key = "secret".to_owned();
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app, "POST", "/api/session", json!({"key":"ключ"}), &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut cyrillic = config;
    cyrillic.api_key = "ключ-ёж".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(cyrillic)),
            "POST",
            "/api/session",
            json!({"key":"ключ-ёж"}),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    Ok(())
}
