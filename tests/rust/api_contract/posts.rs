use axum::http::StatusCode;
use rich_studio::build_router;
use rich_studio::store::db::Store;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, state, telegram, upload};

#[tokio::test]
async fn telegram_refusal_is_listed_with_chat_and_title() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.fail(
        "sendRichMessage",
        "Bad Request: not enough rights to send rich messages",
    );
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@chan"})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(
        reply.value["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("not enough rights")
    );
    let listed = call(app, "GET", "/api/posts", None, &[]).await;
    let [post] = listed.value.as_array().ok_or("posts")?.as_slice() else {
        panic!("expected one post: {}", listed.text);
    };
    assert_eq!(post["status"], "failed");
    assert!(
        post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not enough rights")
    );
    assert_eq!(post["chat_id"], "@chan");
    assert_eq!(post["title"], "Тест");
    assert_eq!(post["link"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn failed_channel_rows_are_listed_and_filterable() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.fail("sendRichMessage", "Bad Request: chat not found");
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@a","@b"]})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text);
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    assert_eq!(
        rows.as_array()
            .into_iter()
            .flatten()
            .map(|post| post["status"].clone())
            .collect::<Vec<_>>(),
        [json!("failed"), json!("failed")]
    );
    let failed = call(
        app.clone(),
        "GET",
        "/api/posts?status=failed&chat=@a",
        None,
        &[],
    )
    .await
    .value;
    assert_eq!(failed[0]["chat_id"], "@a");
    assert_eq!(
        call(app, "GET", "/api/posts?status=sent", None, &[])
            .await
            .value,
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn successful_draft_resend_drops_only_its_refusal() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    telegram.fail(
        "sendRichMessage",
        "Bad Request: inline keyboard button URL 'https://' is invalid",
    );
    for chat in ["@c", "@other"] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":blocks_post(),"draft_id":draft_id,"chat_id":chat})),
                &[],
            )
            .await
            .status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{chat}"
        );
    }
    telegram.clear_fail();
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"draft_id":draft_id,"chat_id":"@c"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    let pairs = rows
        .as_array()
        .ok_or("posts")?
        .iter()
        .map(|post| {
            (
                post["status"].as_str().unwrap_or_default().to_owned(),
                post["chat_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        pairs,
        [
            ("sent".to_owned(), "@c".to_owned()),
            ("failed".to_owned(), "@other".to_owned())
        ]
    );
    let failed_id = rows[1]["id"].as_str().ok_or("failed id")?;
    let removed = call(
        app.clone(),
        "DELETE",
        &format!("/api/posts/{failed_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(removed.value["status"], "removed");
    assert_eq!(
        call(app, "GET", "/api/posts", None, &[])
            .await
            .value
            .as_array()
            .into_iter()
            .flatten()
            .map(|post| post["status"].clone())
            .collect::<Vec<_>>(),
        [json!("sent")]
    );
    Ok(())
}

#[tokio::test]
async fn failed_row_recording_never_hides_the_telegram_error() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    // The first send creates the posts table the trigger attaches to.
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_failed before update of status on posts
         when NEW.status='failed'
         begin select raise(abort, 'database is locked'); end;",
    )?;
    telegram.fail("sendRichMessage", "Bad Request: chat not found");
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
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert!(reply.text.contains("chat not found"), "{}", reply.text);
    Ok(())
}

#[tokio::test]
async fn posts_list_shows_title_thumb_link_and_hides_payload() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let uploaded = upload(app.clone(), "cover.png", b"\x89PNG fake", "image/png", &[]).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.text);
    let uploaded = uploaded.value;
    let reference = uploaded["ref"].as_str().ok_or("ref")?;
    let html =
        "<figure><img src=\"tg://photo?id=c\"></figure><h1>Анонс &amp; эфир</h1><p>Подробности</p>";
    let rich = json!({"html":html,"media":[{"id":"c","media":{"type":"photo","media":reference}}]});
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":{"rich_message":rich},"chat_id":"@my_chan"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":{"rich_message":{"markdown":"## **Итоги** недели\n\nтекст"}},"chat_id":"-1001234567890"})),
            &[],
        )
        .await
        .status,
        StatusCode::OK
    );
    let rows = call(app, "GET", "/api/posts", None, &[]).await.value;
    let rows = rows.as_array().ok_or("posts")?;
    assert_eq!(rows.len(), 2);
    let (markdown, first) = (&rows[0], &rows[1]);
    assert_eq!(first["title"], "Анонс & эфир");
    assert_eq!(
        first["thumb"],
        format!(
            "/media/{}",
            reference.strip_prefix("upload:").unwrap_or_default()
        )
    );
    assert_eq!(
        first["link"],
        format!(
            "https://t.me/my_chan/{}",
            first["message_id"].as_i64().ok_or("message id")?
        )
    );
    assert_eq!(first["pinned"], false);
    assert!(first.get("payload").is_none());
    assert_eq!(markdown["title"], "Итоги недели");
    assert_eq!(markdown["thumb"], Value::Null);
    assert_eq!(
        markdown["link"],
        format!(
            "https://t.me/c/1234567890/{}",
            markdown["message_id"].as_i64().ok_or("message id")?
        )
    );
    Ok(())
}
