use axum::http::StatusCode;
use rich_studio::store::posts as post_store;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, call_fail, call_ok, request, studio_with_fake,
};

#[tokio::test]
async fn mcp_publish_is_idempotent_and_marked_as_mcp() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    for _ in 0..2 {
        let (result, _, text) = call(
            app.clone(),
            "publish_post",
            json!({"post":blocks_post(),"idempotency_key":"mcp-1"}),
        )
        .await;
        assert_eq!(result["isError"], false, "{text}");
    }
    assert_eq!(tg.calls_to("sendRichMessage").len(), 1);
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["actor"], "mcp");
    assert_eq!(posts[0]["status"], "sent");
    Ok(())
}

#[tokio::test]
async fn mcp_send_poll_is_idempotent() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let poll = json!({"question":"Когда эфир?","options":["Утром","Вечером"]});
    let result = call_ok(
        app.clone(),
        "send_poll",
        json!({"poll":poll,"chat_id":"@c","idempotency_key":"poll-1"}),
    )
    .await;
    assert_eq!(result["results"][0]["ok"], true);
    assert_eq!(
        tg.last("sendPoll").ok_or("sendPoll")?["body"]["question"],
        "Когда эфир?"
    );
    call_ok(
        app,
        "send_poll",
        json!({"poll":poll,"chat_id":"@c","idempotency_key":"poll-1"}),
    )
    .await;
    assert_eq!(tg.calls_to("sendPoll").len(), 1);
    Ok(())
}

#[tokio::test]
async fn mcp_links_point_at_the_editor_and_preview() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Ссылки"}),
    )
    .await;
    let expected_editor = format!(
        "http://localhost:7424/?draft={}",
        draft["id"].as_str().ok_or("draft id")?
    );
    assert_eq!(draft["editor_url"], expected_editor);
    assert_eq!(
        draft["preview_url"],
        format!(
            "http://localhost:7424/render/draft/{}",
            draft["id"].as_str().ok_or("draft id")?
        )
    );
    let shown = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(shown["editor_url"], draft["editor_url"]);
    let got = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(got["editor_url"], draft["editor_url"]);
    let sent = call_ok(app.clone(), "publish_post", json!({"post":blocks_post()})).await;
    let post = call_ok(app, "get_post", json!({"post_id":sent["post"]["id"]})).await;
    assert_eq!(
        post["editor_url"],
        format!(
            "http://localhost:7424/?post={}",
            sent["post"]["id"].as_str().ok_or("post id")?
        )
    );
    Ok(())
}

#[tokio::test]
async fn mcp_publish_dry_run_and_validation_errors() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x","dry_run":true}),
    )
    .await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["requests"][0]["params"]["chat_id"], "@x");
    let bad = json!({"rich_message":{"blocks":[{"type":"heading","text":"x","size":"big"}]}});
    let text = call_fail(app, "publish_post", json!({"post":bad})).await;
    assert!(text.contains("проверку"), "{text}");
    assert!(tg.all_calls().is_empty());
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn mcp_save_draft_preview_path_and_preview_to_me() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    assert_eq!(
        draft["preview_path"],
        format!("/render/draft/{}", draft["id"].as_str().ok_or("draft id")?)
    );
    let (status, page) = request(
        app.clone(),
        "GET",
        draft["preview_path"].as_str().ok_or("preview path")?,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let sent = call_ok(app, "send_preview_to_me", json!({"post":blocks_post()})).await;
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["chat_id"],
        777
    );
    assert!(
        sent["parsed"]["input_blocks"]
            .as_array()
            .is_some_and(|blocks| !blocks.is_empty())
    );
    assert!(sent["parsed"].get("blocks").is_none(), "{sent}");
    Ok(())
}
