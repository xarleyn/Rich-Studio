use axum::http::StatusCode;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, cookie, login_as, request, settings, signed_callback};

#[tokio::test]
async fn telegram_login_is_signed_browser_bound_one_use_and_owner_stays_root()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 101;
    let app = build_router(AppState::new(config.clone()));
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        json!({}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = body["auth_url"]
        .as_str()
        .and_then(|url| url.split("state=").nth(1))
        .ok_or("state")?;
    let login_cookie = cookie(&headers);
    let callback = signed_callback(state, 101, "1:fixture");
    assert_eq!(
        request(app.clone(), "GET", &callback, Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let tampered = signed_callback(state, 101, "wrong-token");
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &tampered,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app.clone(),
        "GET",
        &callback,
        Value::Null,
        &[("cookie", &login_cookie)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let owner_cookie = cookie(&headers);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .1["level"],
        "admin"
    );
    assert!(auth::account(&config, 101)?.is_none());
    assert!(!config.account_dir(101).exists());
    assert_eq!(
        request(
            app,
            "GET",
            &callback,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    config.owner_telegram_id = 202;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn owner_login_opens_root_admin_without_public_account() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    let (status, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["name"], Value::Null);
    assert_eq!(session["level"], "admin");
    assert!(session.get("user").is_none());
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(auth::account(&config, 555)?.is_none());
    assert!(!auth::account_ids(&config)?.contains(&555));
    assert!(!config.account_dir(555).exists());
    let connection = auth::database(&config)?;
    let sessions: i64 = connection.query_row(
        "select count(*) from account_sessions where telegram_id=?",
        [555],
        |row| row.get(0),
    )?;
    assert_eq!(sessions, 0);
    Ok(())
}

#[tokio::test]
async fn regular_user_alongside_configured_owner_stays_public() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    let (_, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(101));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/export",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn same_telegram_id_is_regular_without_owner_setting() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    let (_, session, _) = request(
        app,
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(555));
    assert!(auth::account_ids(&config)?.contains(&555));
    Ok(())
}

#[tokio::test]
async fn owner_session_dies_when_owner_setting_changes() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 555;
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 555).await?;
    assert_eq!(
        request(
            app,
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut cleared = config.clone();
    cleared.owner_telegram_id = 0;
    assert_eq!(
        request(
            build_router(AppState::new(cleared)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mut other = config;
    other.owner_telegram_id = 777;
    assert_eq!(
        request(
            build_router(AppState::new(other)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn auth_config_reports_owner_login() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let (status, body, _) = request(app, "GET", "/api/auth/config", Value::Null, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["owner_login"], false);
    let mut owner = config;
    owner.owner_telegram_id = 555;
    let (status, body, _) = request(
        build_router(AppState::new(owner)),
        "GET",
        "/api/auth/config",
        Value::Null,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["owner_login"], true);
    Ok(())
}
