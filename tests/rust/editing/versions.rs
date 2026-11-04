use axum::http::StatusCode;
use rich_studio::store::posts;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call_count, http, now, post, served};

#[tokio::test]
async fn every_replacement_keeps_the_previous_content_and_actor() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post": original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let new = post("# Исправлено");
    let (status, first) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(new.clone()),
        &[("x-studio-client", "mcp:claude")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(post("третья")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let versions = versions.as_array().ok_or("versions")?;
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["payload"]["rich_message"], new["rich_message"]);
    assert_eq!(
        versions[1]["payload"]["rich_message"],
        original["rich_message"]
    );
    assert_eq!(versions[1]["actor"], "mcp:claude");
    assert!(
        versions[0]["replaced_at"].as_i64().ok_or("replaced_at")?
            >= versions[1]["replaced_at"].as_i64().ok_or("replaced_at")?
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn edit_by_message_id_also_keeps_a_version() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post": original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let sent = sent["post"].clone();
    let id = sent["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        "/api/edit",
        Some(json!({"post":post("# Исправлено"),"message_id":sent["message_id"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let versions = versions.as_array().ok_or("versions")?;
    assert_eq!(versions.len(), 1);
    assert_eq!(
        versions[0]["payload"]["rich_message"],
        original["rich_message"]
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduled_post_replacement_keeps_a_version() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(post("# Исправлено")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions.as_array().map(Vec::len), Some(1));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn versions_of_unknown_post_is_404() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, body) = http(app, "GET", "/api/posts/nope/versions", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn edit_based_on_an_older_version_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let original = blocks_post();
    let new = post("# Исправлено");
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let sent = sent["post"].clone();
    let id = sent["id"].as_str().ok_or("post id")?.to_owned();
    assert_eq!(sent["version"], 0);
    let (status, first) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["version"], 1);
    let edits = call_count(&mock, "editMessageText");
    let (status, late) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(post("чужая")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{late}");
    assert!(
        late["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("версия 1"),
        "{late}"
    );
    assert_eq!(call_count(&mock, "editMessageText"), edits);
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["payload"]["rich_message"],
        new["rich_message"]
    );
    let (status, second) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(original),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["version"], 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn refused_edit_leaves_the_version_alone() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let new = post("# Исправлено");
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{refused}");
    assert_ne!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["version"],
        0
    );
    mock.mode.store(0, std::sync::atomic::Ordering::SeqCst);
    let (status, retry) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retry}");
    assert_eq!(retry["version"], 1);
    server.abort();
    Ok(())
}
