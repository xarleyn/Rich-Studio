use axum::http::StatusCode;
use rich_studio::{AppState, build_router, metrics::Metrics};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, check, initialized, report, report_with, request_with_headers,
    settings, telegram, telegram_settings,
};

#[tokio::test]
async fn latency_is_kept_per_endpoint_template() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let mut post_id = String::new();
    for _ in 0..3 {
        let (status, sent, _) = request_with_headers(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{sent}");
        post_id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    }
    let (status, _, _) = request_with_headers(
        app,
        "GET",
        &format!("/api/posts/{post_id}/versions"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let report = report(&state).await;
    let latency = &report["metrics"]["http_latency"];
    assert_eq!(latency["POST /api/send"]["n"], 3);
    assert_eq!(latency["GET /api/posts/{id}/versions"]["n"], 1, "{latency}");
    assert!(
        latency
            .as_object()
            .ok_or("http_latency")?
            .keys()
            .all(|key| !key.contains(&post_id))
    );
    let telegram = &report["metrics"]["telegram_latency"]["sendRichMessage"];
    assert_eq!(telegram["n"], 3);
    assert!(telegram["p50_ms"].as_u64() <= telegram["p95_ms"].as_u64());
    assert!(telegram["p95_ms"].as_u64() <= telegram["max_ms"].as_u64());
    Ok(())
}

#[tokio::test]
async fn readiness_is_public_and_reports_only_failing_check_names() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.api_key = "secret".to_owned();
    let state = initialized(settings.clone())?;
    let app = build_router(state.clone());
    let (status, body, _) = request_with_headers(app.clone(), "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ready":true,"failing":[]}));
    let (status, _, _) = request_with_headers(app.clone(), "GET", "/api/health", None, &[]).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    std::fs::remove_file(state.account.database_path())?;
    let (status, cached, _) = request_with_headers(app, "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cached, json!({"ready":true,"failing":[]}));
    let missing = build_router(AppState::new(settings));
    let (status, body, _) = request_with_headers(missing, "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ready"], false);
    assert!(
        body["failing"]
            .as_array()
            .ok_or("failing")?
            .contains(&json!("database"))
    );
    Ok(())
}

#[tokio::test]
async fn unknown_methods_and_raw_telegram_methods_do_not_grow_metrics() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    for index in 0..5 {
        let _ = request_with_headers(app.clone(), &format!("M{index}"), "/api/drafts", None, &[])
            .await?;
    }
    let latency = state.metrics.snapshot()["http_latency"].clone();
    assert!(
        latency
            .as_object()
            .ok_or("http_latency")?
            .keys()
            .all(|key| !key.starts_with('M')),
        "{latency}"
    );
    for index in 0..120 {
        state
            .metrics
            .telegram_call(&format!("method{index}"), true, None, 1, None, false);
    }
    let telegram = state.metrics.snapshot()["telegram_latency"].clone();
    assert!(
        telegram.as_object().ok_or("telegram_latency")?.len() <= 101,
        "{telegram}"
    );
    assert!(telegram.get("other").is_some(), "{telegram}");
    Ok(())
}

#[tokio::test]
async fn telegram_check_ignores_a_rejected_post_but_not_a_setup_problem() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    for (refusal, expected) in [
        (
            "Bad Request: poll members_only is available only in channels",
            "ok",
        ),
        (
            "Bad Request: query is too old and response timeout expired",
            "ok",
        ),
        (
            "Bad Request: can't parse entities: unsupported start tag",
            "ok",
        ),
        ("Bad Request: chat not found", "warn"),
        ("Bad Request: not enough rights to send messages", "warn"),
        (
            "Bad Request: group chat was upgraded to a supergroup chat",
            "warn",
        ),
        ("Bad Request: message thread not found", "warn"),
        ("Bad Request: TOPIC_CLOSED", "warn"),
        ("Bad Request: PEER_ID_INVALID", "warn"),
        ("Bad Request: chat_id is empty", "warn"),
        ("Bad Request: CHAT_WRITE_FORBIDDEN", "warn"),
    ] {
        let metrics = Metrics::default();
        metrics.telegram_call("sendRichMessage", false, Some(refusal), 1, Some(400), false);
        let report = report_with(&state, &metrics).await;
        assert_eq!(check(&report, "telegram")["status"], expected, "{refusal}");
    }
    Ok(())
}

#[tokio::test]
async fn a_refused_post_or_long_poll_does_not_hide_a_setup_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let metrics = Metrics::default();
    metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Forbidden: bot was kicked from the channel chat"),
        1,
        Some(403),
        false,
    );
    metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Bad Request: members_only is available only in channels"),
        1,
        Some(400),
        false,
    );
    let report = report_with(&state, &metrics).await;
    assert!(
        check(&report, "telegram")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("kicked")
    );
    metrics.telegram_call("getUpdates", true, None, 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "warn"
    );
    metrics.telegram_call("sendRichMessage", true, None, 5, None, false);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "ok"
    );
    metrics.telegram_call("getUpdates", false, Some("ConnectError"), 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "warn"
    );
    metrics.telegram_call("getUpdates", true, None, 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "ok"
    );
    Ok(())
}

#[test]
fn monitor_agent_is_read_only() -> TestResult {
    let agent = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/agents/studio-monitor.md"),
    )?;
    let front = agent.split("---").nth(1).ok_or("front matter")?;
    assert!(front.lines().any(|line| line == "name: studio-monitor"));
    let tools = front
        .lines()
        .find_map(|line| line.strip_prefix("tools: "))
        .ok_or("tools")?;
    let tools = tools
        .split(',')
        .map(str::trim)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        !tools.contains("Edit") && !tools.contains("Write"),
        "{tools:?}"
    );
    Ok(())
}

#[test]
fn session_health_hook_is_registered_next_to_the_format_hook() -> TestResult {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(
        root.join(".claude/settings.json"),
    )?)?;
    let start = settings["hooks"]["SessionStart"]
        .as_array()
        .ok_or("SessionStart")?
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(start.contains("studio_health.py"), "{start}");
    assert!(start.contains("session-start.sh"), "{start}");
    assert!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_default()
            .contains("format.py")
    );
    Ok(())
}

#[test]
fn monitor_skill_explains_every_health_check() {
    let skill = include_str!("../../../.claude/skills/studio-health/SKILL.md");
    let explained = skill
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|line| line.split_once("` |").map(|(name, _)| name.to_owned()))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        "database",
        "editor",
        "telegram",
        "loops",
        "scheduler",
        "posts",
        "sends",
        "spec",
        "backup",
        "storage",
        "log",
        "disk",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(explained, expected);
}

#[test]
fn telegram_skill_is_where_claude_code_finds_it() {
    let skill = include_str!("../../../.claude/skills/telegram-rich-posts/SKILL.md");
    let head = skill.split("---").nth(1).unwrap_or_default();
    assert!(head.contains("name: telegram-rich-posts"));
    assert!(
        head.lines()
            .any(|line| line.starts_with("description: ") && line.len() >= 30)
    );
}
