use axum::http::StatusCode;
use rich_studio::account_context::AccountContext;
use rich_studio::store::{auth, drafts};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, call_on, callback_path, cookie, fake_server, login_as,
    login_fields, public_config, query_path, set_cookie_has, signed_hash, start_challenge,
};

#[tokio::test]
async fn public_mode_is_closed_without_operator_keys() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    assert_eq!(
        call(app.clone(), "GET", "/api/drafts", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app.clone(), "GET", "/api/session", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app.clone(), "GET", "/", None, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/auth/config", None, &[]).await.json(),
        json!({"telegram":true,"bot_username":"YourBot","owner_login":false})
    );
    Ok(())
}

#[tokio::test]
async fn signed_login_sets_cookie_identity_and_logout_clears_it() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    let session = call(
        app.clone(),
        "GET",
        "/api/session",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(session["user"]["telegram_id"], json!(101));
    assert_eq!(session["level"], json!("publish"));
    assert_eq!(session["name"], json!("tg:101"));

    let config = call(
        app.clone(),
        "GET",
        "/api/config",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(config["default_chat"], json!(""));
    assert_eq!(config["preview_chat"], json!("101"));
    assert!(drafts::list(&AccountContext::root(&settings))?.is_empty());

    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            "/api/session",
            None,
            &[("cookie", who.as_str())]
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/drafts", None, &[("cookie", who.as_str())])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn forged_identity_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings.clone()));

    for change in ["id", "hash", "first_name"] {
        let (state, state_cookie) = start_challenge(&app).await;
        let base = login_fields(101, auth::now());
        let mut signed = base.clone();
        let hash = if change == "hash" {
            "подделка".to_owned()
        } else {
            signed_hash(&base)
        };
        signed.insert("hash".to_owned(), hash);
        match change {
            "id" => {
                signed.insert("id".to_owned(), "999".to_owned());
            }
            "first_name" => {
                signed.insert("first_name".to_owned(), "другое".to_owned());
            }
            _ => {}
        }
        let path = query_path(&signed, &state);
        let state_header = format!("rs_telegram_login={state_cookie}");
        let reply = call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{change}");
    }
    assert!(auth::account_ids(&settings)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn old_and_future_signatures_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    for offset in [-600_i64, 600] {
        let (state, state_cookie) = start_challenge(&app).await;
        let path = callback_path(&state, &login_fields(101, auth::now() + offset));
        let state_header = format!("rs_telegram_login={state_cookie}");
        let reply = call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())],
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{offset}");
    }
    Ok(())
}

#[tokio::test]
async fn login_requires_browser_challenge_and_cannot_be_replayed() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    let (state, state_cookie) = start_challenge(&app).await;
    let path = callback_path(&state, &login_fields(101, auth::now()));
    let state_header = format!("rs_telegram_login={state_cookie}");
    assert_eq!(
        call(app.clone(), "GET", &path, None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &path,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let duplicated = format!("{path}&id=999");
    assert_eq!(
        call(
            app,
            "GET",
            &duplicated,
            None,
            &[("cookie", state_header.as_str())]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn session_cookie_is_httponly_secure_and_challenge_is_lax() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let app = build_router(AppState::new(settings));

    let start = call_on(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        "localhost",
        Some(json!({})),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    for flag in ["HttpOnly", "Secure", "SameSite=lax"] {
        assert!(set_cookie_has(&start.headers, flag), "{flag}");
    }
    let auth_url = start.json()["auth_url"]
        .as_str()
        .expect("auth_url")
        .to_owned();
    let state = url::Url::parse(&auth_url)
        .expect("auth url")
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("state");
    let state_cookie = cookie(&start.headers, "rs_telegram_login").expect("challenge cookie");
    let path = callback_path(&state, &login_fields(101, auth::now()));
    let state_header = format!("rs_telegram_login={state_cookie}");
    let callback = call_on(
        app,
        "GET",
        &path,
        "localhost",
        None,
        &[
            ("cookie", state_header.as_str()),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(callback.status, StatusCode::SEE_OTHER);
    for flag in ["HttpOnly", "Secure", "SameSite=strict"] {
        assert!(set_cookie_has(&callback.headers, flag), "{flag}");
    }
    assert_eq!(callback.header("cache-control"), "no-store");
    Ok(())
}

#[tokio::test]
async fn login_flood_is_limited_and_logout_still_works() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    let mut last = None;
    for _ in 0..30 {
        last = Some(
            call(
                app.clone(),
                "POST",
                "/api/auth/telegram/start",
                Some(json!({})),
                &[],
            )
            .await,
        );
    }
    let last = last.expect("last challenge");
    assert_eq!(last.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(last.header("retry-after"), "60");
    assert_eq!(
        call(app, "DELETE", "/api/session", None, &[("cookie", &who)])
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}
