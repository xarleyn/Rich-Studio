use axum::http::StatusCode;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::*;

#[tokio::test]
async fn media_used_by_waiting_post_or_draft_is_kept_and_only_exact_references_count()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, _) = mock(|_, _, _| ok(json!(true))).await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, _) = upload(app.clone(), "cat.png", b"\x89PNG", "image/png").await;
    assert_eq!(status, StatusCode::OK);
    let photo = json!({"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":"upload:cat.png"}}]}});
    let (status, _) = schedule(
        app.clone(),
        json!({"post":photo,"at":"2099-01-01T09:00:00+03:00","chat_ids":["@test_channel"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Кот","post":photo})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, refused) =
        request_as(app.clone(), "DELETE", "/api/media/cat.png", None, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        refused["detail"]["used_by"],
        json!(["отложенный пост в @test_channel", "черновик «Кот»"])
    );
    let (_, files) = request_as(app.clone(), "GET", "/api/media", None, &[]).await;
    assert!(
        files
            .as_array()
            .into_iter()
            .flatten()
            .any(|file| file["name"] == "cat.png"),
        "{files}"
    );
    let (status, _) = request_as(
        app.clone(),
        "DELETE",
        "/api/media/cat.png?force=true",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, files) = request_as(app.clone(), "GET", "/api/media", None, &[]).await;
    assert_eq!(files, json!([]));

    upload(app.clone(), "dog.png", b"\x89PNG", "image/png").await;
    let (status, _) = request_as(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Другое","post":{"rich_message":{"markdown":"upload:dog.png.bak"}}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request_as(app, "DELETE", "/api/media/dog.png", None, &[])
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}
