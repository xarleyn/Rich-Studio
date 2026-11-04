use std::collections::BTreeMap;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    services::{health, housekeeping, in_flight, loops::LoopStatus},
    store::{idempotency, posts as posts_store},
};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{
    TestResult, blocks_post, check, initialize, initialized, report, settings, telegram_settings,
};

#[tokio::test]
async fn observer_never_logs_body_or_query_and_tenant_health_hides_global_state() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = AppState::new(settings.clone());
    initialize(&state.account)?;
    let app = build_router(state.clone());
    let response=app.oneshot(Request::builder().method("POST").uri("/api/drafts?secret=QUERY_SECRET")
        .header("host","localhost").header("content-type","application/json").header("x-request-id","trace-fixture")
        .body(Body::from(json!({"title":"PRIVATE_POST_SENTINEL","post":{"rich_message":{"html":"<p>BODY_SECRET</p>"}}}).to_string()))?).await?;
    assert!(response.status().is_success());
    assert_eq!(response.headers()["x-request-id"], "trace-fixture");
    assert_eq!(response.headers()["x-robots-tag"], "noindex, nofollow");
    let _ = to_bytes(response.into_body(), 1_000_000).await?;
    let lines = state.logs.read("INFO", 100, None, "", "trace-fixture")?;
    assert!(!lines.is_empty());
    let text = serde_json::to_string(&lines)?;
    assert!(
        !text.contains("QUERY_SECRET")
            && !text.contains("BODY_SECRET")
            && !text.contains("PRIVATE_POST_SENTINEL")
    );
    let context = AccountContext::account(&settings, 77, Role::Publish);
    initialize(&context)?;
    let updates = state.updates_status.read().await.clone();
    let report = health::report(
        &settings,
        &context,
        &state.spec.read().await.clone(),
        &state.logs,
        &state.metrics,
        &LoopStatus::default(),
        &updates,
    );
    assert_eq!(report["metrics"], json!({}));
    assert_eq!(report["recent_problems"], json!([]));
    assert!(
        report["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .all(|check| ["scheduler", "posts", "sends", "spec"]
                .contains(&check["name"].as_str().unwrap_or_default()))
    );
    Ok(())
}

#[tokio::test]
async fn health_of_a_fresh_studio_is_ok_where_it_matters() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let report = report(&state).await;
    assert_eq!(
        report["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .map(|check| check["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        [
            "database",
            "scheduler",
            "posts",
            "sends",
            "spec",
            "editor",
            "telegram",
            "loops",
            "disk",
            "backup",
            "storage",
            "log",
        ]
    );
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        report["bot_api"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Bot API")
    );
    assert!(report["time"].as_str().unwrap_or_default().ends_with('Z'));
    for check in report["checks"].as_array().ok_or("checks")? {
        assert_eq!(check["status"], "ok", "{check}");
    }
    assert_eq!(report["status"], "ok");
    assert_eq!(report["problems"], json!([]));
    assert_eq!(
        report["metrics"]
            .as_object()
            .ok_or("metrics")?
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "http_latency",
            "log_problems",
            "requests",
            "telegram_calls",
            "telegram_latency",
            "uptime_s",
        ]
        .into_iter()
        .collect()
    );
    Ok(())
}

#[tokio::test]
async fn health_names_what_needs_a_person_and_what_to_do() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let now = housekeeping::now();
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "failed",
            message_id: None,
            scheduled_at: None,
            error: Some("Telegram: chat not found"),
            kind: "rich",
            actor: "api",
        },
    )?;
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(now - 600),
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?;
    idempotency::claim(&state.account, "k1", "send", "fp")?;
    idempotency::finish(&state.account, "k1", "uncertain", &json!("пост мог уйти"))?;
    state.logs.event(
        "ERROR",
        "rich-studio.scheduler",
        "scheduler iteration failed",
        json!({"event":"scheduler.error"}),
    );
    state.metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Bad Gateway"),
        1,
        None,
        false,
    );
    let checked = report(&state).await;
    assert_eq!(checked["status"], "warn");
    let posts = check(&checked, "posts");
    assert_eq!(posts["status"], "warn");
    assert_eq!(
        posts["details"]["posts"][0]["error"],
        "Telegram: chat not found"
    );
    let scheduler = check(&checked, "scheduler");
    assert_eq!(scheduler["status"], "warn");
    assert!(
        scheduler["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("выключены"),
        "{scheduler}"
    );
    let sends = check(&checked, "sends");
    assert_eq!(sends["status"], "warn");
    assert!(
        sends["action"]
            .as_str()
            .unwrap_or_default()
            .contains("Idempotency-Key")
    );
    assert!(
        check(&checked, "log")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("scheduler iteration failed")
    );
    assert!(
        check(&checked, "telegram")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("Bad Gateway")
    );
    for check in checked["checks"].as_array().ok_or("checks")? {
        if check["status"] != "ok" {
            assert!(
                check["action"]
                    .as_str()
                    .is_some_and(|action| !action.is_empty()),
                "{check}"
            );
        }
    }
    state
        .metrics
        .telegram_call("getMe", true, None, 1, None, false);
    assert_eq!(check(&report(&state).await, "telegram")["status"], "ok");
    Ok(())
}

#[tokio::test]
async fn a_conflicting_bot_is_a_failure() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    settings.background = true;
    settings.updates_on = true;
    let state = initialized(settings)?;
    {
        let mut status = state.updates_status.write().await;
        status.state = "conflict".to_owned();
        status.error = Some("Conflict: terminated by other getUpdates request".to_owned());
    }
    let report = report(&state).await;
    assert_eq!(report["status"], "fail");
    let telegram = check(&report, "telegram");
    assert_eq!(telegram["status"], "fail");
    assert!(
        telegram["action"]
            .as_str()
            .unwrap_or_default()
            .contains("TG_UPDATES=off")
    );
    assert!(
        telegram["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("terminated by other getUpdates")
    );
    Ok(())
}

#[tokio::test]
async fn health_changes_are_logged_once() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let mut previous = BTreeMap::new();
    housekeeping::watch(&report(&state).await, &mut previous, &state.logs);
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "failed",
            message_id: None,
            scheduled_at: None,
            error: Some("boom"),
            kind: "rich",
            actor: "api",
        },
    )?;
    let changed = report(&state).await;
    housekeeping::watch(&changed, &mut previous, &state.logs);
    housekeeping::watch(&changed, &mut previous, &state.logs);
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.health", "")?;
    let changes = lines
        .iter()
        .filter(|line| line["event"] == "health.change" && line["check"] == "posts")
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["status"], "warn");
    assert!(
        changes[0]["action"]
            .as_str()
            .is_some_and(|action| !action.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn a_post_being_sent_right_now_is_not_a_problem() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let id = posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(housekeeping::now() - 5),
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?["id"]
        .as_str()
        .ok_or("post id")?
        .to_owned();
    let guard =
        in_flight::claim(&state.account, &id, housekeeping::now())?.ok_or("in-flight claim")?;
    assert_eq!(check(&report(&state).await, "posts")["status"], "ok");
    drop(guard);
    let flagged = report(&state).await;
    let posts = check(&flagged, "posts");
    assert_eq!(posts["status"], "warn");
    assert_eq!(posts["details"]["posts"][0]["id"], id.as_str());
    posts_store::mark_failed(&state.account, &id, "test")?;
    Ok(())
}
