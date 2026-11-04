use std::collections::BTreeMap;

use axum::http::StatusCode;
use rich_studio::build_router;
use rich_studio::settings::Settings;
use rich_studio::store::db::Store;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, state, telegram, upload};

#[tokio::test]
async fn send_passes_the_payload_through_unchanged() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let post = json!({
        "rich_message": {"html":"<h1>Привет</h1><tg-new-tag x='1'>future</tg-new-tag>","is_rtl":false},
        "reply_markup": {"inline_keyboard":[[{"text":"Сайт","url":"https://t.me"}]]},
        "options": {"disable_notification":true,"protect_content":true,"message_effect_id":"123"}
    });
    let reply = call(app, "POST", "/api/send", Some(json!({"post":post})), &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = telegram.last("sendRichMessage").ok_or("sendRichMessage")?;
    assert!(
        sent.path.contains("/bot123:TEST/sendRichMessage"),
        "{}",
        sent.path
    );
    assert_eq!(sent.body["chat_id"], "@test_channel");
    assert_eq!(sent.body["rich_message"], post["rich_message"]);
    assert_eq!(sent.body["reply_markup"], post["reply_markup"]);
    assert_eq!(sent.body["disable_notification"], true);
    assert_eq!(sent.body["protect_content"], true);
    assert_eq!(sent.body["message_effect_id"], "123");
    let record = &reply.value["post"];
    assert_eq!(record["status"], "sent");
    assert_eq!(record["message_id"], reply.value["message"]["message_id"]);
    Ok(())
}

#[tokio::test]
async fn invalid_post_and_unwritable_row_never_reach_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    let invalid = json!({"post":{"rich_message":{"blocks":[{"type":"nope"}]}}});
    assert_eq!(
        call(app.clone(), "POST", "/api/send", Some(invalid), &[])
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(telegram.count("sendRichMessage"), 0);

    Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_insert before insert on posts
         begin select raise(abort, 'database is locked'); end;",
    )?;
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(telegram.count("sendRichMessage"), 0);
    Ok(())
}

#[tokio::test]
async fn numeric_chat_id_is_sent_as_an_integer() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(
            app,
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"chat_id":"-1001234567890"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?.body["chat_id"],
        json!(-1001234567890i64)
    );
    Ok(())
}

#[tokio::test]
async fn upload_is_attached_and_stored_as_upload_reference() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let uploaded = upload(app.clone(), "обложка 1.png", b"PNGDATA", "image/png", &[])
        .await
        .value;
    assert_eq!(uploaded["kind"], "photo");
    let reference = uploaded["ref"].as_str().ok_or("ref")?.to_owned();
    assert!(reference.starts_with("upload:"));
    let post = json!({"rich_message":{"blocks":[
        {"type":"photo","photo":{"type":"photo","media":reference}}
    ]}});
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = telegram.last("sendRichMessage").ok_or("send")?;
    assert!(sent.raw.contains("attach://f0"), "{}", sent.raw);
    assert!(sent.raw.contains("PNGDATA"), "{}", sent.raw);
    let id = reply.value["post"]["id"].as_str().ok_or("post id")?;
    let stored = call(app, "GET", &format!("/api/posts/{id}"), None, &[])
        .await
        .value;
    assert_eq!(
        stored["payload"]["rich_message"]["blocks"][0]["photo"]["media"],
        reference
    );
    Ok(())
}

#[tokio::test]
async fn delete_published_post_sends_telegram_and_marks_it_deleted() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let sent = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await
    .value["post"]
        .clone();
    let id = sent["id"].as_str().ok_or("post id")?;
    let removed = call(app, "DELETE", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
    assert_eq!(removed.value["status"], "deleted");
    assert_eq!(
        telegram.last("deleteMessage").ok_or("delete")?.body["message_id"],
        sent["message_id"]
    );
    Ok(())
}

#[tokio::test]
async fn send_to_several_channels_deduplicates_and_reports_each_one() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b","@a","-1002"]})),
        &[],
    )
    .await
    .value;
    assert_eq!(
        telegram
            .calls_to("sendRichMessage")
            .into_iter()
            .map(|call| call.body["chat_id"].clone())
            .collect::<Vec<_>>(),
        [json!("@a"), json!("@b"), json!(-1002)]
    );
    assert_eq!(
        reply["results"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|result| result["ok"].clone())
            .collect::<Vec<_>>(),
        [json!(true), json!(true), json!(true)]
    );
    Ok(())
}

#[tokio::test]
async fn upload_over_limit_is_refused_and_uploads_are_served_safely() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("MAX_UPLOAD_MB", "0.001")]));
    let before = call(app.clone(), "GET", "/api/media", None, &[])
        .await
        .value;
    let reply = upload(app.clone(), "big.png", &[b'x'; 5000], "image/png", &[]).await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.text
    );
    assert!(
        reply.value["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("МБ"),
        "{}",
        reply.text
    );
    assert_eq!(
        call(app.clone(), "GET", "/api/media", None, &[])
            .await
            .value,
        before
    );

    let html = upload(
        app.clone(),
        "page.html",
        b"<script>alert(1)</script>",
        "text/html",
        &[],
    )
    .await
    .value;
    let name = html["name"].as_str().ok_or("name")?;
    let served = call(app.clone(), "GET", &format!("/media/{name}"), None, &[]).await;
    assert_eq!(
        served
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/octet-stream")
    );
    assert!(
        served
            .headers
            .get("content-disposition")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("attachment")
    );
    assert!(
        served
            .headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .starts_with("sandbox")
    );
    let png = upload(app.clone(), "a.png", b"png", "image/png", &[])
        .await
        .value;
    let png_name = png["name"].as_str().ok_or("name")?;
    let served = call(app, "GET", &format!("/media/{png_name}"), None, &[]).await;
    assert_eq!(
        served
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        served
            .headers
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    Ok(())
}

#[test]
fn upload_limit_follows_own_bot_api_server() -> TestResult {
    let dir = TempDir::new()?;
    let bare = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))?;
    assert_eq!(bare.max_upload_mb, 50.0);
    let own = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), "http://localhost:8081".to_owned()),
    ]))?;
    assert_eq!(own.max_upload_mb, 2000.0);
    let explicit = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("MAX_UPLOAD_MB".to_owned(), "10".to_owned()),
    ]))?;
    assert_eq!(explicit.max_upload_mb, 10.0);
    Ok(())
}

#[tokio::test]
async fn options_cannot_override_core_params() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for key in ["chat_id", "rich_message", "reply_markup"] {
        let mut post = blocks_post();
        post["options"] = json!({key:"@other"});
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":post})),
                &[]
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{key}"
        );
    }
    assert_eq!(telegram.count("sendRichMessage"), 0);
    Ok(())
}
