use std::collections::BTreeSet;

use axum::http::StatusCode;
use rich_studio::build_router;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, state, telegram};

#[tokio::test]
async fn render_pages_escape_payload_and_whitelist_theme() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let evil = json!({"rich_message":{"html":"</script><script>alert(1)</script>"}});
    let page = call(
        app.clone(),
        "POST",
        "/api/render?theme=dark",
        Some(evil),
        &[],
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        !page.text.contains("<script>alert(1)</script>"),
        "{}",
        page.text
    );
    assert!(!page.text.contains("__THEME__"));
    assert!(page.text.contains("data-theme=\"dark\""), "{}", page.text);
    assert!(
        page.headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("script-src 'self'")),
        "render CSP"
    );

    let injected = call(
        app.clone(),
        "POST",
        "/api/render?theme=x%22%3Balert(1)//",
        Some(blocks_post()),
        &[],
    )
    .await;
    assert!(!injected.text.contains("x\";alert(1)"), "{}", injected.text);
    assert!(
        injected.text.contains("data-theme=\"auto\""),
        "{}",
        injected.text
    );

    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"x","post":{"rich_message":{"html":"<p>a</p>"}},"\",x=alert(1)//":1})),
        &[],
    )
    .await
    .value;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let draft_page = call(
        app,
        "GET",
        &format!("/render/draft/{id}?theme=__PAYLOAD__"),
        None,
        &[],
    )
    .await;
    assert!(
        draft_page.text.contains("data-theme=\"auto\""),
        "{}",
        draft_page.text
    );
    assert!(
        !draft_page.text.contains("data-theme=\"{"),
        "{}",
        draft_page.text
    );

    let index = call(build_router(state(&dir, &base, &[])), "GET", "/", None, &[]).await;
    assert!(
        index
            .headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("script-src 'self'")),
        "index CSP"
    );
    Ok(())
}

#[tokio::test]
async fn check_reports_bot_username_and_rights() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let reply = call(app, "GET", "/api/check", None, &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["can_post"], true);
    assert_eq!(reply.value["bot"]["username"], "studio_test_bot");
    Ok(())
}

#[tokio::test]
async fn editor_modules_are_served_as_javascript() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let static_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app/static");
    let index = std::fs::read_to_string(static_dir.join("index.html"))?;
    assert!(
        index.contains("<script type=\"module\" src=\"/static/js/main.js\">"),
        "editor index entry point changed"
    );
    let pattern = regex::Regex::new(r"from '\./([\w-]+\.js)'")?;
    let mut imports = BTreeSet::new();
    for entry in std::fs::read_dir(static_dir.join("js"))? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("js") {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        for capture in pattern.captures_iter(&source) {
            if let Some(name) = capture.get(1) {
                imports.insert(name.as_str().to_owned());
            }
        }
    }
    assert!(!imports.is_empty());
    for name in imports {
        let reply = call(app.clone(), "GET", &format!("/static/js/{name}"), None, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert!(
            reply
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .contains("javascript"),
            "{name}: {:?}",
            reply.headers.get("content-type")
        );
    }
    Ok(())
}
