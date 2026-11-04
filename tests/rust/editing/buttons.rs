use axum::http::StatusCode;
use rich_studio::store::posts;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, add, config, http, now, post, served};

#[tokio::test]
async fn http_buttons_reject_non_object_callbacks_before_any_telegram_call()
-> Result<(), Box<dyn std::error::Error>> {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt as _;
    let dir = TempDir::new()?;
    let config = config(&dir);
    let state = rich_studio::AppState::new(config);
    let record = add(&state.account, "sent", &post("Old"));
    let path = format!("/api/posts/{}/buttons", record["id"].as_str().ok_or("id")?);
    let app = rich_studio::build_router(state.clone());
    for callbacks in [json!(1), json!([])] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"reply_markup":null,"callbacks":callbacks}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(state.metrics.snapshot()["telegram_calls"], json!({}));
    assert_eq!(
        posts::get(&state.account, record["id"].as_str().ok_or("id")?)?.ok_or("post")?["version"],
        0
    );
    Ok(())
}

#[tokio::test]
async fn scheduled_buttons_are_a_version_and_wait_for_the_scheduler() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":post("пост"),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[("x-studio-client", "ui")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["version"], 1);
    assert_eq!(changed["payload"]["reply_markup"], keyboard);
    let (status, versions) = http(
        app.clone(),
        "GET",
        &format!("/api/posts/{id}/versions"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let before = versions[0].clone();
    assert!(
        before["payload"]
            .get("reply_markup")
            .is_none_or(Value::is_null)
    );
    assert_eq!(before["actor"], "ui");
    let (status, same) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(same["version"], 1);
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger studio_scheduler_claim after update of version on posts
         when new.version = old.version + 1 and old.status = 'scheduled'
         begin update posts set status = 'sending' where id = new.id; end;",
    )?;
    let (status, refused) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":null})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("уже отправляется"),
        "{refused}"
    );
    let stored = posts::get(&state.account, &id)?.ok_or("post")?;
    assert_eq!(stored["payload"]["reply_markup"], keyboard);
    assert_eq!(stored["version"], 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn refused_buttons_give_the_version_back() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post("пост")})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let stored = posts::get(&state.account, &id)?.ok_or("post")?;
    assert_eq!(stored["version"], 0);
    assert!(
        stored["payload"]
            .get("reply_markup")
            .is_none_or(Value::is_null)
    );
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions, json!([]));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn buttons_cut_off_mid_request_may_have_changed() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post("пост")})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(3, std::sync::atomic::Ordering::SeqCst);
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, uncertain) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uncertain}");
    assert!(
        uncertain["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Кнопки могли смениться"),
        "{uncertain}"
    );
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["version"],
        1
    );
    server.abort();
    Ok(())
}
