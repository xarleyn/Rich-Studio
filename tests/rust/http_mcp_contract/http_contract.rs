use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;

use crate::support::{TestResult, app, request, rpc};

#[tokio::test]
async fn all_legacy_http_operations_and_mcp_schemas_survive_the_sdk_adapter() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status, schema) = request(app.clone(), "GET", "/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    let original: Value = serde_json::from_str(include_str!("../../../docs/openapi.json"))?;
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
    let original: Value =
        serde_json::from_str(include_str!("../../../src/mcp_tools/contract.json"))?;
    assert_eq!(listed["result"]["tools"], original["tools"]);
    assert_eq!(
        listed["result"]["tools"].as_array().ok_or("tools")?.len(),
        39
    );
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap_or_default(), tool))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        tools["publish_post"]["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(tools["validate_post"]["annotations"]["readOnlyHint"], true);
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
