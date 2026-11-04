use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::{api::static_assets, public_site};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{TestResult, app, get, get_with, head};

#[tokio::test]
async fn pwa_assets_are_public_and_linked_from_editor() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in ["/api/drafts", "/media/private.png"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "app.example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    let worker = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sw.js")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(worker.status(), StatusCode::OK);
    assert_eq!(worker.headers()["cache-control"], "no-cache");
    assert_eq!(worker.headers()["service-worker-allowed"], "/");
    assert!(
        worker.headers()["content-type"]
            .to_str()?
            .contains("javascript")
    );
    assert!(!to_bytes(worker.into_body(), 1_000_000).await?.is_empty());

    let manifest = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/manifest.webmanifest")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(manifest.status(), StatusCode::OK);
    let manifest: Value =
        serde_json::from_slice(&to_bytes(manifest.into_body(), 1_000_000).await?)?;
    assert_eq!(manifest["id"], "/");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["scope"], "/");
    assert_eq!(manifest["display"], "standalone");
    let icons = manifest["icons"].as_array().ok_or("icons")?;
    assert!(
        icons
            .iter()
            .filter(|icon| icon["purpose"] == "any")
            .filter_map(|icon| icon["sizes"].as_str())
            .any(|size| size == "192x192")
    );
    assert!(icons.iter().any(|icon| icon["purpose"] == "maskable"));
    for icon in icons {
        let src = icon["src"].as_str().ok_or("icon src")?;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(src)
                    .header("host", "app.example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{src}");
        let bytes = to_bytes(response.into_body(), 1_000_000).await?;
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{src}");
        let width = u32::from_be_bytes(bytes[16..20].try_into()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into()?);
        assert_eq!(format!("{width}x{height}"), icon["sizes"], "{src}");
    }
    let apple = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/icons/apple-touch-icon.png")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(apple.status(), StatusCode::OK);

    let offline = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/offline.html")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(offline.status(), StatusCode::OK);
    let offline = String::from_utf8(to_bytes(offline.into_body(), 1_000_000).await?.to_vec())?;
    assert!(offline.contains("Нет подключения"));
    assert!(offline.contains("You're offline"));
    assert!(!offline.contains("<script"));

    let editor = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(editor.status(), StatusCode::OK);
    let editor = String::from_utf8(to_bytes(editor.into_body(), 1_000_000).await?.to_vec())?;
    assert!(editor.contains("rel=\"manifest\" href=\"/static/manifest.webmanifest\""));
    assert!(editor.contains("rel=\"apple-touch-icon\""));
    assert!(!editor.contains("<rs-install"));
    assert!(editor.contains("rel=\"describedby\" href=\"/llms.txt\" type=\"text/markdown\""));
    Ok(())
}

#[tokio::test]
async fn icons_discovery_head_and_fingerprint_cache_are_usable() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let mut paths = vec!["/favicon.ico", "/robots.txt", "/sitemap.xml"];
    paths.extend_from_slice(public_site::AGENT_PATHS);
    for path in paths {
        let (status, headers, _) = get(app.clone(), path, "example.com").await?;
        let (head_status, head_headers, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(head_status, status, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
        assert_eq!(
            head_headers["content-type"], headers["content-type"],
            "{path}"
        );
    }
    let (status, _, favicon) = get(app.clone(), "/favicon.ico", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(favicon.starts_with(b"\x00\x00\x01\x00"));

    let static_dir = rich_studio::runtime::static_dir();
    let asset = "/static/landing.css";
    let digest = static_assets::fingerprint(&static_dir, asset)
        .await
        .ok_or("missing fingerprint")?;
    let source = std::fs::read("app/static/landing.css")?;
    for (query, immutable) in [
        (String::new(), false),
        ("?v=old".to_owned(), false),
        (format!("?v={digest}"), true),
        (format!("?v={digest}&v={digest}"), false),
    ] {
        let (status, headers, body) = get_with(
            app.clone(),
            &format!("{asset}{query}"),
            "example.com",
            &[("accept-encoding", "identity")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(
            headers["cache-control"],
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=0, must-revalidate"
            },
            "{query}"
        );
        assert_eq!(body, source, "{query}");
    }
    for path in [
        "/static/../../.env",
        "/static/%2e%2e/%2e%2e/.env",
        "/api/drafts",
        "/static/%00",
    ] {
        assert_eq!(
            static_assets::fingerprint(&static_dir, path).await,
            None,
            "{path}"
        );
    }
    for encoding in ["identity", "br"] {
        let (status, _, _) = get_with(
            app.clone(),
            "/static/%00",
            "example.com",
            &[("accept-encoding", encoding)],
        )
        .await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{encoding}");
    }
    Ok(())
}
