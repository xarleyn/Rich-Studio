use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use rich_studio::{AppState, build_router, settings::Settings};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn response(uri: &str) -> Result<axum::response::Response, Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let settings = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        directory.path().to_string_lossy().into_owned(),
    )]))?;
    let app = build_router(AppState::new(settings));
    let request = Request::builder()
        .header("host", "localhost")
        .uri(uri)
        .body(Body::empty())?;
    Ok(app.oneshot(request).await?)
}

async fn body_text(
    response: axum::response::Response,
) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok(String::from_utf8(bytes.to_vec())?)
}

#[tokio::test]
async fn liveness_reports_ok() -> TestResult {
    let response = response("/healthz").await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await?, r#"{"ok":true}"#);
    Ok(())
}

#[tokio::test]
async fn index_serves_the_existing_editor() -> TestResult {
    let response = response("/").await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body_text(response)
            .await?
            .contains("RichPost — студия публикаций")
    );
    Ok(())
}

#[tokio::test]
async fn mcp_get_is_rejected_without_opening_a_stream() -> TestResult {
    let response = response("/mcp").await?;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers()
            .get(header::ALLOW)
            .ok_or("missing Allow")?,
        "POST"
    );
    Ok(())
}

#[test]
fn bind_address_defaults_to_the_editor_port() {
    assert_eq!(rich_studio::bind_address(None), "127.0.0.1:7424");
}

#[test]
fn bind_address_accepts_an_explicit_socket_address() {
    assert_eq!(
        rich_studio::bind_address(Some("127.0.0.1:7434".into())),
        "127.0.0.1:7434"
    );
}
