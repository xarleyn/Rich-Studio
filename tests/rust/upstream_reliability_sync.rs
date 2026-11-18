#[path = "publication_safety/support.rs"]
#[allow(dead_code)]
mod support;

use axum::http::StatusCode;
use rich_studio::services::{publisher, scheduler};
use rich_studio::store::{self, db, posts};
use rich_studio::telegram::{
    TelegramError,
    uploads::{Uploads, check_uploads},
};
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;

use support::*;

#[tokio::test]
async fn later_storage_refusal_keeps_broadcast_result_and_idempotency()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, payload| ok(message(payload, 91))).await;
    let state = AppState::new(settings(&dir, &base));
    let mut connection = db::Store::connect(&state.account)?;
    db::migrate(&mut connection)?;
    connection.execute_batch("create trigger reject_second before insert on posts when new.chat_id='@two' begin select raise(abort,'full disk'); end;")?;
    let app = build_router(state.clone());
    let body = json!({"post":post(),"chat_ids":["@one","@two"]});
    let (status, first) = request(
        app.clone(),
        "/api/send",
        body.clone(),
        &[("Idempotency-Key", "broadcast")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["results"][0]["ok"], true);
    assert_eq!(first["results"][1]["ok"], false);
    assert!(
        first["results"][1]["error"]
            .as_str()
            .is_some_and(|error| error.contains("не отправлено"))
    );
    let (status, again) =
        request(app, "/api/send", body, &[("Idempotency-Key", "broadcast")]).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, first);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn confirmed_send_with_failed_bookkeeping_is_repaired_without_resending()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, payload| ok(message(payload, 92))).await;
    let state = AppState::new(settings(&dir, &base));
    let mut connection = db::Store::connect(&state.account)?;
    db::migrate(&mut connection)?;
    connection.execute_batch("create trigger reject_sent before update of status on posts when new.status='sent' begin select raise(abort,'full disk'); end;")?;
    let app = build_router(state.clone());
    let body = json!({"post":post(),"chat_id":"@one"});
    let (status, first) = request(
        app.clone(),
        "/api/send",
        body.clone(),
        &[("Idempotency-Key", "unrecorded")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{first}");
    let detail = &first["detail"];
    assert_eq!(detail["published"], true, "{first}");
    assert_eq!(detail["results"][0]["message_id"], 92);
    assert_eq!(posts::list(&state.account)?[0]["status"], "sending");
    assert_eq!(
        rich_studio::services::in_flight::recover(&state.account)?,
        0
    );
    let (blocked, _) = request(
        app.clone(),
        "/api/send",
        body.clone(),
        &[("Idempotency-Key", "new-while-pending")],
    )
    .await;
    assert_eq!(blocked, StatusCode::CONFLICT);
    assert_eq!(posts::list(&state.account)?.len(), 1);
    connection.execute_batch("drop trigger reject_sent;")?;
    publisher::write_unrecorded(&state.account)?;
    assert_eq!(posts::list(&state.account)?[0]["status"], "sent");
    let (status, _) = request(app, "/api/send", body, &[("Idempotency-Key", "unrecorded")]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 1);
    Ok(())
}

#[tokio::test]
async fn uncertain_post_flag_survives_later_success_and_old_health_window()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| Reply::Drop).await;
    let state = AppState::new(settings(&dir, &base));
    let (status, _) = request(
        build_router(state.clone()),
        "/api/send",
        json!({"post":post(),"draft_id":"draft","chat_id":"@one"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let row = posts::list(&state.account)?.remove(0);
    assert_eq!(row["outcome_unknown"], true);
    let id = row["id"].as_str().ok_or("post id")?;
    let connection = db::Store::connect(&state.account)?;
    connection.execute(
        "update posts set error='arbitrary diagnostic',created_at=1 where id=?",
        [id],
    )?;
    assert_eq!(
        posts::drop_refused(&state.account, &json!("@one"), "draft", &[])?,
        0
    );
    let problems = store::stats::problem_posts(&state.account, scheduler::now() - 86400)?;
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0]["outcome_unknown"], true);
    Ok(())
}

