use std::collections::BTreeMap;

use axum::http::StatusCode;
use rich_studio::{AppState, build_router, services::housekeeping};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, check, initialize, initialized, json_request, photo, report, settings, telegram,
    telegram_settings, upload,
};

#[tokio::test]
async fn unused_media_is_reported_and_cleaned_up() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = AppState::new(settings);
    let _database = initialize(&state.account)?;
    let app = build_router(state);
    for name in ["draft.png", "sent.png", "old.png", "lonely.png"] {
        upload(app.clone(), name, b"\x89PNG").await?;
    }
    let (status, draft) = json_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"d","post":photo("draft.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let (status, sent) = json_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":photo("sent.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let (status, old) = json_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":photo("old.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{old}");
    let id = old["post"]["id"].as_str().ok_or("post id")?;
    let (status, deleted) =
        json_request(app.clone(), "DELETE", &format!("/api/posts/{id}"), None).await?;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, listed) = json_request(app.clone(), "GET", "/api/media", None).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let used = listed
        .as_array()
        .ok_or("media list")?
        .iter()
        .map(|file| {
            (
                file["name"].as_str().unwrap_or_default().to_owned(),
                file["used"].as_bool().unwrap_or(true),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        used,
        BTreeMap::from([
            ("draft.png".to_owned(), true),
            ("sent.png".to_owned(), true),
            ("old.png".to_owned(), false),
            ("lonely.png".to_owned(), false),
        ])
    );
    let (status, cleaned) = json_request(app.clone(), "POST", "/api/media/cleanup", None).await?;
    assert_eq!(status, StatusCode::OK, "{cleaned}");
    let mut deleted = cleaned["deleted"]
        .as_array()
        .ok_or("deleted")?
        .iter()
        .map(|name| name.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    deleted.sort();
    assert_eq!(deleted, ["lonely.png", "old.png"]);
    let (_, listed) = json_request(app, "GET", "/api/media", None).await?;
    let mut names = listed
        .as_array()
        .ok_or("media list")?
        .iter()
        .map(|file| file["name"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["draft.png", "sent.png"]);
    Ok(())
}

#[tokio::test]
async fn media_of_a_missed_post_is_protected_without_force() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = AppState::new(settings);
    let _database = initialize(&state.account)?;
    let app = build_router(state.clone());
    upload(app.clone(), "later.png", b"\x89PNG").await?;
    let at = housekeeping::now() + 3600;
    let (status, scheduled) = json_request(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":photo("later.png"),"chat_id":"@c","at":at})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{scheduled}");
    let id = scheduled["id"].as_str().ok_or("scheduled id")?;
    assert!(rich_studio::store::scheduler::claim(
        &state.account,
        id,
        i64::MAX
    )?);
    assert!(rich_studio::store::scheduler::missed(
        &state.account,
        id,
        "студия не работала"
    )?);
    let (status, conflict) =
        json_request(app.clone(), "DELETE", "/api/media/later.png", None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["detail"]["used_by"],
        json!(["пропущенный пост в @c"])
    );
    let (status, forced) =
        json_request(app, "DELETE", "/api/media/later.png?force=true", None).await?;
    assert_eq!(status, StatusCode::OK, "{forced}");
    Ok(())
}

#[tokio::test]
async fn unused_uploads_are_counted_and_reported() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    upload(app, "big.png", &vec![b'x'; 2048]).await?;
    let big = 201 * 1024 * 1024;
    std::fs::OpenOptions::new()
        .write(true)
        .open(state.account.media_dir.join("big.png"))?
        .set_len(big)?;
    let report = report(&state).await;
    let storage = check(&report, "storage");
    assert_eq!(storage["status"], "warn");
    assert!(
        storage["action"]
            .as_str()
            .unwrap_or_default()
            .contains("MEDIA_UNUSED_DAYS")
    );
    assert_eq!(
        storage["details"]["media"],
        json!({"files":1,"bytes":big,"unused_files":1,"unused_bytes":big})
    );
    Ok(())
}
