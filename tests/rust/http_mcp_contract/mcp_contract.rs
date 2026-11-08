use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use rich_studio::services::kinds;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{
    TestResult, app, call_ok, request, request_with_headers, rpc, studio_with_fake,
};

#[tokio::test]
async fn mcp_enforces_required_arguments_and_preserves_list_content_shape() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let missing = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"save_draft","arguments":{}}),
    )
    .await;
    assert_eq!(missing["result"]["isError"], true);
    let invalid = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"studio_logs","arguments":{"limit":99999}}),
    )
    .await;
    assert_eq!(invalid["result"]["isError"], true);
    for title in ["one", "two"] {
        let saved=rpc(app.clone(),"tools/call",json!({"name":"save_draft","arguments":{"title":title,"post":{"rich_message":{"markdown":title}}}})).await;
        assert_eq!(saved["result"]["isError"], false, "{saved}");
    }
    let listed = rpc(
        app,
        "tools/call",
        json!({"name":"list_drafts","arguments":{}}),
    )
    .await;
    let blocks = listed["result"]["content"].as_array().ok_or("content")?;
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|block| {
        serde_json::from_str::<Value>(block["text"].as_str().unwrap_or(""))
            .is_ok_and(|value| value.is_object())
    }));
    Ok(())
}

#[tokio::test]
async fn mcp_publish_off_hides_and_rejects_publishing_tools() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, false);
    let listed = rpc(app.clone(), "tools/list", json!({})).await;
    let contract: Value =
        serde_json::from_str(include_str!("../../../src/mcp_tools/contract.json"))?;
    let publishing = BTreeSet::from([
        "publish_post",
        "schedule_post",
        "edit_post",
        "reschedule_post",
        "cancel_scheduled_post",
        "send_poll",
        "create_invite_link",
        "edit_invite_link",
        "revoke_invite_link",
        "send_media_post",
        "approve_suggested_post",
        "decline_suggested_post",
        "send_widget",
        "edit_widget_data",
    ]);
    let expected = contract["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| !publishing.contains(name))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    let called=rpc(app,"tools/call",json!({"name":"publish_post","arguments":{"post":{"rich_message":{"markdown":"must not send"}}}})).await;
    assert_eq!(called["result"]["isError"], true);
    Ok(())
}

#[tokio::test]
async fn config_and_mcp_media_schemas_match_the_kind_registry() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status, config) = request(app, "GET", "/api/config", None).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let described = config["kinds"].as_object().ok_or("missing kinds")?;
    assert_eq!(described.len(), kinds::KINDS.len());
    for kind in kinds::KINDS {
        let entry = &described[kind.name];
        assert_eq!(entry["label"], kind.label, "{}", kind.name);
        assert_eq!(entry["badge"], json!(kind.badge), "{}", kind.name);
        assert_eq!(entry["editable"], kind.editable, "{}", kind.name);
        assert_eq!(entry["stoppable"], kind.stoppable, "{}", kind.name);
        assert_eq!(entry["media"], kind.media, "{}", kind.name);
    }
    let contract: Value =
        serde_json::from_str(include_str!("../../../src/mcp_tools/contract.json"))?;
    let tool = contract["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .find(|tool| tool["name"] == "send_media_post")
        .ok_or("send_media_post")?;
    let media = kinds::KINDS
        .iter()
        .filter(|kind| kind.media)
        .map(|kind| kind.name)
        .collect::<Vec<_>>();
    assert_eq!(
        tool["inputSchema"]["properties"]["kind"]["enum"],
        json!(media)
    );
    Ok(())
}

#[tokio::test]
async fn mcp_requires_the_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[("STUDIO_API_KEY", "secret")]).await;
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let (status, _) = request(app.clone(), "POST", "/mcp", Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, listed) =
        request_with_headers(app, "POST", "/mcp", Some(body), &[("x-api-key", "secret")]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn mcp_get_is_405_not_an_endless_stream() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .uri("/mcp")
                .header("accept", "text/event-stream")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    Ok(())
}

#[tokio::test]
async fn refresh_bot_api_reports_failure_and_keeps_the_spec_through_mcp() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(
        &dir,
        &[
            ("SPEC_PIN", "off"),
            ("SPEC_URL", "http://127.0.0.1:9/unavailable"),
        ],
    )
    .await;
    let before = call_ok(app.clone(), "bot_api_status", json!({})).await;
    let report = call_ok(app.clone(), "refresh_bot_api", json!({})).await;
    assert_eq!(report["updated"], false);
    assert_eq!(report["version"], before["version"]);
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{report}"
    );
    let after = call_ok(app, "bot_api_status", json!({})).await;
    assert_eq!(after["gaps"], json!([]));
    assert_eq!(after["last_check"]["error"], report["error"]);
    Ok(())
}

#[tokio::test]
async fn tools_that_reach_telegram_say_so() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let listed = rpc(app, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap_or_default().to_owned(),
                tool["annotations"].clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for name in [
        "send_preview_to_me",
        "check_channel",
        "publish_post",
        "schedule_post",
        "edit_post",
    ] {
        assert_eq!(
            tools.get(name).ok_or(name)?["openWorldHint"],
            true,
            "{name}"
        );
    }
    assert_eq!(tools["send_preview_to_me"]["destructiveHint"], false);
    Ok(())
}

#[test]
fn readme_points_at_the_generated_tool_reference() {
    let readme = include_str!("../../../README.md");
    assert!(readme.contains("docs/reference/mcp-tools.md"));
    assert!(readme.contains("docs/reference/http-api.md"));
}
