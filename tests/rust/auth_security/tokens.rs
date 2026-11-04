use axum::http::StatusCode;
use rich_studio::services::account_tokens;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, bearer, issue_token, login_as, request, settings};

#[tokio::test]
async fn account_token_limits_expiry_and_management_validation_are_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    auth::save_account(&config, &json!({"id":101,"first_name":"User"}))?;
    auth::add_account_session(&config, "browser", 101, 3600)?;
    let app = build_router(AppState::new(config.clone()));
    let owner = [("cookie", "rs_session=browser")];
    for body in [
        json!({"name":"x","level":"admin"}),
        json!({"name":"x","telegram_id":202}),
        json!({"name":"x","expires_in_days":true}),
        json!({"name":"x","expires_in_days":0}),
        json!({"name":"   ","level":"draft"}),
        json!({"name":"a\nb","level":"draft"}),
        json!({"name":"x".repeat(81),"level":"draft"}),
        json!({"name":"x","expires_in_days":366}),
        json!({"name":"x","secret":"chosen"}),
    ] {
        assert_eq!(
            request(app.clone(), "POST", "/api/account/tokens", body, &owner)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut first = Value::Null;
    let mut second = Value::Null;
    for index in 0..account_tokens::MAX_ACTIVE {
        let (status, token, _) = request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":format!("Agent{index}")}),
            &owner,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        if index == 0 {
            first = token.clone();
        }
        if index == 1 {
            second = token;
        }
    }
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"overflow"}),
            &owner
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    auth::database(&config)?.execute(
        "update account_tokens set expires_at=? where id=?",
        rusqlite::params![auth::now(), first["token"]["id"].as_str()],
    )?;
    let bearer = format!("Bearer {}", first["secret"].as_str().ok_or("secret")?);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("authorization", &bearer)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"replacement"}),
            &owner
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let second_id = second["token"]["id"].as_str().ok_or("token id")?;
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("/api/account/tokens/{second_id}"),
            Value::Null,
            &owner
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, unlimited, _) = request(
        app,
        "POST",
        "/api/account/tokens",
        json!({"name":"unlimited","expires_in_days":null}),
        &owner,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(unlimited["token"]["expires_at"].is_null());
    Ok(())
}

#[tokio::test]
async fn revoked_last_token_does_not_open_service_and_mcp_enforces_roles()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "reader", "read-secret", "read")?;
    let app = build_router(AppState::new(config.clone()));
    let headers = [("authorization", "Bearer read-secret")];
    assert_eq!(
        request(app.clone(), "POST", "/api/send", json!({}), &headers)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"x"}}}}}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["isError"], true);
    auth::revoke_token(&config, "reader")?;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn browser_can_issue_list_and_revoke_own_account_token() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.public_url = "https://testserver".to_owned();
    let app = build_router(AppState::new(config.clone()));
    let login = login_as(&app, 101).await?;
    let created = issue_token(
        &app,
        &login,
        json!({"name":"Claude laptop","level":"draft"}),
    )
    .await?;
    let secret = created["secret"].as_str().ok_or("secret")?.to_owned();
    let (status, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["mcp_url"], "https://testserver/mcp");
    assert_eq!(listed["tokens"], json!([created["token"].clone()]));
    let listed_text = listed.to_string();
    assert!(!listed_text.contains(&secret));
    assert!(!listed_text.contains("hash"));
    assert!(
        created["token"]["expires_at"]
            .as_i64()
            .ok_or("expires_at")?
            > auth::now()
    );
    let connection = auth::database(&config)?;
    let hash: String =
        connection.query_row("select hash from account_tokens", [], |row| row.get(0))?;
    assert_eq!(hash, auth::hash_secret(&secret));
    let row: String = connection.query_row(
        "select id||telegram_id||name||hash||level||created_at||ifnull(expires_at,'')||ifnull(last_used_at,'')||ifnull(revoked_at,'') from account_tokens",
        [],
        |row| row.get(0),
    )?;
    assert!(!row.contains(&secret));
    let credential = bearer(&created)?;
    let (status, session, _) = request(
        app.clone(),
        "GET",
        "/api/session",
        Value::Null,
        &[("authorization", &credential)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["user"]["telegram_id"].as_i64(), Some(101));
    let login = login_as(&app, 101).await?;
    let (status, listed, _) = request(
        app.clone(),
        "GET",
        "/api/account/tokens",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed["tokens"][0]["last_used_at"]
            .as_i64()
            .ok_or("last_used_at")?
            > 0
    );
    let id = created["token"]["id"].as_str().ok_or("token id")?;
    let path = format!("/api/account/tokens/{id}");
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &path,
            Value::Null,
            &[("cookie", &login)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &path,
            Value::Null,
            &[("cookie", &login)]
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
            &[("authorization", &credential)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
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
    Ok(())
}

#[tokio::test]
async fn account_tokens_cannot_manage_credentials_or_become_browser_sessions() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    let login = login_as(&app, 101).await?;
    for level in ["read", "draft", "publish"] {
        let created = issue_token(
            &app,
            &login,
            json!({"name":format!("Claude {level}"),"level":level}),
        )
        .await?;
        let credential = bearer(&created)?;
        let headers = [
            ("cookie", login.as_str()),
            ("authorization", credential.as_str()),
        ];
        assert_eq!(
            request(
                app.clone(),
                "GET",
                "/api/account/tokens",
                Value::Null,
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                app.clone(),
                "POST",
                "/api/account/tokens",
                json!({"name":"escalate"}),
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let id = created["token"]["id"].as_str().ok_or("token id")?;
        assert_eq!(
            request(
                app.clone(),
                "DELETE",
                &format!("/api/account/tokens/{id}"),
                Value::Null,
                &headers
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
                json!({"key":created["secret"]}),
                &headers
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, session, _) = request(
        app,
        "GET",
        "/api/session",
        Value::Null,
        &[("cookie", &login)],
    )
    .await;
    assert_eq!(session["level"], "publish");
    Ok(())
}
