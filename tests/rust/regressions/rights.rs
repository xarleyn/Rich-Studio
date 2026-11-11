//! Channel rights, comments and linked discussions.

use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::updates;
use rich_studio::telegram::TelegramError;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, connect_channel, fake_server, login_as, ok_envelope,
    public_settings,
};

#[tokio::test]
async fn removed_admin_receives_no_new_private_topics() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let state = AppState::new(public_settings(&dir, &base));
    let app = build_router(state.clone());
    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(
            json!({"id":-1001,"type":"supergroup","is_forum":true}),
        )],
    );
    connect_channel(&app, &who).await;
    fake.queue("getChatMember", vec![ok_envelope(json!({"status":"left"}))]);
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"message":{"chat":{"id":-1001},"message_thread_id":77,"forum_topic_created":{"name":"Private new topic"}}}),
        )
        .await?,
        Some("topic")
    );
    let listed = call(
        app,
        "GET",
        "/api/channels",
        None,
        &[("cookie", who.as_str())],
    )
    .await
    .json();
    assert_eq!(listed[0]["topics"], json!([]));
    Ok(())
}

#[tokio::test]
async fn first_comment_requires_current_source_rights_and_linked_discussion() -> TestResult {
    for (allowed, linked) in [(false, true), (true, false), (true, true)] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let mut config = public_settings(&dir, &base);
        config.updates_on = true;
        let state = AppState::new(config);
        let app = build_router(state.clone());
        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        let mut post = blocks_post();
        post["comment"] = json!({"html":"<p>First comment</p>"});
        let sent = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"chat_id":"-1001","post":post})),
            &[("cookie", who.as_str())],
        )
        .await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
        let saved = sent.json()["post"].clone();
        fake.queue(
            "getChatMember",
            vec![ok_envelope(
                json!({"status":if allowed {"creator"} else {"left"}}),
            )],
        );
        fake.queue(
            "getChat",
            vec![ok_envelope(json!({
                "id":-1001,
                "type":"channel",
                "linked_chat_id":if linked {-1002} else {-1003},
            }))],
        );
        fake.clear_calls();
        assert_eq!(
            updates::handle_update(
                &state,
                &json!({
                    "message":{
                        "chat":{"id":-1002},
                        "message_id":900,
                        "is_automatic_forward":true,
                        "forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":saved["message_id"]},
                    }
                }),
            )
            .await?,
            Some("comment")
        );
        assert_eq!(
            fake.count("sendRichMessage"),
            usize::from(allowed && linked),
            "allowed={allowed} linked={linked}"
        );
        let result = call(
            app,
            "GET",
            &format!("/api/posts/{}", saved["id"].as_str().unwrap_or_default()),
            None,
            &[("cookie", who.as_str())],
        )
        .await
        .json();
        assert_eq!(
            result["comment_status"],
            if allowed && linked { "sent" } else { "failed" },
            "allowed={allowed} linked={linked}: {result}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn linked_comment_does_not_change_transport_policy() -> TestResult {
    for refused in [false, true] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        fake.queue(
            "getChat",
            vec![ok_envelope(
                json!({"id":-1001,"type":"channel","linked_chat_id":-1002}),
            )],
        );
        if refused {
            fake.queue(
                "sendRichMessage",
                vec![json!({"ok":false,"error_code":403,"description":"Forbidden"})],
            );
        }
        let context = AccountContext::account(&config, 101, Role::Publish);
        let transport = state.transport();
        let result = transport
            .send_linked_comment(
                &context,
                &json!(-1001),
                &json!(-1002),
                77,
                json!({"html":"Comment"}),
            )
            .await;
        if refused {
            assert!(
                matches!(result, Err(TelegramError::Refused { .. })),
                "{result:?}"
            );
        } else {
            result.unwrap_or_else(|error| panic!("comment: {error}"));
        }
        let error = transport
            .call(
                &context,
                "sendRichMessage",
                json!({"chat_id":-1002,"rich_message":{"html":"Other"}}),
                Uploads::default(),
            )
            .await
            .expect_err("an unlinked chat must stay outside the comment exception");
        assert!(
            matches!(error, TelegramError::Policy(_)),
            "refused={refused}: {error}"
        );
        assert!(error.to_string().contains("привяжите"), "{error}");
    }
    Ok(())
}
