use axum::http::StatusCode;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{
    TestResult, bearer, blocks_post, fake_server, issue_token, login_as, mcp_call_as, request,
    settings,
};

#[tokio::test]
async fn account_sessions_tokens_and_drafts_are_isolated_over_http()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    for uid in [101, 202] {
        auth::save_account(
            &config,
            &json!({"id":uid,"first_name":format!("User {uid}")}),
        )?;
        auth::add_account_session(&config, &format!("session-{uid}"), uid, 3600)?;
    }
    let app = build_router(AppState::new(config.clone()));
    let one = [("cookie", "rs_session=session-101")];
    let two = [("cookie", "rs_session=session-202")];
    let (status, draft, _) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        json!({"title":"Private","post":{"rich_message":{"markdown":"secret"}}}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = format!("/api/drafts/{}", draft["id"].as_str().ok_or("draft id")?);
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &two)
            .await
            .1,
        json!([])
    );
    let (status, token, _) = request(
        app.clone(),
        "POST",
        "/api/account/tokens",
        json!({"name":"Agent","level":"read"}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let bearer = format!("Bearer {}", token["secret"].as_str().ok_or("secret")?);
    let credential = [("authorization", bearer.as_str())];
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"Denied","post":{}}),
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/account/tokens",
            Value::Null,
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":token["secret"]}),
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let token_path = format!(
        "/api/account/tokens/{}",
        token["token"]["id"].as_str().ok_or("token id")?
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/api/session", Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn account_isolation_covers_root_tokens_drafts_and_token_ids() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    auth::add_token(&config, "operator", "operator-secret", "admin")?;
    let app = build_router(AppState::new(config));
    let a = login_as(&app, 101).await?;
    let first = issue_token(&app, &a, json!({"name":"Claude laptop"})).await?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"A private","post":blocks_post()}),
            &[("cookie", &a)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let b = login_as(&app, 202).await?;
    let second = issue_token(&app, &b, json!({"name":"Claude laptop"})).await?;
    let (_, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &b)],
    )
    .await;
    assert_eq!(listed["tokens"], json!([second["token"].clone()]));
    let first_id = first["token"]["id"].as_str().ok_or("token id")?;
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("/api/account/tokens/{first_id}"),
            Value::Null,
            &[("cookie", &b)]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/account/tokens",
            Value::Null,
            &[("authorization", "Bearer operator-secret")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let first_bearer = bearer(&first)?;
    let second_bearer = bearer(&second)?;
    let (_, own) = mcp_call_as(
        app.clone(),
        &[("authorization", first_bearer.as_str())],
        "list_drafts",
        json!({}),
    )
    .await;
    let (_, other) = mcp_call_as(
        app.clone(),
        &[("authorization", second_bearer.as_str())],
        "list_drafts",
        json!({}),
    )
    .await;
    let (_, operator) = mcp_call_as(
        app.clone(),
        &[("authorization", "Bearer operator-secret")],
        "list_drafts",
        json!({}),
    )
    .await;
    assert!(own.contains("A private"));
    assert!(!other.contains("A private"));
    assert!(!operator.contains("A private"));
    let first_secret = first["secret"].as_str().ok_or("secret")?;
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("x-api-key", first_secret)]
        )
        .await
        .0,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn mcp_and_http_enforce_account_roles_and_current_telegram_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (origin, fake) = fake_server().await;
    let mut config = settings(&dir, true);
    config.api_base = origin;
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    fake.queue(
        "getChat",
        vec![json!({"ok":true,"result":{"id":-1001,"type":"channel","title":"Мой","username":"mine"}})],
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/channels",
            json!({"chat_id":"@mine"}),
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    let read = issue_token(&app, &login, json!({"name":"read","level":"read"})).await?;
    let draft = issue_token(&app, &login, json!({"name":"draft","level":"draft"})).await?;
    let publish = issue_token(&app, &login, json!({"name":"publish","level":"publish"})).await?;
    let read_bearer = bearer(&read)?;
    let draft_bearer = bearer(&draft)?;
    let publish_bearer = bearer(&publish)?;
    let read_headers = [("authorization", read_bearer.as_str())];
    let draft_headers = [("authorization", draft_bearer.as_str())];
    let publish_headers = [("authorization", publish_bearer.as_str())];
    let (result, _) = mcp_call_as(app.clone(), &read_headers, "list_drafts", json!({})).await;
    assert_eq!(result["isError"], false);
    let (result, _) = mcp_call_as(
        app.clone(),
        &read_headers,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], true);
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft_headers,
        "save_draft",
        json!({"post":blocks_post()}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let (result, _) = mcp_call_as(
        app.clone(),
        &draft_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/send",
            json!({"post":blocks_post()}),
            &draft_headers
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for headers in [
        read_headers.as_slice(),
        draft_headers.as_slice(),
        publish_headers.as_slice(),
    ] {
        assert_eq!(
            request(app.clone(), "GET", "/api/logs", Value::Null, headers)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(app.clone(), "POST", "/api/raw/getMe", json!({}), headers)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (result, _) = mcp_call_as(app.clone(), headers, "studio_logs", json!({})).await;
        assert_eq!(result["isError"], true);
    }
    let (result, _) = mcp_call_as(
        app.clone(),
        &publish_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    fake.clear();
    fake.queue(
        "getChatMember",
        vec![json!({"ok":true,"result":{"status":"member"}})],
    );
    let (result, _) = mcp_call_as(
        app,
        &publish_headers,
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@mine","idempotency_key":"new"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(fake.send_calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn account_token_expiration_and_personal_mode_are_checked_per_request() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 101).await?;
    let created = issue_token(&app, &login, json!({"name":"Claude laptop"})).await?;
    let credential = bearer(&created)?;
    let headers = [("authorization", credential.as_str())];
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &headers)
            .await
            .0,
        StatusCode::OK
    );
    let mut personal = config.clone();
    personal.public = false;
    auth::add_token(&personal, "keep-auth-on", "keep-auth-secret", "read")?;
    assert_eq!(
        request(
            build_router(AppState::new(personal)),
            "GET",
            "/api/drafts",
            Value::Null,
            &headers
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let connection = auth::database(&config)?;
    connection.execute(
        "update account_tokens set expires_at=?",
        rusqlite::params![auth::now()],
    )?;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &headers
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_token_management_requires_telegram_login_and_same_origin() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(app.clone(), "GET", "/api/account/tokens", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let login = login_as(&app, 101).await?;
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"x"}),
            &[
                ("cookie", login.as_str()),
                ("origin", "https://evil.example")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "DELETE",
            "/api/account/tokens/unknown",
            Value::Null,
            &[("cookie", &login), ("sec-fetch-site", "cross-site")]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}
