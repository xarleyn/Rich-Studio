#[path = "api_contract/support.rs"]
#[allow(dead_code)]
mod support;

use axum::http::StatusCode;
use rich_studio::{build_router, store};
use serde_json::json;
use tempfile::TempDir;

use support::*;

#[tokio::test]
async fn checking_a_channel_requires_post() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let response = call(app.clone(), "GET", "/api/check", None, &[]).await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(telegram.calls().is_empty());
    let response = call(app, "POST", "/api/check", Some(json!({})), &[]).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    assert_eq!(response.value["can_post"], true);
    Ok(())
}

#[tokio::test]
async fn diagnostic_redaction_keeps_normal_user_payloads_intact() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _) = telegram().await;
    let token = "123456:abcdefghijklmnopqrstuvwxyz_1234567890";
    let app = build_router(state(&dir, &base, &[("TG_BOT_TOKEN", token)]));
    let response = call(
        app.clone(),
        "GET",
        &format!("/api/spec/type/{token}"),
        None,
        &[],
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(!response.text.contains(token), "{}", response.text);
    assert!(response.text.contains("<secret>"));

    let post =
        json!({"rich_message":{"html":format!("<p>{token}</p>")},"future_field":{"body":"kept"}});
    let response = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text);
    let id = response.value["id"].as_str().ok_or("missing draft id")?;
    let response = call(app, "GET", &format!("/api/drafts/{id}"), None, &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.value["payload"]["future_field"],
        json!({"body":"kept"})
    );
    assert!(response.text.contains(token));
    Ok(())
}

#[tokio::test]
async fn validation_uses_repeated_query_channels_from_the_current_account() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _) = telegram().await;
    let state = state(&dir, &base, &[("TG_DEFAULT_CHAT", "@channel")]);
    for (id, kind) in [("@channel", "channel"), ("7", "private")] {
        store::channels::save(
            &state.account,
            &json!({"chat_id":id,"type":kind,"title":id}),
        )?;
    }
    let app = build_router(state);
    let summary = call(app.clone(), "GET", "/api/spec", None, &[]).await;
    let fields = summary.value["post"]["types"]["Post"]["fields"]
        .as_array()
        .ok_or("post fields")?;
    for (name, kind) in [("pin", "Boolean"), ("callbacks", "Object")] {
        assert!(
            fields
                .iter()
                .any(|field| field["name"] == name && field["types"] == json!([kind])),
            "{}",
            summary.text
        );
    }
    assert!(
        summary.value["post"]["types"]["PostOptions"]["fields"]
            .as_array()
            .is_some_and(|fields| fields.iter().all(|field| field["name"] != "chat_id"))
    );
    let post = json!({"rich_message":{"html":"<p>Post</p>"},"reply_markup":{"inline_keyboard":[[{"text":"Open","web_app":{"url":"https://example.com"}}]]}});
    let private = call(
        app.clone(),
        "POST",
        "/api/validate?chat_ids=7",
        Some(post.clone()),
        &[],
    )
    .await;
    assert_eq!(private.status, StatusCode::OK, "{}", private.text);
    assert!(
        !private.value["warnings"].to_string().contains("web_app"),
        "{}",
        private.text
    );
    let channel = call(
        app.clone(),
        "POST",
        "/api/validate?chat_ids=7&chat_ids=%40channel",
        Some(post.clone()),
        &[],
    )
    .await;
    assert!(
        channel.value["warnings"].to_string().contains("web_app"),
        "{}",
        channel.text
    );
    let default = call(app, "POST", "/api/validate", Some(post), &[]).await;
    assert_eq!(default.value["warnings"], channel.value["warnings"]);
    Ok(())
}

#[tokio::test]
async fn keyboard_edits_preserve_in_post_and_orphan_answers_and_validate_new_replies() -> TestResult
{
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let post = json!({
        "rich_message":{"html":"<p>Vote</p><tg-button-row><tg-button type=callback_data data=vote>Vote</tg-button></tg-button-row>"},
        "reply_markup":{"inline_keyboard":[[{"text":"Like","callback_data":"like"}]]},
        "callbacks":{"vote":"In-text answer","like":"Old keyboard","ghost":"Kept orphan"}
    });
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
    let id = sent.value["post"]["id"].as_str().ok_or("post id")?;
    let path = format!("/api/posts/{id}/buttons");
    let changed = call(
        app.clone(),
        "POST",
        &path,
        Some(json!({
            "reply_markup":{"inline_keyboard":[[{"text":"Like","callback_data":"like"}]]},
            "callbacks":{"like":"New keyboard"}
        })),
        &[],
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    assert_eq!(
        changed.value["payload"]["callbacks"],
        json!({"vote":"In-text answer","like":"New keyboard","ghost":"Kept orphan"})
    );
    let before = telegram.calls_to("editMessageReplyMarkup").len();
    let rejected = call(
        app,
        "POST",
        &path,
        Some(json!({
            "reply_markup":{"inline_keyboard":[[{"text":"Like","callback_data":"like"}]]},
            "callbacks":{"like":{"reply":{"rich_message":{"html":""}}}}
        })),
        &[],
    )
    .await;
    assert_eq!(
        rejected.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        rejected.text
    );
    assert_eq!(telegram.calls_to("editMessageReplyMarkup").len(), before);
    Ok(())
}
