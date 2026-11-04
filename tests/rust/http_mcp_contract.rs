use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::{AppState, build_router, services::kinds, settings::Settings};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn app(dir: &TempDir, publish: bool) -> Router {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), "http://127.0.0.1:1".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
        (
            "MCP_PUBLISH".to_owned(),
            if publish { "on" } else { "off" }.to_owned(),
        ),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    build_router(AppState::new(settings))
}
async fn request(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("response body: {error}"));
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
async fn rpc(app: Router, method: &str, params: Value) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":9,"method":method,"params":params})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
#[tokio::test]
async fn all_legacy_http_operations_and_mcp_schemas_survive_the_sdk_adapter() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status, schema) = request(app.clone(), "GET", "/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    let original: Value = serde_json::from_str(include_str!("../../docs/openapi.json"))?;
    assert_eq!(schema, original);
    assert_eq!(
        schema["paths"]
            .as_object()
            .ok_or("paths")?
            .values()
            .map(|methods| methods.as_object().map_or(0, serde_json::Map::len))
            .sum::<usize>(),
        71
    );
    let init=rpc(app.clone(),"initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"contract","version":"1"}})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "rich-studio");
    assert!(
        init["result"]["instructions"]
            .as_str()
            .ok_or("instructions")?
            .contains("explicitly confirmed")
    );
    let listed = rpc(app, "tools/list", json!({})).await;
    let original: Value = serde_json::from_str(include_str!("../../src/mcp_tools/contract.json"))?;
    assert_eq!(listed["result"]["tools"], original["tools"]);
    assert_eq!(
        listed["result"]["tools"].as_array().ok_or("tools")?.len(),
        39
    );
    Ok(())
}
#[tokio::test]
async fn malformed_http_models_fail_before_side_effects_and_unknown_post_fields_remain_open()
-> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for (path, body) in [
        ("/api/drafts", json!([])),
        ("/api/send", json!({})),
        (
            "/api/send",
            json!({"post":{"rich_message":{"markdown":"test"},"pin":"bad"}}),
        ),
        ("/api/posts/missing/buttons", json!({"callbacks":1})),
        ("/api/posts/missing/buttons", json!({"callbacks":[]})),
        (
            "/api/templates/missing/drafts",
            json!({"version":1,"values":{"name":3}}),
        ),
    ] {
        let (status, result) = request(app.clone(), "POST", path, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path} {result}");
    }
    let post = json!({"rich_message":{"markdown":"future"},"future_option":{"value":123}});
    let (status, created) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"contract","post":post})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (_, draft) = request(
        app,
        "GET",
        &format!("/api/drafts/{}", created["id"].as_str().ok_or("draft id")?),
        None,
    )
    .await;
    assert_eq!(draft["payload"]["future_option"], post["future_option"]);
    Ok(())
}
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
    assert!(
        !listed["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "publish_post")
    );
    let called=rpc(app,"tools/call",json!({"name":"publish_post","arguments":{"post":{"rich_message":{"markdown":"must not send"}}}})).await;
    assert_eq!(called["result"]["isError"], true);
    Ok(())
}

#[test]
fn python_model_coercions_defaults_and_open_telegram_payloads_match() -> TestResult {
    use rich_studio::services::contracts;
    let document = contracts::http_document()?;
    let schema = &document["components"]["schemas"]["PublishReq"];
    let rich = json!({"markdown":"hello","future_field":{"pin":"true","values":[null,"1",1],"large_integer":u64::MAX}});
    let options = json!({"disable_notification":"false","future_option":["0",false]});
    let normalized = contracts::normalize(
        document,
        schema,
        &json!({"post":{"rich_message":rich,"options":options,"pin":"TRUE","ignored_outer":1},"chat_id":"-1001","dry_run":"yes","ignored_request":1}),
    )?;
    assert_eq!(
        normalized,
        json!({"post":{"rich_message":rich,"options":options,"pin":true,"reply_markup":null,"callbacks":{},"comment":null,"poll_after":null},"chat_id":"-1001","chat_ids":null,"draft_id":null,"dry_run":true,"as_new":false})
    );
    assert_eq!(
        contracts::parse_post(&json!({"rich_message":{},"pin":"off"}))?["pin"],
        false
    );
    for bad in [
        json!({}),
        json!({"rich_message":{},"options":[]}),
        json!({"rich_message":{},"callbacks":1}),
        json!({"rich_message":{},"pin":"bad"}),
    ] {
        assert!(contracts::parse_post(&bad).is_err(), "{bad}");
    }
    let draft = &document["components"]["schemas"]["DraftReq"];
    for version in [json!("1"), json!(1.0), json!(true), json!("1.0")] {
        assert_eq!(
            contracts::normalize(
                document,
                draft,
                &json!({"post":{"arbitrary":[1,"2"]},"version":version})
            )?["version"],
            1
        );
    }
    let token = &document["components"]["schemas"]["TokenReq"];
    for expiry in [json!("1"), json!(1.0), json!(true)] {
        assert!(
            contracts::normalize(
                document,
                token,
                &json!({"name":"client","expires_in_days":expiry})
            )
            .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn http_normalizes_model_fields_before_dry_run_and_optimistic_update() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status,dry)=request(app.clone(),"POST","/api/send",Some(json!({"post":{"rich_message":{"markdown":"hello"},"pin":"false","options":{"disable_notification":"false"}},"chat_id":"@test","dry_run":"true"}))).await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(
        dry["requests"][0]["params"]["disable_notification"],
        "false"
    );
    let (_, draft) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"post":{"raw_data":"unchanged"}})),
    )
    .await;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let version = draft["version"].as_i64().ok_or("version")?;
    let (status, rejected) = request(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"post":{"raw_data":"must not overwrite"},"version":9223372036854775808u64})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    let (_, unchanged) = request(app.clone(), "GET", &format!("/api/drafts/{id}"), None).await;
    assert_eq!(unchanged["version"], version);
    assert_eq!(unchanged["payload"], draft["payload"]);
    let (status, updated) = request(
        app,
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"post":{"raw_data":"updated"},"version":version.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["version"], version + 1);
    Ok(())
}

