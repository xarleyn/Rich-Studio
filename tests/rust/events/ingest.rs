use axum::http::StatusCode;
use rich_studio::{
    AppState, build_router,
    services::updates,
    settings::Settings,
    store::{meta, posts},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn webhook_secret_strict_id_and_persistent_claim_prevent_duplicate_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    config.updates_mode = "webhook".to_owned();
    config.webhook_secret = "verified-secret".to_owned();
    let state = AppState::new(config.clone());
    let post = add(
        &state.account,
        11,
        7,
        json!({"rich_message":{"markdown":"Hi"},"callbacks":{"a":"answer"}}),
    );
    let app = build_router(state.clone());
    let update = callback(8, 11, 7, "a");
    assert_eq!(
        request(app.clone(), update.clone(), None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(app.clone(), update.clone(), Some("wrong")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            json!({"update_id":true}),
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(app.clone(), update.clone(), Some("verified-secret"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            build_router(AppState::new(config.clone())),
            update,
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("answerCallbackQuery"), 1);
    assert_eq!(
        posts::get(&state.account, post["id"].as_str().ok_or("id")?)?.ok_or("post")?["clicks"]["a"],
        1
    );
    mock.mode.store(1, Ordering::SeqCst);
    let failed = callback(9, 11, 7, "a");
    assert_eq!(
        request(app.clone(), failed.clone(), Some("verified-secret"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app, failed, Some("verified-secret")).await.0,
        StatusCode::OK
    );
    assert_eq!(mock.count("answerCallbackQuery"), 2);
    assert_eq!(updates::public_status(&state).await["mode"], "webhook");
    config.updates_mode = "polling".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            json!({"update_id":10}),
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
    Ok(())
}

#[test]
fn webhook_configuration_requires_secret_and_https_public_url() {
    let config = |pairs: &[(&str, &str)]| {
        Settings::from_map(
            &pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    assert!(
        config(&[
            ("TG_UPDATES_MODE", "webhook"),
            ("STUDIO_URL", "https://example.org"),
        ])
        .is_err()
    );
    assert!(
        config(&[
            ("TG_UPDATES_MODE", "webhook"),
            ("TG_WEBHOOK_SECRET", "valid"),
            ("STUDIO_URL", "http://example.org"),
        ])
        .is_err()
    );
    let valid = config(&[
        ("TG_UPDATES_MODE", "webhook"),
        ("TG_WEBHOOK_SECRET", "valid"),
        ("STUDIO_URL", "https://example.org"),
    ])
    .unwrap_or_else(|error| panic!("settings: {error}"));
    assert_eq!(valid.updates_mode, "webhook");
}

#[tokio::test]
async fn polling_persists_offset_before_failed_handler_and_webhook_registration_is_exact()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    config.webhook_secret = "hook-secret".to_owned();
    let state = AppState::new(config);
    let post = add(
        &state.account,
        11,
        1,
        json!({"rich_message":{"markdown":"Hi"},"callbacks":{"x":"value"}}),
    );
    *mock.updates.lock().unwrap_or_else(|e| panic!("lock: {e}")) =
        json!([callback(44, 11, 1, "x")]);
    mock.mode.store(1, Ordering::SeqCst);
    updates::poll_once(&state).await?;
    updates::poll_once(&state).await?;
    assert_eq!(
        meta::get(&state.account, updates::OFFSET_KEY)?.as_deref(),
        Some("45")
    );
    assert_eq!(mock.count("answerCallbackQuery"), 1);
    assert_eq!(
        posts::get(&state.account, post["id"].as_str().ok_or("id")?)?.ok_or("post")?["clicks"]["x"],
        1
    );
    updates::register_webhook(&state).await?;
    let registration = mock
        .calls()
        .into_iter()
        .find(|(method, _)| method == "setWebhook")
        .ok_or("registration")?
        .1;
    assert_eq!(
        registration,
        json!({"url":"https://studio.example/api/telegram/webhook","secret_token":"hook-secret","max_connections":1,"allowed_updates":updates::ALLOWED_UPDATES})
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn webhook_loop_registers_once_with_exact_payload_and_stops()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    config.updates_mode = "webhook".to_owned();
    config.webhook_secret = "test_secret-123".to_owned();
    config.public_url = "https://example.org".to_owned();
    let state = AppState::new(config);
    let stopping = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(updates::run(state, stopping.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while mock.count("setWebhook") == 0 {
            mock.changed.notified().await;
        }
    })
    .await?;
    stopping.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    let registrations: Vec<Value> = mock
        .calls()
        .into_iter()
        .filter(|(method, _)| method == "setWebhook")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(
        registrations,
        [
            json!({"url":"https://example.org/api/telegram/webhook","secret_token":"test_secret-123","allowed_updates":updates::ALLOWED_UPDATES,"max_connections":1})
        ]
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn webhook_deduplicates_bot_commands() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config);
    let mut update = private_ru(11, "/start");
    update["update_id"] = json!(42);
    updates::receive_webhook(&state, &update).await?;
    updates::receive_webhook(&state, &update).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}
