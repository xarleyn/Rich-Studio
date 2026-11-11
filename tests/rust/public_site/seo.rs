use std::collections::BTreeSet;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rich_studio::public_site;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{TestResult, app, app_with, between, body_text, get, head, seo_pages};

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
    let entries = text
        .split("<url>")
        .skip(1)
        .filter_map(|chunk| chunk.split("</url>").next())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), public_site::page_paths().len());
    let date_format = time::format_description::parse_borrowed::<3>("[year]-[month]-[day]")?;
    let today = time::OffsetDateTime::now_utc().date();
    let hreflang = regex::Regex::new(r#"hreflang="([^"]+)""#)?;
    let mut locations = BTreeSet::new();
    for entry in entries {
        let location = between(entry, "<loc>", "</loc>")?;
        locations.insert(location.to_owned());
        let lastmod = between(entry, "<lastmod>", "</lastmod>")?;
        let modified = time::Date::parse(lastmod, &date_format)?;
        assert!(modified <= today, "{lastmod}");
        let alternates = hreflang
            .captures_iter(entry)
            .filter_map(|captures| captures.get(1))
            .map(|matched| matched.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            alternates,
            BTreeSet::from(["ru", "en", "x-default"]),
            "{location}"
        );
    }
    let expected = public_site::page_paths()
        .into_iter()
        .map(|path| format!("https://example.com{path}"))
        .collect::<BTreeSet<_>>();
    assert_eq!(locations, expected);
    let pages = seo_pages()?;
    assert_eq!(
        pages.keys().cloned().collect::<BTreeSet<_>>(),
        public_site::page_paths()
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    for location in &locations {
        let path = location
            .strip_prefix("https://example.com")
            .ok_or("sitemap host")?;
        let (status, headers, body) = get(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(!headers.contains_key("x-robots-tag"), "{path}");
        let html = body_text(&body)?;
        assert!(
            html.contains(&format!("<link rel=\"canonical\" href=\"{location}\">")),
            "{path}"
        );
        assert_eq!(html.matches("<h1").count(), 1, "{path}");
        assert!(
            html.contains("hreflang=\"ru\"") && html.contains("hreflang=\"en\""),
            "{path}"
        );
        assert!(!html.contains("__APP_URL__"), "{path}");
        assert!(
            headers["content-security-policy"]
                .to_str()?
                .contains("script-src 'self'"),
            "{path}"
        );
        let (head_status, head_headers, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(head_status, StatusCode::OK, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
        assert_eq!(
            head_headers["content-type"], headers["content-type"],
            "{path}"
        );
        assert_eq!(
            head_headers["content-length"], headers["content-length"],
            "{path}"
        );
    }
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
async fn public_metadata_is_parseable_hashed_and_not_authorized_by_request_headers() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let pages = seo_pages()?;
    let placeholders = regex::Regex::new(r"__[A-Z_]+__")?;
    let social_preview = regex::Regex::new(
        r#"<meta property="og:image" content="https://example\.com/static/images/social-preview\.png\?v=[a-f0-9]{12}">"#,
    )?;
    for path in public_site::page_paths() {
        let entry = pages.get(&path).ok_or("SEO entry")?;
        let title = entry["title"].as_str().ok_or("SEO title")?;
        let description = entry["description"].as_str().ok_or("SEO description")?;
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
        assert!(html.contains(&format!("<title>{title}</title>")), "{path}");
        assert!(
            html.contains(&format!(
                "<meta name=\"description\" content=\"{description}\">"
            )),
            "{path}"
        );
        assert_eq!(
            html.matches("<script type=\"application/ld+json\">")
                .count(),
            1,
            "{path}"
        );
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
        assert!(!placeholders.is_match(&html), "{path}");
        assert!(social_preview.is_match(&html), "{path}");
        assert!(!html.contains("__STRUCTURED_DATA__") && !html.contains("__SITE_URL__"));
        assert!(html.contains("property=\"og:image\""));
        assert!(html.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(html.contains("property=\"og:image:width\" content=\"1200\""));
        assert!(html.contains("property=\"og:image:height\" content=\"630\""));
        if path.contains("/features/") {
            assert!(
                graph.iter().any(|item| item["@type"] == "BreadcrumbList"),
                "{path}"
            );
        }
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
async fn app_and_local_previews_are_noindex() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in ["/", "/en/", "/landing", "/landing/en/"] {
        let (status, headers, _) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers["x-robots-tag"], "noindex, nofollow", "{path}");
    }
    let (status, _, home) = get(app.clone(), "/", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body_text(&home)?.contains("data-nosnippet"));
    let (status, headers, _) = get(app, "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["x-robots-tag"], "noindex, nofollow");
    Ok(())
}

#[tokio::test]
async fn robots_allows_reading_noindex_and_advertises_only_marketing_sitemap() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, headers, body) = get(app.clone(), "/robots.txt", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str()?.starts_with("text/plain"));
    let text = body_text(&body)?;
    assert!(text.contains("Allow: /"));
    assert!(text.contains("Sitemap: https://example.com/sitemap.xml"));
    let (status, _, body) = get(app.clone(), "/robots.txt", "app.example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let text = body_text(&body)?;
    assert!(!text.contains("Disallow: /") && !text.contains("Sitemap:"));
    let (status, _, _) = get(app, "/sitemap.xml", "app.example.com").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn json_ld_cannot_break_out_of_script_from_a_configured_url() -> TestResult {
    let directory = TempDir::new()?;
    let malicious = "https://app.example.com/?x=</script><script>alert(1)</script>&y=2";
    let (app, _settings) = app_with(&directory, &[("STUDIO_URL", malicious)])?;
    let (status, _, body) = get(app, "/", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let html = body_text(&body)?;
    assert_eq!(
        html.matches("<script type=\"application/ld+json\">")
            .count(),
        1
    );
    let data = between(html, "<script type=\"application/ld+json\">", "</script>")?;
    assert!(!data.contains('<') && !data.contains('&'), "{data}");
    assert!(!html.contains("<script>alert(1)</script>"));
    let parsed: Value = serde_json::from_str(data)?;
    assert!(serde_json::to_string(&parsed)?.contains("</script>"));
    Ok(())
}