#[tokio::test]
async fn mcp_post_model_is_checked_before_save_publish_and_preview() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for name in [
        "validate_post",
        "save_draft",
        "preview_post",
        "send_preview_to_me",
        "publish_post",
    ] {
        let result=rpc(app.clone(),"tools/call",json!({"name":name,"arguments":{"post":{"rich_message":{"markdown":"hello"},"options":[]},"dry_run":true}})).await;
        assert_eq!(result["result"]["isError"], true, "{name}: {result}");
    }
    let listed = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"list_drafts","arguments":{}}),
    )
    .await;
    assert_eq!(listed["result"]["content"], json!([]));
    let saved=rpc(app.clone(),"tools/call",json!({"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"hello"},"pin":"true"}}})).await;
    let id = saved["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("saved id")?;
    let both=rpc(app.clone(),"tools/call",json!({"name":"preview_post","arguments":{"draft_id":id,"post":{"rich_message":{"markdown":"different"}}}})).await;
    assert_eq!(both["result"]["isError"], true);
    let shown = rpc(
        app,
        "tools/call",
        json!({"name":"preview_post","arguments":{"draft_id":id}}),
    )
    .await;
    assert_eq!(shown["result"]["structuredContent"]["post"]["pin"], true);
    assert_eq!(
        shown["result"]["structuredContent"]["post"]["callbacks"],
        json!({})
    );
    Ok(())
}

#[tokio::test]
async fn optional_json_bodies_accept_empty_and_null_before_service_lookup() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for path in [
        "/api/posts/missing/pin",
        "/api/suggested-posts/missing/approve",
        "/api/suggested-posts/missing/decline",
    ] {
        for body in [None, Some(Value::Null)] {
            let (status, result) = request(app.clone(), "POST", path, body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {result}");
        }
    }
    Ok(())
}

#[test]
fn normalized_http_request_preserves_python_idempotency_fingerprint() -> TestResult {
    use rich_studio::services::{contracts, idempotency};
    let document = contracts::http_document()?;
    let input = json!({"post":{"rich_message":{"markdown":"hello"},"pin":"false","_studio":{"ignored":true}},"chat_id":"@channel","dry_run":0,"ignored_request":"discarded"});
    let normalized = contracts::normalize(
        document,
        &document["components"]["schemas"]["PublishReq"],
        &input,
    )?;
    // Exact Post.model_dump + PublishReq.model_dump from Python, including defaults.
    let python = json!({"post":{"rich_message":{"markdown":"hello"},"reply_markup":null,"options":{},"pin":false,"comment":null,"callbacks":{},"poll_after":null},"chat_id":"@channel","chat_ids":null,"draft_id":null,"dry_run":false,"as_new":false});
    assert_eq!(normalized, python);
    assert_eq!(
        idempotency::fingerprint("send", &normalized),
        idempotency::fingerprint("send", &python)
    );
    Ok(())
}

#[tokio::test]
async fn http_patch_preserves_omitted_fields_and_explicit_clear_values() -> TestResult {
    async fn echo(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
        axum::Json(body)
    }
    let app = Router::new()
        .route("/api/invite-links", axum::routing::patch(echo))
        .layer(axum::middleware::from_fn(
            rich_studio::api::validation::validate,
        ));
    let (status, omitted) = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        Some(json!({"invite_link":"https://t.me/+fixture"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(omitted, json!({"invite_link":"https://t.me/+fixture"}));
    let (status,cleared)=request(app,"PATCH","/api/invite-links",Some(json!({"invite_link":"https://t.me/+fixture","name":"","member_limit":"0","creates_join_request":"false"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cleared,
        json!({"invite_link":"https://t.me/+fixture","name":"","member_limit":0,"creates_join_request":false})
    );
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
    let contract: Value = serde_json::from_str(include_str!("../../src/mcp_tools/contract.json"))?;
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
