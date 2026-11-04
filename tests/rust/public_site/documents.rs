use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::account_context::AccountContext;
use rich_studio::public_site;
use rich_studio::store::{drafts, posts};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{
    TestResult, app, app_with, bearer, body_text, get, get_with, issue_token, login, post_json,
    recording_telegram,
};

#[tokio::test]
async fn public_documents_keep_exact_routes_head_metadata_and_configured_destinations() -> TestResult
{
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let placeholders = regex::Regex::new(r"__[A-Z_]+__")?;
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
            assert!(
                response.headers()["link"].to_str()?.contains("describedby"),
                "{host}{path}"
            );
            assert!(!response.headers().contains_key("set-cookie"));
            let content_type = response.headers()["content-type"].to_str()?.to_owned();
            assert!(
                content_type.starts_with(if path.ends_with(".json") {
                    "application/json"
                } else {
                    "text/"
                }),
                "{host}{path}: {content_type}"
            );
            let body =
                String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
            assert!(!body.starts_with('\u{feff}'), "{host}{path}: BOM");
            assert!(!placeholders.is_match(&body), "{host}{path}: placeholder");
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
            assert_eq!(head.headers()["content-type"].to_str()?, content_type);
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
        "/skills/richpost/AGENTS.md",
        "/agents.md/extra",
        "/features/unknown/",
        "/bundled/agent-docs/setup.md",
        "/skills/richpost/../../settings.py",
        "/skills/richpost/%2e%2e/%2e%2e/settings.py",
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
async fn forwarded_host_cannot_rewrite_installation_destinations() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::AGENT_PATHS {
        let forged = [
            ("x-forwarded-host", "attacker.invalid"),
            ("forwarded", "host=attacker.invalid"),
        ];
        let (status, _, body) = get_with(app.clone(), path, "mirror.example", &forged).await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        let (plain_status, _, plain) = get(app.clone(), path, "example.com").await?;
        assert_eq!(plain_status, StatusCode::OK, "{path}");
        assert_eq!(body, plain, "{path}");
        let text = body_text(&body)?;
        assert!(
            !text.contains("attacker.invalid") && !text.contains("mirror.example"),
            "{path}"
        );
    }
    let (status, _, _) = get(app.clone(), "/agents.md", "attacker.invalid").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn agent_documents_do_not_expose_secrets_or_account_data() -> TestResult {
    let directory = TempDir::new()?;
    let (app, _settings) = app_with(
        &directory,
        &[
            ("STUDIO_API_KEY", "operator-secret-sentinel"),
            ("TG_BOT_TOKEN", "999:BOT_SECRET_SENTINEL"),
            ("TG_IMPORT_API_ID", "12345"),
            ("TG_IMPORT_API_HASH", "import-secret-sentinel"),
        ],
    )?;
    let mut public_docs = BTreeMap::new();
    for path in public_site::AGENT_PATHS {
        let (status, _, body) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        public_docs.insert(*path, body);
    }
    let session = login(&app, "app.example.com", "999:BOT_SECRET_SENTINEL").await?;
    let created = issue_token(&app, "app.example.com", &session).await?;
    let bearer = bearer(&created)?;
    let block = json!({
        "rich_message": {
            "blocks": [
                {"type": "heading", "size": 1, "text": "Тест"},
                {"type": "paragraph", "text": ["Привет, ", {"type": "bold", "text": "мир"}]}
            ]
        }
    });
    let (status, _, draft) = post_json(
        app.clone(),
        "/api/drafts",
        "app.example.com",
        json!({"title": "PRIVATE_ACCOUNT_SENTINEL", "post": block}),
        &[("cookie", &session)],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let secret = created["secret"].as_str().ok_or("token secret")?;
    for (path, content) in &public_docs {
        let (status, _, plain) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(&plain, content, "{path} changed without authentication");
        let (status, _, authenticated) = get_with(
            app.clone(),
            path,
            "app.example.com",
            &[("authorization", &bearer)],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(&authenticated, content, "{path} changed for a bearer token");
        for value in [
            "operator-secret-sentinel",
            "999:BOT_SECRET_SENTINEL",
            "import-secret-sentinel",
            secret,
            "PRIVATE_ACCOUNT_SENTINEL",
        ] {
            assert!(
                !body_text(content)?.contains(value),
                "{path} leaked {value}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn public_document_access_does_not_grant_mcp_or_token_management() -> TestResult {
    let directory = TempDir::new()?;
    let (base, calls) = recording_telegram().await?;
    let (app, settings) = app_with(
        &directory,
        &[("TG_API_BASE", base.as_str()), ("STUDIO_BACKGROUND", "off")],
    )?;
    let (status, _, _) = get(app.clone(), "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mcp = [
        ("accept", "application/json, text/event-stream"),
        ("mcp-protocol-version", "2025-06-18"),
    ];
    let (status, _, body) = post_json(
        app.clone(),
        "/mcp",
        "app.example.com",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &mcp,
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let session = login(&app, "app.example.com", "1:public-site-secret").await?;
    let created = issue_token(&app, "app.example.com", &session).await?;
    let bearer = bearer(&created)?;
    let mut headers = vec![("authorization", bearer.as_str())];
    headers.extend(mcp);
    let (status, _, result) = post_json(
        app.clone(),
        "/mcp",
        "app.example.com",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_status","arguments":{}}}),
        &headers,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_ne!(result["result"]["isError"], true, "{result}");
    let (status, _, denied) = post_json(
        app.clone(),
        "/api/account/tokens",
        "app.example.com",
        json!({"name":"automatic"}),
        &[("authorization", &bearer)],
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let root = AccountContext::root(&settings);
    assert!(drafts::list(&root)?.is_empty());
    assert!(posts::list(&root)?.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "Telegram was called");
    Ok(())
}

#[tokio::test]
async fn private_installation_documents_work_behind_operator_auth() -> TestResult {
    let directory = TempDir::new()?;
    let (app, _settings) = app_with(
        &directory,
        &[
            ("STUDIO_PUBLIC", "off"),
            ("STUDIO_API_KEY", "private-studio-sentinel"),
            ("STUDIO_LANDING_URL", ""),
        ],
    )?;
    for path in public_site::AGENT_PATHS {
        let (status, _, _) = get(app.clone(), path, "localhost").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let (status, _, guide) = get(app.clone(), "/agents.md", "localhost").await?;
    assert_eq!(status, StatusCode::OK);
    let guide = body_text(&guide)?;
    assert!(guide.contains("Ask the operator"));
    assert!(!guide.contains("private-studio-sentinel"));
    let (status, _, _) = get(app.clone(), "/api/drafts", "localhost").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}
