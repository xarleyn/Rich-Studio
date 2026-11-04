use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use rich_studio::services::{scheduler, updates};
use rich_studio::store::{bot_dialog, posts};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn first_comment_with_updates_off_is_reported_not_left_waiting()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, result) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["post"]["comment_status"], "skipped");
    assert!(
        result["warnings"].to_string().contains("TG_UPDATES"),
        "{result}"
    );
    Ok(())
}

#[tokio::test]
async fn network_error_on_first_comment_is_recorded() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler = Arc::clone(&attempts);
    let (base, _) = mock(move |_, method, payload| match method {
        "sendRichMessage" if handler.fetch_add(1, Ordering::SeqCst) == 0 => {
            ok(message(payload, 10))
        }
        "sendRichMessage" => Reply::Drop,
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, sent) = request(
        app,
        "/api/send",
        json!({"post":content,"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let message_id = sent["message"]["message_id"].as_i64().ok_or("message id")?;
    let event = json!({"update_id":9,"message":{"message_id":77,"chat":{"id":-2002},"is_automatic_forward":true,"forward_origin":{"type":"channel","chat":{"id":-1001},"message_id":message_id}}});
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("comment")
    );
    let post = posts::get(&context(&dir), id)?.ok_or("post")?;
    assert_eq!(post["comment_status"], "failed");
    assert!(
        post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("мог уйти"),
        "{post}"
    );
    Ok(())
}

#[tokio::test]
async fn comment_waiting_for_more_than_a_day_expires() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let app = build_router(AppState::new(config));
    let mut content = post();
    content["comment"] = json!({"html":"<p>x</p>"});
    let (status, sent) = request(app, "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let context = context(&dir);
    let now = scheduler::now();
    assert!(
        bot_dialog::expire_comments(&context, now - updates::COMMENT_WAIT, "старое")?.is_empty()
    );
    rich_studio::store::db::Store::connect(&context)?.execute(
        "update posts set sent_at=? where id=?",
        rusqlite::params![now - updates::COMMENT_WAIT - 10, id],
    )?;
    assert_eq!(
        bot_dialog::expire_comments(&context, now - updates::COMMENT_WAIT, "не пришла")?,
        std::slice::from_ref(&id)
    );
    assert_eq!(
        posts::get(&context, &id)?.ok_or("post")?["comment_status"],
        "failed"
    );
    Ok(())
}

#[tokio::test]
async fn preview_buttons_answer_like_in_the_channel_and_old_clicks_still_count()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 20)),
        "answerCallbackQuery" => refused(
            "Bad Request: query is too old and response timeout expired or query ID is invalid",
        ),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.preview_chat = "777".to_owned();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    let (status, preview) = request(
        app.clone(),
        "/api/preview-send",
        json!({"post":content}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let message_id = preview["message_id"].as_i64().ok_or("message id")?;
    assert_eq!(
        updates::handle_update(&state, &callback_update(777, message_id, "like")).await?,
        Some("callback")
    );
    let answer = last_payload(&mut requests, "answerCallbackQuery").ok_or("answerCallbackQuery")?;
    assert_eq!(answer["text"], "Спасибо");

    let (status, sent) = request(
        app,
        "/api/send",
        json!({"post":{"rich_message":{"markdown":"клик"},"reply_markup":button},"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let mid = posts::get(&context(&dir), id)?.ok_or("post")?["message_id"]
        .as_i64()
        .ok_or("message id")?;
    assert_eq!(
        updates::handle_update(&state, &callback_update(-1001, mid, "like")).await?,
        Some("callback")
    );
    let post = posts::get(&context(&dir), id)?.ok_or("post")?;
    assert_eq!(post["clicks"], json!({"like":1}));
    Ok(())
}
