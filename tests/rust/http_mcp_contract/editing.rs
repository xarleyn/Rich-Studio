use axum::http::StatusCode;
use rich_studio::store::{drafts as draft_store, posts as post_store};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, call_fail, call_list, call_ok, request, studio_with_fake,
};

#[tokio::test]
async fn agent_reads_a_post_before_editing_and_sees_history() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let post = call_ok(app.clone(), "get_post", json!({"post_id":pid})).await;
    assert_eq!(
        post["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    assert_eq!(post["status"], "sent");
    call_ok(
        app.clone(),
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Новое"}}}),
    )
    .await;
    let history = call_list(app, "post_history", json!({"post_id":pid})).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["actor"], "mcp");
    assert_eq!(
        history[0]["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    Ok(())
}

#[tokio::test]
async fn draft_editing_a_published_post_never_goes_out_as_a_new_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x"}),
    )
    .await;
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    let original = posts[0]["id"].as_str().ok_or("post id")?.to_owned();
    let draft = draft_store::save(
        &state.account,
        None,
        "правка",
        &json!({"rich_message":{"markdown":"новый текст"}}),
        None,
        Some(&original),
        None,
    )?
    .ok_or("draft")?;
    for (tool, extra) in [
        ("publish_post", json!({"chat_id":"@x"})),
        ("schedule_post", json!({"at":"2030-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({"draft_id":draft["id"],"version":draft["version"]});
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let text = call_fail(app.clone(), tool, arguments).await;
        assert!(text.contains("edit_post"), "{tool}: {text}");
        assert!(text.contains(&original), "{tool}: {text}");
    }
    let after = post_store::list(&state.account)?;
    assert_eq!(after.len(), 1);
    assert_eq!(tg.calls_to("sendRichMessage").len(), 1);
    let preview = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(preview["version"], draft["version"]);
    let (result, _, text) = call(
        app.clone(),
        "edit_post",
        json!({"post_id":original,"draft_id":draft["id"],"version":preview["version"]}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(
        tg.last("editMessageText").ok_or("editMessageText")?["body"]["rich_message"],
        json!({"markdown":"новый текст"})
    );
    let after = post_store::list(&state.account)?;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["id"], original);
    let stored = post_store::get(&state.account, &original)?.ok_or("post")?;
    assert_eq!(
        stored["payload"]["rich_message"],
        json!({"markdown":"новый текст"})
    );
    Ok(())
}

#[tokio::test]
async fn edit_post_by_draft_checks_the_version_and_the_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x"}),
    )
    .await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@y"}),
    )
    .await;
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 2);
    let one = posts[0]["id"].as_str().ok_or("first post")?.to_owned();
    let two = posts[1]["id"].as_str().ok_or("second post")?.to_owned();
    let draft = draft_store::save(
        &state.account,
        None,
        "правка",
        &blocks_post(),
        None,
        Some(&one),
        None,
    )?
    .ok_or("draft")?;
    let text = call_fail(
        app.clone(),
        "edit_post",
        json!({"post_id":two,"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert!(text.contains(&one), "{text}");
    let text = call_fail(
        app.clone(),
        "edit_post",
        json!({"post_id":one,"draft_id":draft["id"]}),
    )
    .await;
    assert!(text.contains("version"), "{text}");
    assert!(tg.calls_to("editMessageText").is_empty());
    Ok(())
}

#[tokio::test]
async fn edit_post_with_a_stale_post_version_is_a_tool_error() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let post = call_ok(app.clone(), "get_post", json!({"post_id":pid})).await;
    let done = call_ok(
        app.clone(),
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Моё"}},"post_version":post["version"]}),
    )
    .await;
    assert_eq!(
        done["version"].as_i64(),
        post["version"].as_i64().map(|version| version + 1)
    );
    let text = call_fail(
        app,
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Чужое"}},"post_version":post["version"]}),
    )
    .await;
    assert!(text.contains("get_post"), "{text}");
    Ok(())
}
