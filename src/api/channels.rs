use crate::security::ScopedState;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::services::channels::{ChannelError, ChannelService};
use crate::telegram::TelegramError;

#[derive(Deserialize)]
pub struct ChatQuery {
    chat_id: Option<String>,
}

pub fn service(state: &AppState) -> ChannelService {
    crate::services::channels::from_state(state)
}

pub fn telegram_error(error: &TelegramError) -> StatusCode {
    match error {
        TelegramError::Refused {
            code: Some(400), ..
        } => StatusCode::UNPROCESSABLE_ENTITY,
        TelegramError::Policy(_) => StatusCode::FORBIDDEN,
        TelegramError::MissingUpload(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

pub fn error_response(error: ChannelError) -> Response {
    let status = match &error {
        ChannelError::BadRequest(_) => StatusCode::BAD_REQUEST,
        ChannelError::Forbidden(_) => StatusCode::FORBIDDEN,
        ChannelError::NotFound(_) => StatusCode::NOT_FOUND,
        ChannelError::Telegram(error) => telegram_error(error),
        ChannelError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // An uncertain gateway answer is not a refusal: say so, like the reference API's 502 handler.
    let detail = if matches!(&error, ChannelError::Telegram(TelegramError::NoAnswer(_))) {
        format!("{error}. Действие могло выполниться, проверьте в Telegram")
    } else {
        error.to_string()
    };
    (status, Json(json!({"detail":detail}))).into_response()
}

#[utoipa::path(get, path = "/api/channels", operation_id = "list_channels_api_channels_get", summary = "Каналы студии с названиями, темами форума и правами бота", tag = "channels", responses((status = 200, description = "Successful Response")))]
pub async fn list(ScopedState(state): ScopedState) -> Response {
    match service(&state).run(|service| service.list()).await {
        Ok(channels) => Json(channels).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(post, path = "/api/channels", operation_id = "check_channel_api_channels_post", summary = "Добавить канал или проверить его заново: название, форум, права бота", tag = "channels", responses((status = 200, description = "Successful Response")))]
pub async fn check(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    match service(&state)
        .check(body.get("chat_id").cloned().unwrap_or(Value::Null))
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(get, path = "/api/check", operation_id = "check_api_check_get", summary = "Бот админ канала? Есть права на публикацию? Канал запоминается с названием", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn check_query(
    ScopedState(state): ScopedState,
    Query(query): Query<ChatQuery>,
) -> Response {
    let chat = Value::String(
        query
            .chat_id
            .filter(|chat| !chat.is_empty())
            .unwrap_or_else(|| state.settings.default_chat.clone()),
    );
    match service(&state).check(chat).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(put, path = "/api/channels/{chat_id}/topics", operation_id = "set_topics_api_channels__chat_id__topics_put", summary = "Темы форума вручную: Bot API не умеет их перечислять", description = "Темы приходят и сами, из событий создания темы.", tag = "channels", responses((status = 200, description = "Successful Response")))]
pub async fn topics(
    ScopedState(state): ScopedState,
    Path(chat_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let topics = body
        .get("topics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match service(&state)
        .run(move |service| service.set_topics(&chat_id, topics))
        .await
    {
        Ok(channel) => Json(channel).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(delete, path = "/api/channels/{chat_id}", operation_id = "remove_channel_api_channels__chat_id__delete", summary = "Убрать канал из списка (в Telegram ничего не меняется)", tag = "channels", responses((status = 200, description = "Successful Response")))]
pub async fn remove(ScopedState(state): ScopedState, Path(chat_id): Path<String>) -> Response {
    match service(&state)
        .run(move |service| service.remove(&chat_id))
        .await
    {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => error_response(error),
    }
}
