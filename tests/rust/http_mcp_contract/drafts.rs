use axum::http::StatusCode;
use rich_studio::store::{drafts as draft_store, posts as post_store};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, call_fail, call_list, call_ok, request, studio_with_fake,
};

#[tokio::test]
async fn draft_channel_from_save_draft_is_used_by_publish() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс","chat_ids":["@agent_chan"]}),
    )
    .await;
    assert_eq!(draft["chat_ids"], json!(["@agent_chan"]));
    let again = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert_eq!(again["chat_ids"], json!(["@agent_chan"]));
    for (tool, extra) in [
        ("publish_post", json!({})),
        ("schedule_post", json!({"at":"2099-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({
            "draft_id":draft["id"],"version":again["version"],"dry_run":true
        });
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let dry = call_ok(app.clone(), tool, arguments).await;
        assert_eq!(
            dry["requests"][0]["params"]["chat_id"], "@agent_chan",
            "{tool}"
        );
    }
    assert!(tg.all_calls().is_empty());
    let (result, _, text) = call(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":again["version"]}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["chat_id"],
        "@agent_chan"
    );
    call_ok(
        app.clone(),
        "check_channel",
        json!({"chat_id":"@agent_chan"}),
    )
    .await;
    let channels = call_list(app, "list_channels", json!({})).await;
    let titles = channels
        .iter()
        .filter(|channel| channel["chat_id"] == "@agent_chan")
        .map(|channel| channel["title"].clone())
        .collect::<Vec<_>>();
    assert_eq!(titles, vec![json!("Тест")]);
    Ok(())
}

#[tokio::test]
async fn agent_continues_a_draft_without_making_copies() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    let full = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(
        full["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    let new = json!({"rich_message":{"markdown":"# Правка агента"}});
    let updated = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":new,"draft_id":draft["id"],"version":full["version"]}),
    )
    .await;
    assert_eq!(updated["id"], draft["id"]);
    assert_eq!(updated["title"], "Анонс");
    assert_eq!(
        updated["version"].as_i64(),
        full["version"].as_i64().map(|version| version + 1)
    );
    assert_eq!(draft_store::list(&state.account)?.len(), 1);
    let text = call_fail(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":full["version"]}),
    )
    .await;
    assert!(text.contains("изменили"), "{text}");
    let (result, _, _) = call(
        app,
        "save_draft",
        json!({"post":blocks_post(),"draft_id":"nope"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    Ok(())
}

#[tokio::test]
async fn agent_save_keeps_the_link_to_the_edited_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, draft) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Правка","post":blocks_post(),"post_id":pid})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let id = draft["id"].as_str().ok_or("draft id")?;
    call_ok(
        app,
        "save_draft",
        json!({"post":{"rich_message":{"markdown":"x"}},"draft_id":id}),
    )
    .await;
    let saved = draft_store::get(&state.account, id)?.ok_or("draft")?;
    assert_eq!(saved["post_id"], pid);
    Ok(())
}

#[tokio::test]
async fn agent_sends_the_post_once_and_then_works_by_draft_id() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Один раз"}),
    )
    .await;
    assert_eq!(draft["validation"]["ok"], true);
    let preview = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(
        preview["post"]["rich_message"],
        blocks_post()["rich_message"]
    );
    assert_eq!(preview["validation"]["ok"], true);
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"chat_id":"@x","dry_run":true}),
    )
    .await;
    assert_eq!(dry["dry_run"], true);
    assert!(tg.all_calls().is_empty());
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"chat_id":"@x"}),
    )
    .await;
    assert!(text.contains("version"), "{text}");
    assert!(tg.all_calls().is_empty());
    let (result, _, text) = call(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"],"chat_id":"@x"}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["draft_id"], draft["id"]);
    assert_eq!(posts[0]["actor"], "mcp");
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["rich_message"],
        blocks_post()["rich_message"]
    );
    Ok(())
}

#[tokio::test]
async fn publishing_a_draft_changed_after_the_preview_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(app.clone(), "save_draft", json!({"post":blocks_post()})).await;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let version = draft["version"].as_i64().ok_or("version")?;
    draft_store::save(
        &state.account,
        Some(id),
        "",
        &json!({"rich_message":{"markdown":"правка в редакторе"}}),
        Some(version),
        None,
        None,
    )?
    .ok_or("draft was not saved")?;
    for (tool, extra) in [
        ("publish_post", json!({})),
        ("schedule_post", json!({"at":"2030-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({"draft_id":draft["id"],"version":draft["version"]});
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let text = call_fail(app.clone(), tool, arguments).await;
        assert!(text.contains("изменили"), "{tool}: {text}");
    }
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert!(text.contains("одно из двух"), "{text}");
    assert!(tg.all_calls().is_empty());
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn save_draft_reports_validation_errors() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let bad = json!({"rich_message":{"blocks":[{"type":"table","rows":[]}]}});
    let draft = call_ok(app, "save_draft", json!({"post":bad})).await;
    assert_eq!(draft["validation"]["ok"], false);
    assert!(
        draft["validation"]["errors"][0]
            .as_str()
            .unwrap_or_default()
            .contains("cells"),
        "{}",
        draft["validation"]["errors"]
    );
    Ok(())
}
