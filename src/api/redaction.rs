//! Scrub diagnostics at the HTTP boundary without changing successful user data.

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use serde_json::Value;

use crate::{AppState, logs, settings::Settings};

fn scrub(settings: &Settings, value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(logs::scrub(settings, &text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| scrub(settings, item))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (logs::scrub(settings, &key), scrub(settings, value)))
                .collect(),
        ),
        value => value,
    }
}

pub async fn errors(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if !(response.status().is_client_error() || response.status().is_server_error()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // Diagnostics have no reason to exceed the largest accepted draft. Keep a
    // bounded fallback even for a failed adapter that returns an unbounded body.
    let bytes = match to_bytes(body, 8 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            return Response::from_parts(
                parts,
                Body::from(r#"{"detail":"error response exceeds diagnostic limit"}"#),
            );
        }
    };
    let output = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => scrub(&state.settings, value).to_string().into_bytes(),
        Err(_) => match std::str::from_utf8(&bytes) {
            Ok(text) => logs::scrub(&state.settings, text).into_bytes(),
            Err(_) => bytes.to_vec(),
        },
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(output))
}
