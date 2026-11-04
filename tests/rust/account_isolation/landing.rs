use std::collections::BTreeMap;

use axum::http::StatusCode;
use rich_studio::settings::Settings;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{BOT_TOKEN, TestResult, call_on, config};

#[test]
fn public_requires_https_server_configuration() {
    let error = Settings::from_map(&BTreeMap::from([(
        "STUDIO_PUBLIC".to_owned(),
        "on".to_owned(),
    )]))
    .expect_err("public mode without a server address");
    assert!(
        error
            .problems
            .iter()
            .any(|problem| problem.contains("STUDIO_MODE=server")),
        "{error}"
    );
    let config = Settings::from_map(&BTreeMap::from([
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        ("STUDIO_URL".to_owned(), "https://studio.example".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
    ]))
    .expect("valid public configuration");
    assert!(config.public);
    assert_eq!(config.bot_username, "YourBot");
}

#[tokio::test]
async fn landing_host_and_app_host_are_separate() -> TestResult {
    let dir = TempDir::new()?;
    let settings = config(
        dir.path(),
        &[
            ("STUDIO_MODE", "server"),
            ("STUDIO_PUBLIC", "on"),
            ("STUDIO_URL", "https://app.example.com"),
            ("STUDIO_LANDING_URL", "https://example.com"),
            ("TG_BOT_TOKEN", BOT_TOKEN),
            ("TG_API_BASE", "http://127.0.0.1:9"),
            ("TG_UPDATES", "off"),
            ("STUDIO_BACKGROUND", "off"),
            ("SPEC_PIN", "on"),
        ],
    );
    let app = build_router(AppState::new(settings));

    let landing = call_on(app.clone(), "GET", "/", "example.com", None, &[]).await;
    assert_eq!(landing.status, StatusCode::OK);
    let landing_text = landing.text();
    assert!(landing_text.contains("Хороший пост."), "{landing_text}");
    assert!(
        landing_text.contains("href=\"https://app.example.com?lang=ru\""),
        "{landing_text}"
    );
    assert!(!landing_text.contains("id=\"title\""), "{landing_text}");
    assert!(
        !landing
            .header("content-security-policy")
            .contains("https://telegram.org")
    );

    let application = call_on(app.clone(), "GET", "/", "app.example.com", None, &[]).await;
    assert_eq!(application.status, StatusCode::OK);
    assert!(application.text().contains("id=\"title\""));
    assert!(
        application
            .header("content-security-policy")
            .contains("https://telegram.org")
    );

    let session = call_on(app.clone(), "GET", "/api/session", "example.com", None, &[]).await;
    assert_eq!(session.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        session.header("location"),
        "https://app.example.com/api/session"
    );

    assert_eq!(
        call_on(
            app.clone(),
            "POST",
            "/api/auth/telegram/start",
            "example.com",
            Some(json!({})),
            &[]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let start = call_on(
        app,
        "POST",
        "/api/auth/telegram/start",
        "app.example.com",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(start.status, StatusCode::OK);
    assert!(
        start.json()["auth_url"]
            .as_str()
            .expect("auth_url")
            .starts_with("https://app.example.com/auth/telegram/callback?"),
        "{}",
        start.text()
    );
    assert!(
        !start
            .header("set-cookie")
            .to_ascii_lowercase()
            .contains("domain=")
    );
    Ok(())
}

#[test]
fn landing_configuration_requires_a_separate_https_host() {
    let base = BTreeMap::from([
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        (
            "STUDIO_URL".to_owned(),
            "https://app.example.com".to_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
    ]);
    let mut values = base.clone();
    values.insert(
        "STUDIO_LANDING_URL".to_owned(),
        "https://example.com".to_owned(),
    );
    let config = Settings::from_map(&values).expect("landing configuration");
    assert!(
        config.allowed_hosts.contains("example.com")
            && config.allowed_hosts.contains("app.example.com")
    );
    assert!(config.is_landing_host("example.com"));
    assert!(!config.is_landing_host("app.example.com"));

    for url in [
        "http://example.com",
        "https://app.example.com",
        "https://example.com/sub",
        "https://user@example.com",
    ] {
        let mut values = base.clone();
        values.insert("STUDIO_LANDING_URL".to_owned(), url.to_owned());
        let error = Settings::from_map(&values).expect_err(url);
        assert!(
            error
                .problems
                .iter()
                .any(|problem| problem.contains("STUDIO_LANDING_URL")),
            "{url}: {error}"
        );
    }
}
