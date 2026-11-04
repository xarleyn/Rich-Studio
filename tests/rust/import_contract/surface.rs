use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    mcp_tools::tools::imports as import_tools,
    services::{
        imports,
        media::{MediaError, MediaService, public_address},
    },
    settings::Settings,
    store::{auth, db, drafts},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr},
};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::support::*;

#[tokio::test]
async fn private_link_without_session_and_invalid_link_fail_before_network() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.import_api_id = 1;
    settings.import_api_hash = "fixture".into();
    settings.import_proxy = "socks5://127.0.0.1:1".into();
    let context = AccountContext::account(&settings, 111, Role::Owner);
    let private = imports::import_post(&settings, &context, "https://t.me/c/12345/203")
        .await
        .err()
        .ok_or("private succeeded")?;
    assert_eq!(private.status, 403);
    assert!(private.to_string().contains("собственн"), "{private}");
    assert_eq!(
        imports::import_post(&settings, &context, "http://127.0.0.1/secret")
            .await
            .err()
            .ok_or("invalid succeeded")?
            .status,
        422
    );
    Ok(())
}

#[tokio::test]
async fn http_status_and_input_contract_work_behind_real_security_guard() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    let app = rich_studio::api::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/imports/telegram")
                .header("host", "localhost")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await?)?;
    assert_eq!(body["mtproto_configured"], false);
    assert_eq!(body["own_session"], false);
    for body in [
        json!({"url":"https://t.me.evil.org/a/1"}),
        json!({"url":"https://evil.example/a"}),
        json!({}),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/imports/telegram")
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    Ok(())
}

#[tokio::test]
async fn import_requires_draft_permission() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    auth::add_token(&state.settings, "reader", "read-secret", "read")?;
    let app = rich_studio::build_router(state);
    let request = Request::builder()
        .uri("/api/imports/telegram")
        .header("host", "localhost")
        .header("authorization", "Bearer read-secret")
        .body(Body::empty())?;
    assert_eq!(app.clone().oneshot(request).await?.status(), StatusCode::OK);
    for uri in ["/api/imports/telegram", "/api/imports/forward"] {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("authorization", "Bearer read-secret")
            .body(Body::from(
                json!({"url":"https://t.me/simple_agi/203"}).to_string(),
            ))?;
        assert_eq!(
            app.clone().oneshot(request).await?.status(),
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_import_uses_the_same_service_and_stays_unpublished() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    assert_eq!(
        import_tools::call(state.clone(), "telegram_import_status", json!({})).await?,
        imports::status(&state.settings, &state.account)
    );
    // A live MTProto/widget read is the only success path and needs Telegram. The offline
    // contract is that MCP returns exactly the service validation result and creates nothing.
    for url in ["https://t.me/c/12345/203", "https://t.me.evil.org/a/1"] {
        let expected = imports::import_post(&state.settings, &state.account, url)
            .await
            .err()
            .ok_or("service accepted")?
            .to_string();
        let error = import_tools::call(state.clone(), "import_telegram_post", json!({"url":url}))
            .await
            .err()
            .ok_or("mcp accepted")?;
        assert_eq!(error, expected, "{url}");
    }
    db::migrate(&mut db::Store::connect(&state.account)?)?;
    assert!(drafts::list(&state.account)?.is_empty());
    let count: i64 =
        db::Store::connect(&state.account)?
            .query_row("select count(*) from posts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn import_media_always_denies_private_addresses() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.media_fetch_private = true;
    assert!(settings.media_fetch_private);
    assert!(public_address(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], "public.example").is_err());
    let context = AccountContext::root(&settings);
    let service = MediaService::new(context.clone(), 50.0, 500.0);
    let error = service
        .fetch("http://127.0.0.1/secret", None, true, true)
        .await
        .err()
        .ok_or("accepted private address")?;
    assert!(matches!(error, MediaError::BadRequest(_)), "{error}");
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    Ok(())
}

#[test]
fn import_configuration_is_optional_and_validated() -> TestResult {
    let dir = TempDir::new()?;
    let base = || BTreeMap::from([("DATA_DIR".into(), dir.path().to_string_lossy().into_owned())]);
    let optional = Settings::from_map(&base())?;
    assert_eq!(optional.import_api_id, 0);
    assert!(optional.import_api_hash.is_empty());
    let mut id_only = base();
    id_only.insert("TG_IMPORT_API_ID".into(), "123".into());
    let error = Settings::from_map(&id_only)
        .err()
        .ok_or("accepted import id without hash")?;
    assert!(error.to_string().contains("задаются вместе"), "{error}");
    let mut proxy = base();
    proxy.insert("TG_IMPORT_PROXY_URL".into(), "file:///secret".into());
    let error = Settings::from_map(&proxy)
        .err()
        .ok_or("accepted file proxy")?;
    assert!(error.to_string().contains("TG_IMPORT_PROXY_URL"), "{error}");
    Ok(())
}
