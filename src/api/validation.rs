//! Enforce the preserved JSON input contracts before entering Value-based adapters.
use crate::{AppState, services::contracts};
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{MatchedPath, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
fn same_path(left: &str, right: &str) -> bool {
    let left = left.split('/').collect::<Vec<_>>();
    let right = right.split('/').collect::<Vec<_>>();
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            *left == right || (left.starts_with('{') && right.starts_with('{'))
        })
}
fn failure(detail: String) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail":[{"type":"value_error","loc":["body"],"msg":detail}]})),
    )
        .into_response()
}
pub async fn validate(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let Ok(document) = contracts::http_document() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let method = request.method().as_str().to_lowercase();
    let schema = document["paths"]
        .as_object()
        .and_then(|paths| paths.iter().find(|(path, _)| same_path(path, &route)))
        .and_then(|(_, operations)| operations.get(&method))
        .and_then(|operation| operation.get("requestBody"));
    let Some(body_schema) = schema else {
        return next.run(request).await;
    };
    let Some(schema) = body_schema["content"]["application/json"].get("schema") else {
        return next.run(request).await;
    };
    let limit = request
        .extensions()
        .get::<AppState>()
        .map_or(2 * 1024 * 1024, |state| {
            ((state.settings.max_upload_mb * 1024.0 * 1024.0 * 1.4) as usize)
                .saturating_add(2 * 1024 * 1024)
        });
    let (mut parts, body) = request.into_parts();
    let bytes = match to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"detail":"request body is too large"})),
            )
                .into_response();
        }
    };
    let value = if bytes.is_empty() {
        if body_schema["required"] == true {
            return failure("body: required".to_owned());
        }
        contracts::empty_body(document, schema)
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Null) if body_schema["required"] != true => {
                contracts::empty_body(document, schema)
            }
            Ok(value) => value,
            Err(_) => return failure("body: invalid JSON".to_owned()),
        }
    };
    let value = match contracts::normalize_present(document, schema, &value) {
        Ok(value) => value,
        Err(error) => return failure(error),
    };
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}
