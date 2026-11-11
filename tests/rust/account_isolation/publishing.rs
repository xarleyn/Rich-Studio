use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::store::auth;
use rich_studio::telegram::TelegramError;
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, connect_channel, fake_server, login_as, ok_envelope,
    public_config,
};

#[tokio::test]
async fn public_publish_cannot_use_operator_default_or_unlinked_chat() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let at = auth::now() + 120;
    for (path, body) in [
        ("/api/send", json!({"post":blocks_post()})),
        ("/api/edit", json!({"post":blocks_post(),"message_id":100})),
        ("/api/schedule", json!({"post":blocks_post(),"at":at})),
        ("/api/preview-send", json!({"post":blocks_post()})),
    ] {
        let mut body = body;
        body["chat_id"] = json!("@stranger");
        assert_eq!(
            call(app.clone(), "POST", path, Some(body), &[("cookie", &who)])
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert!(
        !fake
            .methods()
            .iter()
            .any(|method| method.starts_with("send") || method.starts_with("edit"))
    );
    Ok(())
}

#[tokio::test]
async fn send_rechecks_rights_and_uses_numeric_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"-1001"})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        fake.last("sendRichMessage")["body"]["chat_id"],
        json!(-1001)
    );

    fake.clear_calls();
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"chat_id":"-1001"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn preview_goes_to_the_signed_in_user() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    let reply = call(
        app,
        "POST",
        "/api/preview-send",
        Some(json!({"post":blocks_post()})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fake.last("sendRichMessage")["body"]["chat_id"], json!(101));
    Ok(())
}

#[tokio::test]
async fn cross_account_reply_source_is_refused() -> TestResult {
    for path in ["/api/send", "/api/preview-send"] {
        for encoded in [false, true] {
            let dir = TempDir::new()?;
            let (base, fake) = fake_server().await;
            let settings = public_config(&dir, &base);
            let app = build_router(AppState::new(settings));

            let who = login_as(&app, 101).await;
            connect_channel(&app, &who).await;
            let parameters = json!({"chat_id":-100999,"message_id":1});
            let reply = if encoded {
                json!(parameters.to_string())
            } else {
                parameters
            };
            let mut post = blocks_post();
            post["options"] = json!({"reply_parameters":reply});
            fake.clear_calls();
            let response = call(
                app,
                "POST",
                path,
                Some(json!({"post":post,"chat_id":"-1001"})),
                &[("cookie", &who)],
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{path} encoded={encoded}: {}",
                response.text()
            );
            assert!(
                !fake
                    .methods()
                    .iter()
                    .any(|method| method.starts_with("send")),
                "{path} encoded={encoded}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn reply_source_guard_applies_to_every_transport_method() -> TestResult {
    let dir = TempDir::new()?;
    let settings = public_config(&dir, "http://127.0.0.1:9");
    let context = AccountContext::account(&settings, 101, Role::Publish);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    for method in ["sendPoll", "sendLivePhoto", "sendPaidMedia"] {
        let error = transport
            .call(
                &context,
                method,
                json!({"chat_id":101,"reply_parameters":{"chat_id":-100999}}),
                Uploads::default(),
            )
            .await
            .expect_err("cross-account reply source");
        assert!(
            matches!(&error, TelegramError::Policy(_)),
            "{method}: {error}"
        );
        assert!(error.to_string().contains("привяжите"), "{method}: {error}");
    }
    Ok(())
}

#[tokio::test]
async fn reply_to_own_linked_channel_is_allowed_and_rechecks_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(json!({
            "id":-1001,"type":"channel","title":"Мой","username":"mine"
        }))],
    );
    connect_channel(&app, &who).await;

    for source in [json!(-1001), json!("@mine")] {
        let mut post = blocks_post();
        post["options"] = json!({"reply_parameters":{"chat_id":source,"message_id":1}});
        fake.clear_calls();
        let reply = call(
            app.clone(),
            "POST",
            "/api/preview-send",
            Some(json!({"post":post.clone()})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(
            fake.last("sendRichMessage")["body"]["reply_parameters"]["chat_id"],
            json!(-1001)
        );

        fake.queue(
            "getChatMember",
            vec![ok_envelope(json!({"status":"member"}))],
        );
        fake.clear_calls();
        let refused = call(
            app.clone(),
            "POST",
            "/api/preview-send",
            Some(json!({"post":post})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(fake.count("sendRichMessage"), 0);
    }
    Ok(())
}

#[tokio::test]
async fn paid_broadcast_is_refused_before_sending() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let mut post = blocks_post();
    post["options"] = json!({"allow_paid_broadcast":true});
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":post,"chat_id":"-1001"})),
            &[("cookie", &who)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fake.count("sendRichMessage"), 0);
    Ok(())
}
