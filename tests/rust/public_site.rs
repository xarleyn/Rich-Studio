use std::collections::BTreeMap;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rich_studio::{AppState, api::static_assets, build_router, public_site, settings::Settings};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn app(directory: &TempDir) -> Result<Router, Box<dyn std::error::Error>> {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
        (
            "STUDIO_URL".to_owned(),
            "https://app.example.com".to_owned(),
        ),
        (
            "STUDIO_LANDING_URL".to_owned(),
            "https://example.com".to_owned(),
        ),
        ("TG_BOT_USERNAME".to_owned(), "fixture_bot".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:public-site-secret".to_owned()),
        (
            "STUDIO_API_KEY".to_owned(),
            "private-operator-key".to_owned(),
        ),
    ]))?;
    Ok(build_router(AppState::new(settings)))
}

#[tokio::test]
async fn public_documents_keep_exact_routes_head_metadata_and_configured_destinations() -> TestResult
{
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for host in ["example.com", "app.example.com"] {
        for path in public_site::AGENT_PATHS {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(*path)
                        .header("host", host)
                        .header("x-forwarded-host", "attacker.invalid")
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::OK, "{host}{path}");
            assert_eq!(response.headers()["cache-control"], "public, max-age=300");
            assert!(response.headers().contains_key("link"));
            assert!(!response.headers().contains_key("set-cookie"));
            let body =
                String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
            assert!(
                !body.contains("attacker.invalid")
                    && !body.contains("public-site-secret")
                    && !body.contains("private-operator-key")
            );
            assert!(
                !body.contains("__APP_URL__")
                    && !body.contains("__SITE_URL__")
                    && !body.contains("__TOKEN_STEPS__")
            );
            let head = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("HEAD")
                        .uri(*path)
                        .header("host", host)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(head.status(), StatusCode::OK);
            assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents.json")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    let catalog: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await?)?;
    assert_eq!(catalog["mcp"][0]["url"], "https://app.example.com/mcp");
    for path in [
        "/skills/other/SKILL.md",
        "/agents.md/extra",
        "/features/unknown/",
        "/bundled/agent-docs/setup.md",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let protected = app
        .oneshot(
            Request::builder()
                .uri("/api/drafts")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn marketing_templates_and_sitemap_preserve_languages_and_fixed_inventory() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::page_paths() {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let text = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
        assert!(text.contains("rel=\"describedby\" href=\"/llms.txt\""));
        assert!(!text.contains("__APP_URL__") && !text.contains("__LANDING_URL__"));
        assert!(text.contains(&format!("https://example.com{path}")));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sitemap.xml")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    let text = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
    assert_eq!(public_site::page_paths().len(), 12);
    assert_eq!(text.matches("<url>").count(), 12);
    assert_eq!(text.matches("<lastmod>").count(), 12);
    assert_eq!(text.matches("<xhtml:link").count(), 36);
    assert!(text.contains("xmlns:xhtml=\"http://www.w3.org/1999/xhtml\""));
    for path in public_site::page_paths() {
        assert!(text.contains(&format!("<loc>https://example.com{path}</loc>")));
    }
    assert!(!text.contains("/api/") && !text.contains("/landing") && !text.contains("/agents.md"));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/sitemap.xml")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

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
        for method in ["GET", "HEAD"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!(
                            "{}?utm_source=a%20b&lang=en",
                            path.trim_end_matches('/')
                        ))
                        .header("host", "example.com")
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(
                response.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{method} {path}"
            );
            assert_eq!(
                response.headers()["location"],
                format!("{path}?utm_source=a%20b&lang=en")
            );
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
    for path in ["/missing", "/features/no-such-guide/", "/en/missing"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response.headers()["content-type"]
                .to_str()?
                .starts_with("text/html")
        );
        assert_eq!(response.headers()["x-robots-tag"], "noindex, nofollow");
        assert!(response.headers().contains_key("content-security-policy"));
        let text = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
        assert!(text.contains("RichPost"));
        assert!(!text.contains("rel=\"canonical\"") && !text.contains("__"));
        assert!(text.contains(if path.starts_with("/en/") {
            "lang=\"en\""
        } else {
            "lang=\"ru\""
        }));
        assert!(text.contains("/features/"));
    }
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

#[tokio::test]
async fn public_metadata_is_parseable_hashed_and_not_authorized_by_request_headers() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::page_paths() {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("host", "example.com")
                    .header("x-csp-hash", "attacker")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        let csp = response.headers()["content-security-policy"]
            .to_str()?
            .to_owned();
        assert!(!csp.contains("attacker"));
        let script_policy = csp
            .split(';')
            .find(|part| part.trim_start().starts_with("script-src"))
            .ok_or("missing script-src")?;
        assert!(!script_policy.contains("unsafe-inline") && !script_policy.contains("unsafe-eval"));
        let html = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
        let json = html
            .split_once("<script type=\"application/ld+json\">")
            .and_then(|(_, tail)| tail.split_once("</script>"))
            .map(|(json, _)| json)
            .ok_or("missing structured data")?;
        let hash = STANDARD.encode(Sha256::digest(json.as_bytes()));
        assert!(script_policy.contains(&format!("'sha256-{hash}'")));
        let data: Value = serde_json::from_str(json)?;
        assert_eq!(data["@context"], "https://schema.org");
        let graph = data["@graph"].as_array().ok_or("missing schema graph")?;
        assert!(!graph.is_empty());
        assert!(!html.contains("__STRUCTURED_DATA__") && !html.contains("__SITE_URL__"));
        assert!(html.contains("property=\"og:image\""));
        assert!(html.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(html.contains("/static/images/social-preview.png?v="));
    }
    let app_page = app
        .oneshot(
            Request::builder()
                .uri("/en/features/")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(app_page.status(), StatusCode::OK);
    assert_eq!(app_page.headers()["x-robots-tag"], "noindex, nofollow");
    Ok(())
}

#[tokio::test]
async fn static_cache_requires_actual_file_fingerprint_and_respects_brotli_quality() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let css = std::fs::read("app/static/landing.css")?;
    let fingerprint = rich_studio::hex::encode(&Sha256::digest(&css))[..12].to_owned();
    for (query, immutable) in [
        (String::new(), false),
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
    for (accept, encoded) in [("br", true), ("br;q=0, identity;q=1", false)] {
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
    // ServeDir currently rejects multi-range requests instead of producing a
    // multipart body. Either form must never claim a compressed envelope.
    assert_eq!(multiple.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(!multiple.headers().contains_key("content-encoding"));
    let icon = app
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
        "/static/%2e%2e/secret.css",
        "/static/%2E%2E%2fsecret.css",
        "/static/..%5csecret.css",
        "/static/C:/secret.css",
        "/static/..\\secret.css",
        "/static//asset.css",
        "/static/missing.css",
        "/static/",
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
