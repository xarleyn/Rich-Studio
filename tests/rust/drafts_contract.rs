use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::AppState;
use rich_studio::build_router;
use rich_studio::services::drafts::{DraftError, DraftService, SaveDraft};
use rich_studio::settings::Settings;
use rich_studio::spec::diff::diff_specs;
use rich_studio::spec::source::Spec;
use rich_studio::spec::validator::Validator;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn post() -> Value {
    json!({
        "rich_message": {"blocks": [{"type": "paragraph", "text": "Привет", "future_field": 1}]},
        "options": {"disable_notification": true},
        "_studio": {"buffers": {"html": "<p>Привет</p>"}}
    })
}

fn settings(dir: &TempDir, extra: &[(&str, &str)]) -> Settings {
    let mut values = BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]);
    for (name, value) in extra {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}"))
}

fn state(dir: &TempDir) -> AppState {
    AppState::new(settings(dir, &[]))
}

async fn request(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())));
    let response = app
        .oneshot(request.unwrap_or_else(|error| panic!("request: {error}")))
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}"))
    };
    (status, value)
}

#[test]
fn draft_service_round_trips_unknown_fields_and_enforces_versions() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir);
    let service = DraftService::new(state.account.clone());
    service.migrate()?;
    let original = post();
    let created = service.save(SaveDraft {
        id: None,
        title: "Анонс".to_owned(),
        post: original.clone(),
        version: None,
        post_id: None,
        chat_ids: Some(vec!["@Agent_Chan".to_owned(), "@agent_chan".to_owned()]),
    })?;
    assert_eq!(created["payload"], original);
    assert_eq!(created["chat_ids"], json!(["@Agent_Chan"]));
    assert_eq!(created["version"], 0);

    let updated = service.save(SaveDraft {
        id: created["id"].as_str().map(str::to_owned),
        title: "Анонс 2".to_owned(),
        post: json!({"rich_message": {"markdown": "new"}}),
        version: Some(0),
        post_id: Some("post-1".to_owned()),
        chat_ids: None,
    })?;
    assert_eq!(updated["version"], 1);
    assert_eq!(updated["chat_ids"], json!(["@Agent_Chan"]));
    assert!(matches!(
        service.save(SaveDraft {
            id: created["id"].as_str().map(str::to_owned),
            title: "stale".to_owned(),
            post: post(),
            version: Some(0),
            post_id: None,
            chat_ids: None,
        }),
        Err(DraftError::Conflict { .. })
    ));
    assert_eq!(service.list()?.len(), 1);
    assert_eq!(
        service.get(created["id"].as_str().unwrap_or_default())?["post_id"],
        "post-1"
    );

    let default_chat = service.save(SaveDraft {
        id: None,
        title: "Канал по умолчанию".to_owned(),
        post: post(),
        version: None,
        post_id: None,
        chat_ids: Some(vec![]),
    })?;
    assert!(default_chat["chat_ids"].is_null());
    Ok(())
}

