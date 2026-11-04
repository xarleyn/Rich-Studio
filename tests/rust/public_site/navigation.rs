use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::public_site;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{TestResult, app, body_text, get, head};

#[tokio::test]
async fn public_head_and_canonical_slashes_keep_methods_and_queries() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::page_paths() {
        let head = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(&path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(head.status(), StatusCode::OK, "HEAD {path}");
        assert!(
            head.headers()["content-type"]
                .to_str()?
                .starts_with("text/html")
        );
        assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
        if path == "/" {
            continue;
        }
        for query in ["utm_source=a%20b&lang=en", "utm_source=telegram&x=1"] {
            for method in ["GET", "HEAD"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(format!("{}?{query}", path.trim_end_matches('/')))
                            .header("host", "example.com")
                            .body(Body::empty())?,
                    )
                    .await?;
                assert_eq!(
                    response.status(),
                    StatusCode::PERMANENT_REDIRECT,
                    "{method} {path}?{query}"
                );
                assert_eq!(
                    response.headers()["location"],
                    format!("{path}?{query}"),
                    "{method} {path}?{query}"
                );
            }
        }
    }
    for path in ["/robots.txt", "/sitemap.xml", "/favicon.ico"] {
        let head = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(head.status(), StatusCode::OK, "HEAD {path}");
        assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
    }
    let unknown = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/features/unknown")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert!(!unknown.headers().contains_key("location"));
    let write = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/en")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(write.status(), StatusCode::FORBIDDEN);
    assert!(!write.headers().contains_key("location"));
    Ok(())
}

#[tokio::test]
async fn marketing_missing_pages_are_html_without_changing_api_authorization() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in [
        "/missing-page",
        "/mcp-missing",
        "/docs-missing",
        "/features/no-such-guide/",
        "/en/features/no-such-guide/",
        "/missing",
        "/en/missing",
    ] {
        let (status, headers, body) = get(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!headers.contains_key("location"), "{path}");
        assert!(
            headers["content-type"].to_str()?.starts_with("text/html"),
            "{path}"
        );
        assert_eq!(headers["x-robots-tag"], "noindex, nofollow", "{path}");
        assert!(headers.contains_key("content-security-policy"), "{path}");
        let text = body_text(&body)?;
        assert!(text.contains("RichPost"), "{path}");
        assert!(
            !text.contains("rel=\"canonical\"") && !text.contains("__"),
            "{path}"
        );
        assert!(
            text.contains(if path.starts_with("/en/") {
                "lang=\"en\""
            } else {
                "lang=\"ru\""
            }),
            "{path}"
        );
        assert!(text.contains("<a") && text.contains("/features/"), "{path}");
        assert_eq!(text.matches("<h1").count(), 1, "{path}");
        let (head_status, _, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(head_status, StatusCode::NOT_FOUND, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
    }
    let (status, _, body) = get(app.clone(), "/en/no-such-page", "example.com").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body_text(&body)?.starts_with("<!doctype html>\n<html lang=\"en\">"),
        "English fallback keeps its document language"
    );
    let unauthorized = app
        .oneshot(
            Request::builder()
                .uri("/api/unknown")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(
        unauthorized.headers()["content-type"]
            .to_str()?
            .starts_with("application/json")
    );
    Ok(())
}
