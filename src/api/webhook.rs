use crate::{
    AppState,
    security::ScopedState,
    services::{auth, updates},
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[utoipa::path(get, path = "/api/updates", operation_id = "updates_status_api_updates_get", summary = "Приём событий от Telegram: нажатия, комментарии, реакции, опросы", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn status(ScopedState(state): ScopedState) -> Json<Value> {
    Json(updates::public_status(&state).await)
}
pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let settings = &state.settings;
    if !settings.updates_on
        || settings.updates_mode != "webhook"
        || settings.webhook_secret.is_empty()
    {
        return (StatusCode::NOT_FOUND, Json(json!({"detail":"Not found"}))).into_response();
    }
    let secret = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|header| header.to_str().ok())
        .unwrap_or("");
    if !auth::equal_secret(secret, &settings.webhook_secret) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"Неверный секрет webhook"})),
        )
            .into_response();
    }
    let Ok(Json(update)) = body else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"Неверный JSON"})),
        )
            .into_response();
    };
    if !update.is_object() || update["update_id"].as_i64().is_none_or(|id| id < 0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"Неверный update_id"})),
        )
            .into_response();
    }
    match updates::receive_webhook(&state, &update).await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => super::posts::error_response(error),
    }
}