#[tokio::test]
async fn callback_reply_is_single_attempt_and_cannot_override_recipient()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, _, _| Reply::Json(429, json!({"ok":false,"error_code":429,"description":"slow down","parameters":{"retry_after":1}}))).await;
    let state = AppState::new(settings(&dir, &base));
    let reply = json!({"rich_message":{"markdown":"private"},"ephemeral_message_parameters":{"replace_callback_query_message":true}});
    let error = state
        .transport()
        .send_callback_reply(&state.account, "pressed", reply, Uploads::default())
        .await
        .expect_err("rate limit");
    assert!(matches!(error, TelegramError::RateLimited { .. }));
    let payloads = sent_payloads(&mut requests, "sendRichMessage");
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0]["ephemeral_message_parameters"]["callback_query_id"],
        "pressed"
    );
    assert_eq!(
        payloads[0]["ephemeral_message_parameters"]["replace_callback_query_message"],
        true
    );
    for reply in [
        json!({"chat_id":"@other","rich_message":{"markdown":"x"}}),
        json!({"allow_paid_broadcast":false,"rich_message":{"markdown":"x"}}),
        json!({"ephemeral_message_parameters":{"receiver_user_id":123},"rich_message":{"markdown":"x"}}),
    ] {
        assert!(matches!(
            state
                .transport()
                .send_callback_reply(&state.account, "pressed", reply, Uploads::default())
                .await,
            Err(TelegramError::Policy(_))
        ));
    }
    assert_eq!(sent_count(&mut requests, "sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn api_reads_reject_cross_site_before_work_even_without_authentication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, "")));
    for method in ["GET", "HEAD"] {
        for site in ["cross-site", "same-site"] {
            let (status, _) = request_as(
                app.clone(),
                method,
                "/api/config",
                None,
                &[("sec-fetch-site", site)],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {site}");
        }
    }
    let (status, _) = request_as(app, "GET", "/api/config", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[test]
fn future_upload_names_cannot_be_paths_and_reply_errors_preserve_literal_keys()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    for name in ["../other", "..\\other", ".hidden", "bad\nname"] {
        assert!(
            check_uploads(
                &json!({"media":format!("upload:{name}")}),
                &context.media_dir,
                false
            )
            .is_err()
        );
    }
    check_uploads(
        &json!({"media":"upload:later.png","text":"upload:../is text"}),
        &context.media_dir,
        false,
    )?;
    let row = posts::add(
        &context,
        posts::NewPost {
            chat_id: &json!("@one"),
            payload: &post(),
            draft_id: None,
            status: "sent",
            message_id: Some(1),
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "test",
        },
    )?;
    let id = row["id"].as_str().ok_or("post id")?;
    let key = "a.\"[button]";
    store::stats::reply_error(&context, id, key, "first")?;
    store::stats::reply_error(&context, id, key, "second")?;
    let row = posts::get(&context, id)?.ok_or("post")?;
    assert_eq!(row["reply_errors"][key]["n"], 2);
    assert_eq!(row["reply_errors"][key]["error"], "second");
    Ok(())
}

#[test]
fn failed_comment_attention_window_starts_when_the_comment_fails()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let row = posts::add(
        &context,
        posts::NewPost {
            chat_id: &json!("@one"),
            payload: &post(),
            draft_id: None,
            status: "sent",
            message_id: Some(1),
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "test",
        },
    )?;
    let id = row["id"].as_str().ok_or("post id")?;
    let connection = db::Store::connect(&context)?;
    connection.execute(
        "update posts set sent_at=1,comment_status='waiting' where id=?",
        [id],
    )?;
    store::bot_dialog::expire_comments(&context, scheduler::now() - 86400, "not forwarded")?;
    let problems = store::stats::problem_posts(&context, scheduler::now() - 86400)?;
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0]["comment_status"], "failed");
    Ok(())
}

#[test]
fn scrub_masks_percent_encoded_configured_secrets_without_lowercasing_values()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir, "");
    settings.bot_token = "123456789:AbCdEf_SecretToken".to_owned();
    for value in [
        "https://api.telegram.org/bot123456789:AbCdEf_SecretToken/getMe",
        "https://api.telegram.org/bot123456789%3AAbCdEf_SecretToken/getMe",
        "https://api.telegram.org/bot123456789%3aAbCdEf_SecretToken/getMe",
        "https://api.telegram.org/bot123456789%253AAbCdEf_SecretToken/getMe",
    ] {
        let scrubbed = rich_studio::logs::scrub(&settings, value);
        assert!(!scrubbed.contains("AbCdEf_SecretToken"), "{scrubbed}");
    }
    let value: Value = json!("ordinary text");
    assert_eq!(
        rich_studio::logs::scrub(&settings, value.as_str().ok_or("text")?),
        "ordinary text"
    );
    Ok(())
}

