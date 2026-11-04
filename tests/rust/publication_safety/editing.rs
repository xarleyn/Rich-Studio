use axum::http::StatusCode;
use rich_studio::services::updates;
use rich_studio::store::{editing, posts, scheduler as scheduler_store};
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn editing_a_sent_post_keeps_callbacks_and_comment_and_answers_clicks()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mut requests) = mock(|_, method, payload| match method {
        "sendRichMessage" | "editMessageText" => ok(message(payload, 10)),
        _ => ok(json!(true)),
    })
    .await;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    content["comment"] = json!({"html":"<p>Ссылки</p>"});
    let (status, sent) = request(
        app.clone(),
        "/api/send",
        json!({"post":content,"chat_id":-1001}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let new = json!({"rich_message":{"markdown":"# Исправлено"},"reply_markup":button});
    let (status, changed) = request(app, &format!("/api/posts/{id}/edit"), new.clone(), &[]).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["rich_message"], new["rich_message"]);
    assert_eq!(changed["payload"]["callbacks"], json!({"like":"Спасибо"}));
    assert_eq!(
        changed["payload"]["comment"],
        json!({"html":"<p>Ссылки</p>"})
    );

    let click = callback_update(
        -1001,
        changed["message_id"].as_i64().ok_or("message id")?,
        "like",
    );
    assert_eq!(
        updates::handle_update(&state, &click).await?,
        Some("callback")
    );
    let answer = last_payload(&mut requests, "answerCallbackQuery").ok_or("answerCallbackQuery")?;
    assert_eq!(answer["text"], "Спасибо");
    Ok(())
}

#[tokio::test]
async fn repeated_edits_are_not_modified_and_only_real_changes_add_a_version()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, method, payload| match method {
        "sendRichMessage" => ok(message(payload, 10)),
        "editMessageText" | "editMessageReplyMarkup" => Reply::Json(
            400,
            json!({"ok":false,"error_code":400,"description":"Bad Request: message is not modified: specified new message content and reply markup are exactly the same"}),
        ),
        _ => ok(json!(true)),
    })
    .await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let button = json!({"inline_keyboard":[[{"text":"👍","callback_data":"like"}]]});
    let mut content = post();
    content["reply_markup"] = button.clone();
    content["callbacks"] = json!({"like":"Спасибо"});
    let (status, sent) = request(app.clone(), "/api/send", json!({"post":content}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, _) = request(
        app.clone(),
        &format!("/api/posts/{id}/edit"),
        content.clone(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request_as(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}/versions"),
            None,
            &[]
        )
        .await
        .1,
        json!([])
    );
    let only_answers = json!({"rich_message":content["rich_message"],"reply_markup":button,"callbacks":{"like":"Голос учтён"}});
    let (status, changed) = request(
        app.clone(),
        &format!("/api/posts/{id}/edit"),
        only_answers,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        changed["payload"]["callbacks"],
        json!({"like":"Голос учтён"})
    );
    assert_eq!(
        request_as(
            app.clone(),
            "GET",
            &format!("/api/posts/{id}/versions"),
            None,
            &[]
        )
        .await
        .1
        .as_array()
        .map(Vec::len),
        Some(1)
    );

    let mut bare = post();
    bare["reply_markup"] = button.clone();
    let (status, sent) = request(app.clone(), "/api/send", json!({"post":bare}), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let bare_id = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, buttons) = request(
        app,
        &format!("/api/posts/{bare_id}/buttons"),
        json!({"reply_markup":button,"callbacks":{"like":"Спасибо"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{buttons}");
    assert_eq!(buttons["payload"]["callbacks"], json!({"like":"Спасибо"}));
    Ok(())
}

#[tokio::test]
async fn edit_is_refused_once_the_scheduler_has_claimed_the_post()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, record) = schedule(
        app.clone(),
        json!({"post":post(),"at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let context = context(&dir);
    assert!(scheduler_store::claim(&context, &id, 4070930401)?);
    let (status, error) = request(
        app,
        &format!("/api/posts/{id}/edit"),
        json!({"rich_message":{"markdown":"новое"}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("sending"), "{error}");
    let old = record["payload"].clone();
    assert!(!editing::replace_scheduled(
        &context,
        &id,
        &json!({"rich_message":{"markdown":"новое"}}),
        &old,
        "api",
        record["version"].as_i64().unwrap_or_default(),
    )?);
    assert_eq!(
        posts::get(&context, &id)?.ok_or("post")?["payload"]["rich_message"],
        old["rich_message"]
    );
    assert!(editing::versions(&context, &id)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn stale_and_deleted_draft_saves_are_refused_with_current_state()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, draft) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":{"v":1}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let version = draft["version"].as_i64().ok_or("version")?;
    let (status, tab1) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":2},"version":version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tab1["version"], version + 1);
    let (status, stale) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":3},"version":version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["detail"]["current"]["payload"], json!({"v":2}));
    let (status, mine) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title":"A","post":{"v":3}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mine["payload"], json!({"v":3}));
    assert_eq!(
        mine["version"],
        tab1["version"].as_i64().unwrap_or_default() + 1
    );

    let (status, gone) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":{"v":1}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let gone_id = gone["id"].as_str().ok_or("draft id")?.to_owned();
    let gone_version = gone["version"].as_i64().unwrap_or_default();
    assert_eq!(
        request_as(
            app.clone(),
            "DELETE",
            &format!("/api/drafts/{gone_id}"),
            None,
            &[],
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, conflict) = request_as(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{gone_id}"),
        Some(json!({"title":"A","post":{"v":2},"version":gone_version})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(conflict["detail"]["current"].is_null());
    assert_eq!(
        request_as(app, "GET", &format!("/api/drafts/{gone_id}"), None, &[])
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}
