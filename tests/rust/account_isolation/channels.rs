use axum::http::StatusCode;
use rich_studio::account_context::AccountContext;
use rich_studio::services::updates;
use rich_studio::store::channels;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, call, connect_channel, fake_server, login_as, ok_envelope, public_config,
};

#[tokio::test]
async fn only_administrators_can_connect_channels() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChatMember",
        vec![
            ok_envelope(json!({"status":"administrator","can_post_messages":true})),
            ok_envelope(json!({"status":"member"})),
        ],
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@someone"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/channels",
            None,
            &[("cookie", &who)]
        )
        .await
        .json(),
        json!([])
    );
    let channel = connect_channel(&app, &who).await;
    assert_eq!(channel["chat_id"], json!("-1001"));

    let other = login_as(&app, 202).await;
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &other)])
            .await
            .json(),
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn bot_needs_admin_posting_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@mine"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &who)])
            .await
            .json(),
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn adding_bot_connects_channel_to_the_actor() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    let second = login_as(&app, 202).await;
    let event = json!({
        "my_chat_member": {
            "from": {"id": 101},
            "chat": {"id": -1001},
            "new_chat_member": {"status": "administrator"}
        }
    });
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("channel_connected")
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/channels",
            None,
            &[("cookie", &first)]
        )
        .await
        .json()
        .as_array()
        .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        call(app, "GET", "/api/channels", None, &[("cookie", &second)])
            .await
            .json(),
        json!([])
    );
    assert!(channels::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}
