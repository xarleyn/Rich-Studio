use std::collections::{BTreeMap, BTreeSet};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, app, body_text, get, get_with, toml_section};

#[tokio::test]
async fn agent_discovery_links_resolve_and_all_views_agree() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, _, llms) = get(app.clone(), "/llms.txt", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let llms = body_text(&llms)?;
    assert!(llms.starts_with("# RichPost\n"));
    let link = regex::Regex::new(r"\]\((https://(?:app\.)?example\.com[^)]+)\)")?;
    let urls = link
        .captures_iter(llms)
        .filter_map(|captures| captures.get(1))
        .map(|matched| matched.as_str())
        .collect::<Vec<_>>();
    assert!(!urls.is_empty());
    for url in urls {
        let parsed = url::Url::parse(url)?;
        let host = parsed.host_str().ok_or("discovery link host")?;
        let (status, _, body) = get(app.clone(), parsed.path(), host).await?;
        assert_eq!(status, StatusCode::OK, "{url}: {}", body_text(&body)?);
    }
    let (status, _, catalog) = get(app.clone(), "/agents.json", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let catalog: Value = serde_json::from_slice(&catalog)?;
    assert_eq!(catalog["site"]["url"], "https://example.com");
    assert_eq!(catalog["mcp"][0]["url"], "https://app.example.com/mcp");
    assert_eq!(catalog["mcp"][0]["type"], "streamable-http");
    let keys = catalog
        .as_object()
        .ok_or("catalog object")?
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from(["$schema", "version", "standard", "site", "mcp", "skills"])
    );
    let (_, _, directives) = get(app.clone(), "/agents.txt", "example.com").await?;
    let directives = body_text(&directives)?;
    let mcp_url = catalog["mcp"][0]["url"].as_str().ok_or("mcp url")?;
    let skill_url = catalog["skills"][0]["url"].as_str().ok_or("skill url")?;
    assert!(directives.contains(&format!("MCP: {mcp_url}\n")));
    assert!(directives.contains(&format!("Skills: {skill_url}\n")));
    let (_, _, full) = get(app.clone(), "/llms-full.txt", "example.com").await?;
    let full = body_text(&full)?;
    let (_, _, guide) = get(app.clone(), "/agents.md", "example.com").await?;
    let (_, _, skill) = get(app.clone(), "/skills/richpost/SKILL.md", "example.com").await?;
    assert!(full.contains(body_text(&guide)?));
    assert!(full.contains(body_text(&skill)?));
    Ok(())
}

#[tokio::test]
async fn agent_setup_examples_are_parseable_and_keep_credentials_out_of_config() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, _, guide) = get(app.clone(), "/agents.md", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let guide = body_text(&guide)?;
    let fenced_toml = regex::Regex::new(r"(?s)```toml\n(.*?)```")?;
    let toml = fenced_toml
        .captures(guide)
        .and_then(|captures| captures.get(1))
        .ok_or("toml setup block")?
        .as_str();
    let codex = toml_section(toml, "mcp_servers.richpost")?;
    assert_eq!(
        codex,
        BTreeMap::from([
            ("url".to_owned(), "https://app.example.com/mcp".to_owned()),
            (
                "bearer_token_env_var".to_owned(),
                "RICHPOST_TOKEN".to_owned()
            ),
        ])
    );
    let fenced_json = regex::Regex::new(r"(?s)```json\n(.*?)```")?;
    let claude: Value = serde_json::from_str(
        fenced_json
            .captures(guide)
            .and_then(|captures| captures.get(1))
            .ok_or("json setup block")?
            .as_str(),
    )?;
    let claude = &claude["mcpServers"]["richpost"];
    assert_eq!(
        claude,
        &json!({
            "type": "http",
            "url": "https://app.example.com/mcp",
            "headers": {"Authorization": "Bearer ${RICHPOST_TOKEN}"}
        })
    );
    assert_eq!(claude["url"].as_str(), Some(codex["url"].as_str()));
    for required in [
        "Required human step",
        "Токены API и MCP",
        "draft",
        "tools/list",
        "studio_status",
        "isError",
    ] {
        assert!(guide.contains(required), "{required}");
    }
    assert!(guide.contains("Do not mint a token"));
    let (status, _, skill) = get(app.clone(), "/skills/richpost/SKILL.md", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let skill = body_text(&skill)?;
    assert!(skill.starts_with("---\nname: richpost\ndescription:"));
    assert!(skill.contains("https://example.com/agents.md"));
    assert!(skill.contains("Token issuance is a human step"));
    Ok(())
}

#[tokio::test]
async fn human_pages_expose_setup_links_and_markdown_discovery() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for prefix in ["", "/en"] {
        let (status, _, home) = get(app.clone(), &format!("{prefix}/"), "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{prefix}/");
        let home = body_text(&home)?;
        assert!(
            home.contains(&format!("href=\"{prefix}/features/ai-agents/\"")),
            "{prefix}/"
        );
        let (status, _, page) = get(
            app.clone(),
            &format!("{prefix}/features/ai-agents/"),
            "example.com",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{prefix}/features/ai-agents/");
        let page = body_text(&page)?;
        assert!(page.contains("rel=\"describedby\" href=\"/llms.txt\" type=\"text/markdown\""));
        assert!(page.contains("href=\"https://example.com/agents.md\""));
        assert!(page.contains("href=\"/skills/richpost/SKILL.md\""));
    }
    Ok(())
}

#[tokio::test]
async fn languages_are_explicit_and_reciprocal() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let alternate = regex::Regex::new(r#"<link rel="alternate"[^>]+>"#)?;
    let cyrillic = regex::Regex::new("[а-яА-ЯёЁ]")?;
    for path in ["/", "/features/telegram-tables/"] {
        let (status, _, ru_body) = get_with(
            app.clone(),
            path,
            "example.com",
            &[("accept-language", "en")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        let (status, _, en_body) = get_with(
            app.clone(),
            &format!("/en{path}"),
            "example.com",
            &[("accept-language", "ru")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "/en{path}");
        let ru = body_text(&ru_body)?;
        let en = body_text(&en_body)?;
        assert!(ru.contains("<html lang=\"ru\">"), "{path}");
        assert!(en.contains("<html lang=\"en\">"), "/en{path}");
        let ru_alternates = alternate
            .find_iter(ru)
            .map(|matched| matched.as_str())
            .collect::<Vec<_>>();
        let en_alternates = alternate
            .find_iter(en)
            .map(|matched| matched.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ru_alternates, en_alternates, "{path}");
        assert!(en.contains("?lang=en\""), "/en{path}");
        let without_label = en.replace("aria-label=\"Русская версия\"", "");
        assert!(!cyrillic.is_match(&without_label), "/en{path}");
    }
    Ok(())
}