#[tokio::test]
async fn draft_http_crud_and_conflict_match_the_editor_contract() -> TestResult {
    let dir = TempDir::new()?;
    let app = build_router(state(&dir));
    let (status, created) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title": "Анонс", "post": post(), "post_id": "post-1", "chat_ids": ["@chan"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().ok_or("draft id")?;
    let (status, full) = request(app.clone(), "GET", &format!("/api/drafts/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(full["payload"], post());
    let (status, updated) = request(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title": "Без связи", "post": post(), "version": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated["post_id"].is_null());
    assert_eq!(updated["chat_ids"], json!(["@chan"]));
    let (status, conflict) = request(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"title": "bad", "post": post(), "version": 99})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(conflict["detail"]["current"].is_object());
    let (status, _) = request(app.clone(), "DELETE", &format!("/api/drafts/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(app, "GET", &format!("/api/drafts/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn spec_loads_bundled_cache_and_pinned_sources() -> TestResult {
    let dir = TempDir::new()?;
    let bundled = Spec::load_initial(&settings(&dir, &[]));
    assert_eq!(bundled.version(), "Bot API 10.3");
    assert_eq!(bundled.source, "bundled");
    let summary = bundled.summary();
    assert_eq!(
        summary["types"]["InputRichBlock"]["subtypes"]
            .as_array()
            .map(Vec::len),
        Some(24)
    );
    assert_eq!(summary["gaps"], json!([]));

    let mut newer = bundled.data.clone();
    newer["version"] = json!("Bot API 99.0");
    std::fs::write(dir.path().join("api.json"), serde_json::to_vec(&newer)?)?;
    let cached = Spec::load_initial(&settings(&dir, &[]));
    assert_eq!(cached.version(), "Bot API 99.0");
    assert_eq!(cached.source, "cache");
    let pinned = Spec::load_initial(&settings(&dir, &[("SPEC_PIN", "on")]));
    assert_eq!(pinned.version(), "Bot API 10.3");
    assert_eq!(pinned.source, "bundled");

    std::fs::write(dir.path().join("api.json"), b"not-json")?;
    assert_eq!(Spec::load_initial(&settings(&dir, &[])).source, "bundled");
    Ok(())
}

#[tokio::test]
async fn spec_refresh_is_pinned_or_keeps_working_offline() -> TestResult {
    let dir = TempDir::new()?;
    let mut pinned = Spec::load_initial(&settings(&dir, &[("SPEC_PIN", "on")]));
    let report = pinned
        .refresh(&settings(&dir, &[("SPEC_PIN", "on")]), true)
        .await;
    assert_eq!(report["pinned"], true);
    assert_eq!(report["version"], "Bot API 10.3");

    let offline_settings = settings(&dir, &[("SPEC_URL", "http://127.0.0.1:9/unavailable")]);
    let mut offline = Spec::load_initial(&offline_settings);
    let report = offline.refresh(&offline_settings, true).await;
    assert_eq!(report["updated"], false);
    assert_eq!(report["version"], "Bot API 10.3");
    assert!(report["error"].as_str().is_some());
    assert_eq!(offline.version(), "Bot API 10.3");
    Ok(())
}

#[test]
fn validator_warns_on_unknown_fields_but_checks_known_structure() {
    let dir = TempDir::new().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let spec = Spec::load_initial(&settings(&dir, &[]));
    let validator = Validator::new(&spec);
    let valid = validator.check_post(&post());
    assert!(valid["ok"].as_bool().unwrap_or(false));
    assert!(valid["warnings"].to_string().contains("future_field"));
    let invalid = validator.check_post(
        &json!({"rich_message": {"blocks": [{"type": "heading", "size": "2", "text": "x"}]}}),
    );
    assert!(!invalid["ok"].as_bool().unwrap_or(true));
    assert!(
        invalid["errors"]
            .to_string()
            .contains("ожидалось целое число")
    );
}

#[test]
fn diff_reports_new_rich_features() {
    let old = json!({"types": {"InputRichBlock": {"subtypes": ["InputRichBlockParagraph"]}}, "methods": {}});
    let new = json!({
        "types": {"InputRichBlock": {"subtypes": ["InputRichBlockParagraph", "InputRichBlockPoll"]}, "InputRichBlockPoll": {"description": ["A poll"], "fields": []}},
        "methods": {"sendRichPoll": {"description": ["Sends a poll"], "fields": []}}
    });
    let diff = diff_specs(&old, &new);
    assert!(diff.iter().any(|item| item["kind"] == "subtype_added"));
    assert!(diff.iter().any(|item| item["name"] == "sendRichPoll"));
}

async fn mcp(app: Router, method: &str, params: Value) -> Value {
    let (_, response) = request(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})),
    )
    .await;
    response
}

#[tokio::test]
async fn mcp_draft_and_spec_tools_keep_the_agent_workflow() -> TestResult {
    let dir = TempDir::new()?;
    let app = build_router(state(&dir));
    let tools = mcp(app.clone(), "tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for name in [
        "validate_post",
        "save_draft",
        "list_drafts",
        "get_draft",
        "describe_type",
        "bot_api_status",
    ] {
        assert!(names.contains(&name), "missing {name}");
    }

    let saved = mcp(
        app.clone(),
        "tools/call",
        json!({"name": "save_draft", "arguments": {"post": post(), "title": "MCP", "chat_ids": ["@agent_chan"]}}),
    )
    .await;
    let data = saved["result"]["structuredContent"].clone();
    assert_eq!(data["validation"]["ok"], true);
    assert_eq!(data["chat_ids"], json!(["@agent_chan"]));
    assert!(
        data["editor_url"]
            .as_str()
            .unwrap_or_default()
            .contains("?draft=")
    );
    let id = data["id"].as_str().ok_or("draft id")?;
    let fetched = mcp(
        app.clone(),
        "tools/call",
        json!({"name": "get_draft", "arguments": {"draft_id": id}}),
    )
    .await;
    // Python MCP save_draft calls Post.model_validate(raw).model_dump(), unlike the
    // arbitrary dictionary accepted by REST drafts. Captured from app/models.py.
    assert_eq!(
        fetched["result"]["structuredContent"]["payload"],
        json!({
            "rich_message": {"blocks": [{"type": "paragraph", "text": "Привет", "future_field": 1}]},
            "reply_markup": null,
            "options": {"disable_notification": true},
            "pin": false,
            "comment": null,
            "callbacks": {},
            "poll_after": null,
        })
    );

    let described = mcp(
        app,
        "tools/call",
        json!({"name": "describe_type", "arguments": {"name": "InputRichBlockTable"}}),
    )
    .await;
    assert_eq!(
        described["result"]["structuredContent"]["name"],
        "InputRichBlockTable"
    );
    Ok(())
}
