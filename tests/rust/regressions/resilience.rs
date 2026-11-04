//! Health reporting and idempotency when the account database is full.

use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::errors::{ServiceError, StoreError};
use rich_studio::metrics::Metrics;
use rich_studio::services::accounts;
use rich_studio::services::idempotency::{Idempotency, STALE_PENDING};
use rich_studio::store::channels;
use rich_studio::store::db::Store;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, connect_channel, fake_server, login_as, public_settings,
    settings, telegram_check,
};

#[tokio::test]
async fn healthcheck_works_behind_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, "http://127.0.0.1:9");
    config.api_key = "secret".to_owned();
    let app = build_router(AppState::new(config));
    assert_eq!(
        call(app.clone(), "GET", "/healthz", None, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(app, "GET", "/api/config", None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn health_error_order_does_not_depend_on_clock_resolution() -> TestResult {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, "http://127.0.0.1:9");
    config.updates_on = true;
    config.background = false;
    let state = AppState::new(config);
    let metrics = Metrics::default();
    metrics.telegram_call("getMe", true, None, 0, None, false);
    metrics.telegram_call("getUpdates", false, Some("ConnectError"), 0, None, true);
    assert_eq!(telegram_check(&state, &metrics).await["status"], "warn");
    metrics.telegram_call("getUpdates", true, None, 0, None, true);
    assert_eq!(telegram_check(&state, &metrics).await["status"], "ok");
    Ok(())
}

#[tokio::test]
async fn database_full_after_a_send_never_frees_idempotency_key() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let state = AppState::new(config.clone());
    let app = build_router(state.clone());
    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    let connection = Store::connect(&context)?;
    connection.execute_batch(
        "create table acceptance_audit(payload blob);
         create trigger full_after_send before update of status on posts
         when NEW.status='sent' begin
           insert into acceptance_audit values(zeroblob(2097152));
         end;",
    )?;
    drop(connection);
    let headers = [
        ("cookie", who.as_str()),
        ("idempotency-key", "quota-after-send"),
    ];
    let body = json!({"chat_id":"-1001","post":blocks_post()});
    let first = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(body.clone()),
        &headers,
    )
    .await;
    assert_eq!(
        first.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        first.text()
    );
    Store::connect(&context)?.execute_batch("drop trigger full_after_send")?;
    let retried = call(app, "POST", "/api/send", Some(body), &headers).await;
    assert_eq!(
        retried.status,
        StatusCode::BAD_GATEWAY,
        "{}",
        retried.text()
    );
    assert!(retried.text().contains("мог уйти"), "{}", retried.text());
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn real_database_full_recording_send_result_leaves_key_pending() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let mut config = public_settings(&dir, &base);
    config.account_database_mb = 1.0;
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    channels::save(
        &context,
        &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel","can_post":1,"rights":{}}),
    )?;
    let transport = state.transport();
    let action_context = context.clone();
    let action_transport = transport.clone();
    let result = Idempotency::new(context.clone())
        .run(Some("full-result"), "send", &json!({}), "api", move || {
            let context = action_context.clone();
            let transport = action_transport.clone();
            async move {
                transport
                    .call(
                        &context,
                        "sendRichMessage",
                        json!({"chat_id":-1001,"rich_message":{"html":"Once"}}),
                        Uploads::default(),
                    )
                    .await
                    .map_err(ServiceError::Telegram)?;
                Ok(json!({"response":"x".repeat(2*1024*1024)}))
            }
        })
        .await;
    assert!(
        matches!(result, Err(ServiceError::Store(StoreError::StorageFull))),
        "{result:?}"
    );
    let connection = Store::connect(&context)?;
    let status: String = connection.query_row(
        "select status from idempotency where key='full-result'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(status, "pending");
    let conflict = Idempotency::new(context.clone())
        .run(Some("full-result"), "send", &json!({}), "api", || async {
            panic!("a fresh pending key must not run its action again")
        })
        .await;
    assert!(
        matches!(conflict, Err(ServiceError::Conflict(_))),
        "{conflict:?}"
    );
    connection.execute(
        "update idempotency set created_at=created_at-? where key='full-result'",
        [STALE_PENDING + 1],
    )?;
    let stale = Idempotency::new(context)
        .run(Some("full-result"), "send", &json!({}), "api", || async {
            panic!("a stale pending key must not run its action again")
        })
        .await;
    assert!(
        matches!(stale, Err(ServiceError::MayHaveGone(_))),
        "{stale:?}"
    );
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}
