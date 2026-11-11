use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::api::static_assets;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{TestResult, app, get};

#[tokio::test]
async fn static_cache_requires_actual_file_fingerprint_and_respects_brotli_quality() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let css = std::fs::read("app/static/landing.css")?;
    let fingerprint = rich_studio::hex::encode(&Sha256::digest(&css))[..12].to_owned();
    for (query, immutable) in [
        (String::new(), false),
        ("?v=old".to_owned(), false),
        ("?v=000000000000".to_owned(), false),
        (format!("?v={fingerprint}&extra=1"), false),
        (format!("?v={fingerprint}&v={fingerprint}"), false),
        (format!("?v={fingerprint}"), true),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/static/landing.css{query}"))
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["cache-control"],
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=0, must-revalidate"
            }
        );
        assert_eq!(
            to_bytes(response.into_body(), 1_000_000).await?.as_ref(),
            css
        );
    }
    for (accept, encoded) in [
        ("br", true),
        ("gzip, br", true),
        ("br;q=0, identity;q=1", false),
        ("br;q=0, *;q=1", false),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/landing.css")
                    .header("host", "example.com")
                    .header("accept-encoding", accept)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["vary"], "Accept-Encoding");
        assert!(
            response.headers()["content-type"]
                .to_str()?
                .starts_with("text/css"),
            "{accept}"
        );
        assert_eq!(response.headers().contains_key("content-encoding"), encoded);
        if encoded {
            assert_eq!(response.headers()["content-encoding"], "br");
        }
        let expected = if encoded {
            std::fs::read("app/static/landing.css.br")?
        } else {
            css.clone()
        };
        assert_eq!(
            to_bytes(response.into_body(), 1_000_000).await?.as_ref(),
            expected
        );
    }
    let partial = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/landing.css")
                .header("host", "example.com")
                .header("accept-encoding", "br")
                .header("range", "bytes=0-9")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!partial.headers().contains_key("content-encoding"));
    assert_eq!(
        partial.headers()["content-range"],
        format!("bytes 0-9/{}", css.len())
    );
    assert_eq!(
        to_bytes(partial.into_body(), 1_000_000).await?.as_ref(),
        &css[..10]
    );
    let multiple = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/landing.css")
                .header("host", "example.com")
                .header("accept-encoding", "br")
                .header("range", "bytes=0-9,30-39")
                .body(Body::empty())?,
        )
        .await?;
    // Multipart ranges keep serving the original file and never borrow the
    // Brotli sidecar as a compressed envelope.
    assert_eq!(multiple.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!multiple.headers().contains_key("content-encoding"));
    assert!(
        multiple.headers()["content-type"]
            .to_str()?
            .starts_with("multipart/byteranges")
    );
    let multiple = to_bytes(multiple.into_body(), 1_000_000).await?;
    assert!(multiple.windows(10).any(|window| window == &css[..10]));
    assert!(multiple.windows(10).any(|window| window == &css[30..40]));
    let icon = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/favicon.ico")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(icon.status(), StatusCode::OK);
    assert!(
        to_bytes(icon.into_body(), 1_000_000)
            .await?
            .starts_with(&[0, 0, 1, 0])
    );
    let (status, headers, _) = get(app, "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("immutable")
    );
    Ok(())
}

#[tokio::test]
async fn fingerprints_cannot_read_parent_paths_encoded_traversal_or_missing_assets() -> TestResult {
    let directory = TempDir::new()?;
    let static_dir = directory.path().join("static");
    std::fs::create_dir(&static_dir)?;
    std::fs::write(static_dir.join("asset.css"), "a{}")?;
    std::fs::write(directory.path().join("secret.css"), "private")?;
    let first = static_assets::fingerprint(&static_dir, "/static/asset.css")
        .await
        .ok_or("missing fingerprint")?;
    for path in [
        "/static/../secret.css",
        "/static/../../.env",
        "/static/%2e%2e/secret.css",
        "/static/%2e%2e/%2e%2e/.env",
        "/static/%2E%2E%2fsecret.css",
        "/static/..%5csecret.css",
        "/static/C:/secret.css",
        "/static/..\\secret.css",
        "/static//asset.css",
        "/static/missing.css",
        "/static/%00",
        "/static/",
        "/api/drafts",
        "/other/asset.css",
    ] {
        assert_eq!(
            static_assets::fingerprint(&static_dir, path).await,
            None,
            "{path}"
        );
    }
    std::fs::write(static_dir.join("asset.css"), "b{}")?;
    let second = static_assets::fingerprint(&static_dir, "/static/asset.css")
        .await
        .ok_or("missing changed fingerprint")?;
    assert_ne!(first, second);
    let html = static_assets::version_urls(&static_dir, format!(
        "<link href=\"/static/asset.css\"><link href=\"/static/asset.css?v={first}\"><img src=\"/static/missing.png\">"
    )).await?;
    assert!(html.contains(&format!("/static/asset.css?v={second}")));
    assert!(html.contains(&format!("/static/asset.css?v={first}")));
    assert!(html.contains("/static/missing.png\""));
    Ok(())
}

#[test]
fn checked_in_public_derivatives_match_their_sources() -> TestResult {
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string("app/bundled/public-assets.json")?)?;
    let static_dir = std::path::Path::new("app/static");
    let sha = |path: &str| -> Result<String, Box<dyn std::error::Error>> {
        let bytes = std::fs::read(static_dir.join(path))?;
        Ok(rich_studio::hex::encode(&Sha256::digest(&bytes)))
    };
    for (path, entry) in manifest["compressed"].as_object().ok_or("compressed")? {
        assert_eq!(
            sha(path)?,
            entry["source_sha256"].as_str().ok_or("source_sha256")?,
            "{path}"
        );
        assert_eq!(
            sha(&format!("{path}.br"))?,
            entry["brotli_sha256"].as_str().ok_or("brotli_sha256")?,
            "{path}.br"
        );
    }
    for (path, entry) in manifest["derived"].as_object().ok_or("derived")? {
        assert_eq!(
            sha(entry["source"].as_str().ok_or("source")?)?,
            entry["source_sha256"].as_str().ok_or("source_sha256")?,
            "{path}"
        );
        assert_eq!(
            sha(path)?,
            entry["sha256"].as_str().ok_or("sha256")?,
            "{path}"
        );
    }
    let font = &manifest["font"];
    assert_eq!(
        sha("fonts/manrope.ttf")?,
        font["source_sha256"].as_str().ok_or("font source")?
    );
    let font_sha = sha("fonts/manrope.woff2")?;
    assert_eq!(font_sha, font["sha256"].as_str().ok_or("font")?);
    let css = std::fs::read_to_string(static_dir.join("landing.css"))?;
    assert!(css.contains(&format!("manrope.woff2?v={}\"", &font_sha[..12])));
    Ok(())
}
