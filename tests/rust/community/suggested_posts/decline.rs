use axum::http::StatusCode;
use rich_studio::{AppState, build_router, services::updates, store::auth};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{dm_suggestion, mock, request, settings};

#[tokio::test]
async fn decline_keeps_the_author_time_and_checks_the_comment()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    updates::handle_update(
        &state,
        &dm_suggestion(20, json!({"state":"pending","send_date":auth::now() + 600})),
    )
    .await?;
    let early = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:20/approve",
        json!({"send_date":auth::now() + 7200}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(early.0, StatusCode::BAD_REQUEST);
    assert!(
        early.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("автор")
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/suggested-posts/-1009:20/decline",
            json!({"comment":"x".repeat(129)}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let declined = request(
        app.clone(),
        "POST",
        "/api/suggested-posts/-1009:20/decline",
        json!({"comment":"Не по теме канала"}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(declined["state"], "declined");
    assert_eq!(declined["comment"], "Не по теме канала");
    assert_eq!(
        mock.last("declineSuggestedPost")["comment"],
        "Не по теме канала"
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/suggested-posts/-1:1/decline",
            json!({}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
    Ok(())
}
