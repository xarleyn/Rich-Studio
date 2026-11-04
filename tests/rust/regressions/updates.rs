//! Incoming Telegram updates for public accounts: revoked admins and preview callbacks.

use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::updates;
use rich_studio::store::{channels, invite_links, posts};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, call, fake_server, login_as, ok_envelope, public_settings, save_sent_post,
};

#[tokio::test]
async fn revoked_account_does_not_receive_reactions_or_block_other_admin() -> TestResult {
    for status in ["left", "member", "administrator", "unavailable"] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        let first = login_as(&app, 101).await;
        let second = login_as(&app, 202).await;
        let mut records = Vec::new();
        for uid in [101, 202] {
            let context = AccountContext::account(&config, uid, Role::Publish);
            channels::save(
                &context,
                &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel"}),
            )?;
            records.push(save_sent_post(&context, -1001, 77));
        }
        fake.queue(
            "getChatMember",
            vec![
                if status == "unavailable" {
                    json!({"ok":false,"error_code":403,"description":"Forbidden"})
                } else {
                    ok_envelope(json!({"status":status,"can_post_messages":false}))
                },
                ok_envelope(json!({"status":"creator"})),
            ],
        );
        assert_eq!(
            updates::handle_update(
                &state,
                &json!({
                    "message_reaction_count":{
                        "chat":{"id":-1001},
                        "message_id":77,
                        "reactions":[{"type":{"type":"emoji","emoji":"👍"},"total_count":3}],
                    }
                }),
            )
            .await?,
            Some("reactions")
        );
        let first_post = call(
            app.clone(),
            "GET",
            &format!(
                "/api/posts/{}",
                records[0]["id"].as_str().unwrap_or_default()
            ),
            None,
            &[("cookie", first.as_str())],
        )
        .await
        .json();
        assert!(
            first_post["reactions"].is_null()
                || first_post["reactions"]
                    .as_object()
                    .is_some_and(|value| value.is_empty()),
            "{status}: {first_post}"
        );
        let second_post = call(
            app,
            "GET",
            &format!(
                "/api/posts/{}",
                records[1]["id"].as_str().unwrap_or_default()
            ),
            None,
            &[("cookie", second.as_str())],
        )
        .await
        .json();
        assert_eq!(second_post["reactions"], json!({"👍":3}), "{status}");
    }
    Ok(())
}

#[tokio::test]
async fn revoked_account_does_not_receive_poll_or_invite_updates() -> TestResult {
    for event in ["poll", "invite"] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let config = public_settings(&dir, &base);
        let state = AppState::new(config.clone());
        let app = build_router(state.clone());
        login_as(&app, 101).await;
        let context = AccountContext::account(&config, 101, Role::Publish);
        channels::save(
            &context,
            &json!({"chat_id":"-1001","chat_num":-1001,"type":"channel"}),
        )?;
        let post = save_sent_post(&context, -1001, 77);
        posts::update_delivery(
            &context,
            post["id"].as_str().unwrap_or_default(),
            Some(-1001),
            Some("private-poll"),
            None,
        )?;
        invite_links::save(&context, "test-invite", "-1001", "ui", &json!({}))?;
        fake.queue("getChatMember", vec![ok_envelope(json!({"status":"left"}))]);
        let update = if event == "poll" {
            json!({"poll":{"id":"private-poll","total_voter_count":3,"options":[]}})
        } else {
            json!({"chat_join_request":{"chat":{"id":-1001},"invite_link":{"invite_link":"test-invite"}}})
        };
        updates::handle_update(&state, &update).await?;
        let saved = posts::get(&context, post["id"].as_str().unwrap_or_default())?
            .unwrap_or_else(|| panic!("post is missing"));
        assert!(
            saved["poll_results"].is_null()
                || saved["poll_results"]
                    .as_object()
                    .is_some_and(|value| value.is_empty()),
            "{event}: {saved}"
        );
        assert_eq!(
            invite_links::get(&context, "test-invite")?
                .unwrap_or_else(|| panic!("invite link is missing"))["requests"],
            json!(0),
            "{event}"
        );
        assert!(fake.count("getChatMember") >= 1, "{event}");
    }
    Ok(())
}

#[tokio::test]
async fn own_dm_preview_callback_needs_no_channel_membership() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let config = public_settings(&dir, &base);
    let state = AppState::new(config.clone());
    let app = build_router(state.clone());
    login_as(&app, 101).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    updates::remember_preview(
        &context,
        &json!(101),
        77,
        &json!({"button":"Private preview answer"}),
    )
    .await?;
    fake.clear_calls();
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"callback_query":{"id":"query","data":"button","message":{"chat":{"id":101},"message_id":77}}}),
        )
        .await?,
        Some("callback")
    );
    let answers = fake.calls_to("answerCallbackQuery");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["body"]["text"], "Private preview answer");
    assert_eq!(fake.count("getChatMember"), 0);
    Ok(())
}
