use axum::http::StatusCode;
use rich_studio::store::posts;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, http, now, post, served};

#[tokio::test]
async fn draft_remembers_the_post_it_edits() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, draft) = http(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Правка","post":original,"post_id":pid})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["post_id"], pid);
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let (status, updated) = http(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{draft_id}"),
        Some(json!({"title":"Правка","post":post("# Исправлено"),"version":draft["version"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert!(updated["post_id"].is_null());
    let (status, listed) = http(app, "GET", "/api/drafts", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed[0]["post_id"].is_null());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn rest_never_sends_an_edit_copy_as_a_second_post_unless_asked() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@x"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, draft) = http(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"правка","post":blocks_post(),"post_id":pid})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let draft_id = draft["id"].clone();
    for (path, at) in [("/api/send", None), ("/api/schedule", Some(now() + 3600))] {
        for dry in [false, true] {
            let mut body = json!({
                "post":blocks_post(),"chat_id":"@x","draft_id":draft_id,"dry_run":dry
            });
            if let Some(at) = at {
                body["at"] = json!(at);
            }
            let (status, refused) = http(app.clone(), "POST", path, Some(body), &[]).await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert!(
                refused["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&format!("/api/posts/{pid}/edit")),
                "{refused}"
            );
        }
    }
    assert_eq!(posts::list(&state.account)?.len(), 1);
    let (status, copied) = http(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@x","draft_id":draft_id,"as_new":true})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{copied}");
    assert_eq!(posts::list(&state.account)?.len(), 2);
    server.abort();
    Ok(())
}
