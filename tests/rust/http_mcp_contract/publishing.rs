use axum::http::StatusCode;
use rich_studio::store::posts as post_store;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call_fail, call_list, call_ok, request, studio_with_fake,
};

#[tokio::test]
async fn list_posts_reports_status_title_and_failures() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@ok"}),
    )
    .await;
    tg.queue(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"Bad Request: inline keyboard button URL 'https://x' is invalid"}),
    );
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@bad"}),
    )
    .await;
    assert!(text.contains("invalid"), "{text}");
    let rows = call_list(app.clone(), "list_posts", json!({})).await;
    let summary = rows
        .iter()
        .map(|post| {
            (
                post["status"].clone(),
                post["title"].clone(),
                post["pinned"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            (json!("failed"), json!("Тест"), json!(false)),
            (json!("sent"), json!("Тест"), json!(false)),
        ]
    );
    assert_eq!(
        rows[1]["link"],
        format!("https://t.me/ok/{}", rows[1]["message_id"])
    );
    let failed = call_list(app, "list_posts", json!({"status":"failed"})).await;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["chat_id"], "@bad");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid"),
        "{}",
        failed[0]["error"]
    );
    Ok(())
}

#[tokio::test]
async fn agent_moves_and_cancels_scheduled_posts_without_deleting_from_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let record = call_ok(
        app.clone(),
        "schedule_post",
        json!({"post":blocks_post(),"at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    let moved = call_ok(
        app.clone(),
        "reschedule_post",
        json!({"post_id":record["id"],"at":"2099-01-02T09:00:00+03:00"}),
    )
    .await;
    assert!(moved["scheduled_at"].as_i64() > record["scheduled_at"].as_i64());
    let cancelled = call_ok(
        app.clone(),
        "cancel_scheduled_post",
        json!({"post_id":record["id"]}),
    )
    .await;
    assert_eq!(cancelled["status"], "cancelled");
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let text = call_fail(
        app,
        "cancel_scheduled_post",
        json!({"post_id":sent["post"]["id"]}),
    )
    .await;
    assert!(text.contains("sent"), "{text}");
    assert!(tg.calls_to("deleteMessage").is_empty());
    Ok(())
}

#[tokio::test]
async fn schedule_dry_run_creates_nothing() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let report = call_ok(
        app,
        "schedule_post",
        json!({"post":blocks_post(),"at":"2099-01-01T09:00:00+03:00","dry_run":true}),
    )
    .await;
    assert_eq!(report["dry_run"], true);
    assert!(report["at"].as_i64().is_some(), "{report}");
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn publish_without_key_does_not_post_twice() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    for chat in ["@a", "@a", "@b"] {
        call_ok(
            app.clone(),
            "publish_post",
            json!({"post":blocks_post(),"chat_id":chat}),
        )
        .await;
    }
    let sent = tg
        .calls_to("sendRichMessage")
        .into_iter()
        .map(|call| call["body"]["chat_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(sent, vec![json!("@a"), json!("@b")]);
    Ok(())
}

#[tokio::test]
async fn flood_wait_is_reported_with_retry_after() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    tg.queue(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":300}}),
    );
    let (status, error) = request(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
    assert_eq!(error["retry_after"], 300);
    Ok(())
}

#[tokio::test]
async fn channels_are_named_wherever_a_post_goes() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[("TG_DEFAULT_CHAT", "@main")]).await;
    call_ok(app.clone(), "check_channel", json!({"chat_id":"@named"})).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"A","chat_ids":["@named"]}),
    )
    .await;
    assert_eq!(
        draft["goes_to"],
        json!([{"chat_id":"@named","title":"Тест","default":false}])
    );
    let plain = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"B"}),
    )
    .await;
    assert_eq!(
        plain["goes_to"],
        json!([{"chat_id":"@main","title":null,"default":true}])
    );
    let drafts = call_list(app.clone(), "list_drafts", json!({})).await;
    let by_title = drafts
        .iter()
        .filter_map(|draft| {
            draft["title"]
                .as_str()
                .map(|title| (title.to_owned(), draft))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(by_title["A"]["goes_to"][0]["title"], "Тест");
    assert_eq!(by_title["B"]["goes_to"][0]["default"], true);
    let got = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(got["goes_to"][0]["title"], "Тест");
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"],"dry_run":true}),
    )
    .await;
    assert_eq!(dry["requests"][0]["chat_title"], "Тест");
    let sent = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert_eq!(sent["results"][0]["chat_title"], "Тест");
    let later = call_ok(
        app.clone(),
        "schedule_post",
        json!({"post":blocks_post(),"chat_id":"@named","at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    assert_eq!(later["chat_title"], "Тест");
    let rows = call_list(app.clone(), "list_posts", json!({})).await;
    let titles = rows
        .iter()
        .filter_map(|post| post["chat_title"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert_eq!(titles, BTreeSet::from(["Тест".to_owned()]));
    let post = call_ok(app, "get_post", json!({"post_id":rows[0]["id"]})).await;
    assert_eq!(post["chat_title"], "Тест");
    Ok(())
}
