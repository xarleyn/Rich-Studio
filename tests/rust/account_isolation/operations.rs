use std::collections::BTreeSet;
use std::io::{Cursor, Read as _};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::{scheduler, updates};
use rich_studio::store::{auth, posts};
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, connect_channel, fake_server, login_as, ok_envelope,
    public_config, upload,
};

#[tokio::test]
async fn user_operation_right_is_required_even_when_bot_has_it() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    connect_channel(&app, &who).await;
    let context = AccountContext::account(&settings, 101, Role::Publish);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());

    for (method, right) in [
        ("createChatInviteLink", "can_invite_users"),
        ("editMessageText", "can_edit_messages"),
        ("deleteMessage", "can_delete_messages"),
        ("pinChatMessage", "can_edit_messages"),
    ] {
        fake.queue(
            "getChatMember",
            vec![ok_envelope(
                json!({"status":"administrator","can_post_messages":true}),
            )],
        );
        let error = transport
            .call(
                &context,
                method,
                json!({"chat_id":-1001}),
                Uploads::default(),
            )
            .await
            .expect_err("missing user right");
        assert!(
            error.to_string().contains(right),
            "{method}: expected {right}, got {error}"
        );

        let mut member = json!({"status":"administrator","can_post_messages":true});
        member[right] = json!(true);
        fake.queue("getChatMember", vec![ok_envelope(member)]);
        transport
            .call(
                &context,
                method,
                json!({"chat_id":-1001}),
                Uploads::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("{method}: {error}"));
        assert_eq!(fake.last(method)["body"]["chat_id"], json!(-1001));
    }
    Ok(())
}

#[tokio::test]
async fn operator_endpoints_and_logs_are_unavailable_to_users() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    for path in ["/api/logs", "/api/export"] {
        assert_eq!(
            call(app.clone(), "GET", path, None, &[("cookie", &who)])
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    for path in ["/api/raw/getMe", "/api/spec/refresh"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                path,
                Some(json!({})),
                &[("cookie", &who)]
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let health = call(app.clone(), "GET", "/api/health", None, &[("cookie", &who)])
        .await
        .json();
    assert_eq!(health["metrics"], json!({}));
    assert_eq!(health["recent_problems"], json!([]));
    let names: BTreeSet<String> = health["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|check| check["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "scheduler".to_owned(),
            "posts".to_owned(),
            "sends".to_owned(),
            "spec".to_owned(),
        ])
    );
    let mcp = call(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"studio_logs","arguments":{}}})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(mcp.json()["result"]["isError"], json!(true));
    Ok(())
}

#[tokio::test]
async fn scheduler_handles_accounts_and_revoked_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    connect_channel(&app, &first).await;
    let second = login_as(&app, 202).await;
    connect_channel(&app, &second).await;
    let at = auth::now();
    for who in [&first, &second] {
        let reply = call(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"chat_id":"-1001","post":blocks_post(),"at":at})),
            &[("cookie", who)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }
    fake.queue(
        "getChatMember",
        vec![ok_envelope(json!({"status":"member"}))],
    );
    assert_eq!(
        scheduler::run_all(&state, Arc::new(AtomicBool::new(false))).await?,
        2
    );
    let first_posts = call(
        app.clone(),
        "GET",
        "/api/posts",
        None,
        &[("cookie", &first)],
    )
    .await
    .json();
    assert_eq!(first_posts[0]["status"], json!("failed"));
    let second_posts = call(app, "GET", "/api/posts", None, &[("cookie", &second)])
        .await
        .json();
    assert_eq!(second_posts[0]["status"], json!("sent"));
    assert!(posts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn telegram_events_reach_only_the_owning_account() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let state = AppState::new(settings.clone());
    let app = build_router(state.clone());

    let first = login_as(&app, 101).await;
    connect_channel(&app, &first).await;
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"chat_id":"-1001","post":blocks_post()})),
        &[("cookie", &first)],
    )
    .await
    .json();
    let post = sent["post"].clone();
    let id = post["id"].as_str().expect("post id");
    let message_id = post["message_id"].as_i64().expect("message id");
    let second = login_as(&app, 202).await;

    let event = json!({
        "message_reaction_count": {
            "chat": {"id": -1001},
            "message_id": message_id,
            "reactions": [{"type": {"type": "emoji", "emoji": "👍"}, "total_count": 3}],
        }
    });
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("reactions")
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}"),
            None,
            &[("cookie", &first)]
        )
        .await
        .json()["reactions"],
        json!({"👍":3})
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/posts",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert!(posts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn partial_send_keeps_success_and_idempotency_after_rights_are_revoked() -> TestResult {
    for first_revoked in [false, true] {
        let dir = TempDir::new()?;
        let (base, fake) = fake_server().await;
        let settings = public_config(&dir, &base);
        let app = build_router(AppState::new(settings));

        let who = login_as(&app, 101).await;
        connect_channel(&app, &who).await;
        fake.queue(
            "getChat",
            vec![ok_envelope(
                json!({"id":-1002,"type":"channel","title":"Второй"}),
            )],
        );
        let second = call(
            app.clone(),
            "POST",
            "/api/channels",
            Some(json!({"chat_id":"@second"})),
            &[("cookie", &who)],
        )
        .await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.text());

        let mut members = vec![
            ok_envelope(json!({"status":"administrator","can_post_messages":true})),
            ok_envelope(json!({"status":"member"})),
        ];
        if first_revoked {
            members.reverse();
        }
        fake.queue("getChatMember", members);
        let headers = [("cookie", who.as_str()), ("idempotency-key", "partial")];
        let body = json!({"post":blocks_post(),"chat_ids":["-1001","-1002"]});
        let response = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(body.clone()),
            &headers,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text());
        let first = response.json();
        let oks: Vec<bool> = first["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|result| result["ok"].as_bool())
            .collect();
        assert_eq!(
            oks,
            if first_revoked {
                vec![false, true]
            } else {
                vec![true, false]
            }
        );
        let sent = fake.count("sendRichMessage");
        assert_eq!(sent, 1);
        let replay = call(app, "POST", "/api/send", Some(body), &headers).await;
        assert_eq!(replay.json(), first);
        assert_eq!(fake.count("sendRichMessage"), 1);
    }
    Ok(())
}

#[tokio::test]
async fn operator_export_includes_account_databases_and_media() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let who = login_as(&app, 101).await;
    let created = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Мой пост","post":blocks_post()})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    assert_eq!(
        upload(app.clone(), &who, "mine.txt", b"mine").await.status,
        StatusCode::OK
    );
    auth::add_token(&settings, "owner", "owner-secret", "admin")?;
    let exported = call(
        app,
        "GET",
        "/api/export",
        None,
        &[("authorization", "Bearer owner-secret")],
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK, "{}", exported.text());
    let mut archive = zip::ZipArchive::new(Cursor::new(exported.bytes))?;
    let names: BTreeSet<String> = archive
        .file_names()
        .map(|name| name.map(|name| name.into_owned()))
        .collect::<Result<_, _>>()?;
    for expected in [
        "studio.db",
        "accounts/101/studio.db",
        "accounts/101/media/mine.txt",
    ] {
        assert!(names.contains(expected), "{expected}: {names:?}");
    }
    let mut media = Vec::new();
    archive
        .by_name("accounts/101/media/mine.txt")?
        .read_to_end(&mut media)?;
    assert_eq!(media, b"mine");
    Ok(())
}