#[tokio::test]
async fn production_widgets_require_private_chats_but_explicit_alternate_server_keeps_beta_groups()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, "https://api.telegram.org");
    let app = build_router(AppState::new(config.clone()));
    for chat in [json!(-100123), json!("@group")] {
        let (status, body) = request(
            app.clone(),
            "/api/widget",
            json!({"widget":{"html":"<b>tiny</b>"},"chat_id":chat,"at":scheduler::now()+300}),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains("личный чат"));
    }
    config.api_base = "http://127.0.0.1:9".to_owned();
    let (status, body) = request(
        build_router(AppState::new(config)),
        "/api/widget",
        json!({"widget":{"html":"<b>tiny</b>"},"chat_id":-100123,"at":scheduler::now()+300}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scheduled"][0]["status"], "scheduled");
    Ok(())
}

#[test]
fn concurrent_fresh_database_migrations_apply_each_number_exactly_once()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let ready = std::sync::Barrier::new(8);
    std::thread::scope(|threads| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (context, ready) = (&context, &ready);
                threads.spawn(move || {
                    let mut connection = db::Store::connect(context)?;
                    ready.wait();
                    db::migrate(&mut connection)
                })
            })
            .collect();
        for worker in workers {
            worker.join().map_err(|_| "migration worker panicked")??;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    let connection = db::Store::connect(&context)?;
    let version: i64 = connection.query_row("pragma user_version", [], |row| row.get(0))?;
    assert_eq!(version, db::embedded_migrations().len() as i64);
    Ok(())
}

#[tokio::test]
async fn telegram_echoed_configured_secrets_are_scrubbed_before_persistence_and_later_reads()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let key = "configured-api-secret-at-least-thirty-two-characters";
    let echoed = format!("Telegram refusal echo {key}");
    let (base, _) = mock(move |_, _, _| refused(&echoed)).await;
    let mut config = settings(&dir, &base);
    config.api_key = key.to_owned();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let (status, error) = request(
        app.clone(),
        "/api/send",
        json!({"post":post()}),
        &[("x-api-key", key)],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let row = posts::list(&state.account)?.remove(0);
    assert!(!row["error"].as_str().ok_or("stored error")?.contains(key));
    assert!(
        row["error"]
            .as_str()
            .ok_or("stored error")?
            .contains("<secret>")
    );
    for path in ["/api/posts", "/api/health"] {
        let (status, body) =
            request_as(app.clone(), "GET", path, None, &[("x-api-key", key)]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(!body.to_string().contains(key), "{path}: {body}");
    }
    Ok(())
}

#[tokio::test]
async fn comment_activity_metadata_distinguishes_active_abandoned_and_standby_without_cross_account_leaks()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir, ""));
    let row = posts::add(
        &state.account,
        posts::NewPost {
            chat_id: &json!("@one"),
            payload: &post(),
            draft_id: None,
            status: "sent",
            message_id: Some(1),
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "test",
        },
    )?;
    let id = row["id"].as_str().ok_or("post id")?;
    db::Store::connect(&state.account)?
        .execute("update posts set comment_status='sending' where id=?", [id])?;
    let app = build_router(state.clone());
    let active = rich_studio::services::in_flight::comment(&state.account, id)?;
    for (role, expected) in [
        ("leader", json!(true)),
        ("off", json!(true)),
        ("standby", Value::Null),
    ] {
        state.loop_status.write().await.role = role.to_owned();
        let (status, listing) = request_as(app.clone(), "GET", "/api/posts", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(
            listing[0]["comment_in_flight"], expected,
            "list role {role}"
        );
        let (status, detail) =
            request_as(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["comment_in_flight"], expected, "detail role {role}");
    }
    drop(active);
    // The same post id in another account's live registry cannot make this row active.
    let foreign = rich_studio::account_context::AccountContext::account(
        &state.settings,
        123,
        rich_studio::account_context::Role::Owner,
    );
    let _foreign = rich_studio::services::in_flight::comment(&foreign, id)?;
    state.loop_status.write().await.role = "leader".to_owned();
    let (_, listing) = request_as(app.clone(), "GET", "/api/posts", None, &[]).await;
    let (_, detail) = request_as(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(listing[0]["comment_in_flight"], false);
    assert_eq!(detail["comment_in_flight"], false);
    // A standby only lacks knowledge while the database still says sending.
    db::Store::connect(&state.account)?
        .execute("update posts set comment_status='failed' where id=?", [id])?;
    state.loop_status.write().await.role = "standby".to_owned();
    let (_, detail) = request_as(app, "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(detail["comment_in_flight"], false);
    Ok(())
}
