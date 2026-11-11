use std::time::Duration;

use axum::http::StatusCode;
use rich_studio::{
    build_router, logs,
    services::{housekeeping, updates},
    store::{db::Store, posts as posts_store},
};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestError, TestResult, blocks_post, initialized, report, request_with_headers, settings,
    telegram, telegram_error, telegram_settings,
};

#[tokio::test]
async fn request_id_ties_a_request_together_and_every_request_gets_a_new_one() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, sent, headers) = request_with_headers(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let rid = headers["x-request-id"].to_str()?.to_owned();
    assert!(
        rid.len() == 32
            && rid
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{rid}"
    );
    let (_, logged, _) =
        request_with_headers(app.clone(), "GET", &format!("/api/logs?q={rid}"), None, &[]).await?;
    let lines = logged["lines"].as_array().ok_or("lines")?;
    let events = lines
        .iter()
        .map(|line| line["event"].as_str().unwrap_or_default())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(events, ["telegram.call", "http"].into_iter().collect());
    assert!(lines.iter().all(|line| line["request_id"] == rid));
    let (_, _, next) = request_with_headers(app, "GET", "/api/drafts", None, &[]).await?;
    assert_ne!(next["x-request-id"], rid.as_str());
    Ok(())
}

#[test]
fn incoming_request_ids_follow_the_caller_and_w3c_traceparent() {
    assert_eq!(logs::incoming_id(Some("agent-42"), None), "agent-42");
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let parent = "00f067aa0ba902b7";
    assert_eq!(
        logs::incoming_id(None, Some(&format!("00-{trace}-{parent}-01"))),
        trace
    );
    let bad = logs::incoming_id(Some(&format!("{}\n", "x".repeat(65))), None);
    assert!(
        bad.len() == 32 && bad.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{bad}"
    );
    let zero = "0".repeat(32);
    assert_ne!(
        logs::incoming_id(None, Some(&format!("00-{zero}-{parent}-01"))),
        zero
    );
    let no_parent = format!("00-{trace}-{}-01", "0".repeat(16));
    assert_ne!(logs::incoming_id(None, Some(&no_parent)), trace);
    let future = format!("cc-{trace}-{parent}-01-what-the-future-will-be");
    assert_eq!(logs::incoming_id(None, Some(&future)), trace);
    let extra = format!("00-{trace}-{parent}-01-extra");
    assert_ne!(logs::incoming_id(None, Some(&extra)), trace);
    let uppercase = format!("00-{}-{}-01", trace.to_uppercase(), parent.to_uppercase());
    assert_ne!(
        logs::incoming_id(None, Some(&uppercase)),
        trace.to_uppercase()
    );
}

#[tokio::test]
async fn a_scheduler_round_logs_its_calls_under_its_own_request_id() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let mut settings = telegram_settings(&directory, &base)?;
    settings.background = true;
    settings.updates_on = false;
    settings.backup_keep = 0;
    let state = initialized(settings)?;
    let id = posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@test_channel"),
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
    let loops = rich_studio::services::loops::Loops::start(state.clone());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if posts_store::get(&state.account, &id)?
                .as_ref()
                .is_some_and(|post| post["status"] == "sent")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, TestError>(())
    })
    .await??;
    loops.request_stop();
    tokio::time::timeout(Duration::from_secs(10), loops.stop()).await??;
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.telegram", "")?;
    let call = lines
        .iter()
        .find(|line| line["event"] == "telegram.call" && line["method"] == "sendRichMessage")
        .ok_or("scheduler telegram call")?;
    assert!(
        call["request_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("scheduler-"),
        "{call}"
    );
    assert_eq!(
        posts_store::get(&state.account, &id)?.ok_or("sent post")?["status"],
        "sent"
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_incoming_update_is_logged_under_its_update_id() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    Store::connect(&state.account)?.execute("drop table suggested_posts", [])?;
    updates::receive_webhook(
        &state,
        &json!({"update_id":7001,"message":{"chat":{"id":-1001},"message_id":1,
            "suggested_post_info":{"state":"pending","send_date":0,"price":{"amount":1,"currency":"XTR"}}}}),
    )
    .await?;
    let lines = state
        .logs
        .read("ERROR", 100, None, "rich-studio.updates", "")?;
    let failure = lines
        .iter()
        .find(|line| line["event"] == "update.failed")
        .ok_or_else(|| format!("update.failed line: {lines:?}"))?;
    assert_eq!(failure["request_id"], "update-7001");
    assert!(
        failure["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("suggested_posts"),
        "{failure}"
    );
    Ok(())
}

#[tokio::test]
async fn problems_in_the_report_point_to_their_request() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram_error(500, json!({"ok":false,"description":"upstream broke"})).await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, failed, headers) = request_with_headers(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[("x-request-id", "trace-me")],
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{failed}");
    assert_eq!(headers["x-request-id"], "trace-me");
    let report = report(&state).await;
    let problem = report["recent_problems"]
        .as_array()
        .ok_or("recent_problems")?
        .iter()
        .find(|problem| {
            problem["message"]
                .as_str()
                .unwrap_or_default()
                .contains("upstream broke")
        })
        .ok_or_else(|| format!("recent problem: {report}"))?;
    assert_eq!(problem["request_id"], "trace-me");
    Ok(())
}

#[tokio::test]
async fn unhandled_error_keeps_its_request_id() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    Store::connect(&state.account)?.execute("drop table drafts", [])?;
    let app = build_router(state.clone());
    let (status, _, headers) = request_with_headers(
        app,
        "GET",
        "/api/drafts",
        None,
        &[("x-request-id", "boom-1")],
    )
    .await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers["x-request-id"], "boom-1");
    let lines = state
        .logs
        .read("WARNING", 100, None, "rich-studio.http", "")?;
    let access = lines
        .iter()
        .find(|line| line["event"] == "http" && line["status"] == 500)
        .ok_or_else(|| format!("access line: {lines:?}"))?;
    assert_eq!(access["request_id"], "boom-1");
    Ok(())
}
