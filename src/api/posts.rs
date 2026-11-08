use crate::security::ScopedState;
use axum::Json;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde_json::{Value, json};

use crate::AppState;
use crate::errors::{ServiceError, StoreError};
use crate::services::idempotency::Idempotency;
use crate::services::kinds;
use crate::services::posts::PostService;
use crate::telegram::TelegramError;

pub async fn service(state: &AppState) -> PostService {
    crate::services::posts::from_state(state).await
}

pub fn error_response(error: ServiceError) -> Response {
    let (status, detail) = match error {
        ServiceError::BadRequest(message) => (StatusCode::BAD_REQUEST, json!(message)),
        ServiceError::Unprocessable(message) => (StatusCode::UNPROCESSABLE_ENTITY, json!(message)),
        ServiceError::Invalid(detail) => (StatusCode::UNPROCESSABLE_ENTITY, detail),
        ServiceError::Conflict(message) => (StatusCode::CONFLICT, json!(message)),
        ServiceError::NotFound(message) => (StatusCode::NOT_FOUND, json!(message)),
        ServiceError::Upstream(detail) | ServiceError::MayHaveGone(detail) => {
            (StatusCode::BAD_GATEWAY, detail)
        }
        ServiceError::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, json!(message)),
        ServiceError::Store(StoreError::StorageFull) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!(StoreError::StorageFull.to_string()),
        ),
        ServiceError::Store(StoreError::Quota(message)) => {
            (StatusCode::PAYLOAD_TOO_LARGE, json!(message))
        }
        ServiceError::Store(error) => (StatusCode::INTERNAL_SERVER_ERROR, json!(error.to_string())),
        ServiceError::Telegram(error) => {
            let status = super::channels::telegram_error(&error);
            let retry_after = match &error {
                TelegramError::RateLimited { retry_after, .. } => *retry_after,
                _ => None,
            };
            let detail = if matches!(error, TelegramError::Unreachable(_)) {
                format!("Telegram недоступен, пост не отправлен: {error}")
            } else {
                error.to_string()
            };
            let mut body = json!({"detail":detail});
            if let Some(wait) = retry_after {
                body["retry_after"] = json!(wait);
            }
            return (status, Json(body)).into_response();
        }
    };
    (status, Json(json!({"detail":detail}))).into_response()
}

fn key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
}

fn actor(state: &AppState, headers: &HeaderMap) -> String {
    let Ok(pattern) = Regex::new(r"^[A-Za-z0-9._:-]{1,40}$") else {
        return state.principal.actor("");
    };
    let client = headers
        .get("x-studio-client")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| pattern.is_match(value))
        .unwrap_or("");
    state.principal.actor(client)
}

fn send_body(body: &Value) -> Value {
    json!({
        "post":crate::services::posts::normalize_post(&body["post"]),"chat_id":body["chat_id"],"chat_ids":body["chat_ids"],"draft_id":body["draft_id"],
        "dry_run":body["dry_run"].as_bool().unwrap_or(false),"as_new":body["as_new"].as_bool().unwrap_or(false),
    })
}

#[utoipa::path(post, path = "/api/send", operation_id = "send_api_send_post", summary = "Опубликовать пост (sendRichMessage)", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn send(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let service = service(&state).await;
    let normalized = send_body(&body);
    let actor = actor(&state, &headers);
    let action_actor = actor.clone();
    let action_body = normalized.clone();
    let dry_run = normalized["dry_run"].as_bool().unwrap_or(false);
    let as_new = normalized["as_new"].as_bool().unwrap_or(false);
    let action = || async move {
        service
            .send(
                &action_body["post"],
                &action_body["chat_id"],
                &action_body["chat_ids"],
                action_body["draft_id"].as_str(),
                &action_actor,
                dry_run,
                as_new,
            )
            .await
    };
    let result = if dry_run {
        action().await
    } else {
        Idempotency::new(state.account.clone())
            .run(key(&headers), "send", &normalized, &actor, action)
            .await
    };
    match result {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(post, path = "/api/preview-send", operation_id = "preview_send_api_preview_send_post", summary = "Отправить пост себе в личку (TG_PREVIEW_CHAT)", description = "В ответе `parsed`: блоки, на которые Telegram разобрал пост, и они же для повторной отправки (`input_blocks`).\n`chat_id`, отличный от `TG_PREVIEW_CHAT`, требует права `publish`: это уже отправка в чужой чат.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn preview(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    let service = service(&state).await;
    if !body["chat_id"].is_null() && body["chat_id"] != "" {
        let preview = state.settings.preview_chat.clone();
        let same_chat = !preview.is_empty()
            && match (
                service.resolve_chat(&body["chat_id"]),
                service.resolve_chat(&json!(preview)),
            ) {
                (Ok(explicit), Ok(target)) => explicit == target,
                _ => false,
            };
        if !same_chat && !state.principal.can(crate::account_context::Role::Publish) {
            return crate::security::AuthError::new(
                403,
                "для предпросмотра в другом чате нужны права publish",
            )
            .into_response();
        }
    }
    match service.preview(&body["post"], &body["chat_id"]).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(post, path = "/api/schedule", operation_id = "schedule_api_schedule_post", summary = "Отложить публикацию", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn schedule(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let service = service(&state).await;
    let mut normalized = send_body(&body);
    normalized["at"] = body["at"].clone();
    let actor = actor(&state, &headers);
    let action_actor = actor.clone();
    let action_body = normalized.clone();
    let dry_run = normalized["dry_run"].as_bool().unwrap_or(false);
    let as_new = normalized["as_new"].as_bool().unwrap_or(false);
    let action = || async move {
        service
            .run(move |service| {
                service.schedule(
                    &action_body["post"],
                    &action_body["at"],
                    &action_body["chat_id"],
                    &action_body["chat_ids"],
                    action_body["draft_id"].as_str(),
                    &action_actor,
                    dry_run,
                    as_new,
                )
            })
            .await
    };
    let result = if dry_run {
        action().await
    } else {
        Idempotency::new(state.account.clone())
            .run(key(&headers), "schedule", &normalized, &actor, action)
            .await
    };
    match result {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(post, path = "/api/poll", operation_id = "send_poll_api_poll_post", summary = "Опрос или викторина в канал (sendPoll), сразу или отложенно", description = "Опрос сразу после поста — поле `poll_after` в самом посте: оно проверяется по `sendPoll` до отправки поста.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn poll(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    send_kind(state, headers, body, kinds::POLL).await
}

#[utoipa::path(post, path = "/api/media-post", operation_id = "send_media_api_media_post_post", summary = "Live Photo (sendLivePhoto) или платный контент за звёзды (sendPaidMedia), сразу или отложенно", description = "Отдельное сообщение, не rich-пост: в блоках `InputRichMessage` таких медиа нет. Платный контент требует,\nчтобы в канале была разрешена публикация платных медиа (`can_send_paid_media`).", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn media(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let kind = body["kind"].as_str().unwrap_or_default().to_owned();
    if ![kinds::LIVE_PHOTO, kinds::PAID_MEDIA].contains(&kind.as_str()) {
        return error_response(ServiceError::Unprocessable(
            "kind: live_photo или paid_media".to_owned(),
        ));
    }
    send_kind(state, headers, body, &kind).await
}

#[utoipa::path(post, path = "/api/widget", operation_id = "send_widget_api_widget_post", summary = "HTML-виджет в чат (sendWidget), сразу или отложенно", description = "Отдельное сообщение: HTML со встроенными CSS и JS. После отправки HTML неизменяем — меняются только данные (`editMessageWidgetData`). Каналы не входят в прямую отправку (Bot API 10.4): личный чат, обычная группа или супергруппа. Лимиты: исходник 32 768 байт, после сжатия zlib level 9 — 4 096; `chat_data`/`public_data` — по 1 024 байта.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn widget(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    send_kind(state, headers, body, kinds::WIDGET).await
}

#[utoipa::path(post, path = "/api/widget/validate", operation_id = "widget_validate_api_widget_validate_post", summary = "Проверить виджет до отправки: размеры и запрещённые возможности", description = "В ответе `ok`, `errors`, `warnings`, `size_raw` (байт UTF-8) и `size_compressed` (после zlib level 9). Ничего не отправляется и не сохраняется.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn widget_validate(Json(body): Json<Value>) -> Response {
    Json(crate::services::widgets::report(&body["widget"])).into_response()
}

async fn send_kind(state: AppState, headers: HeaderMap, body: Value, kind_name: &str) -> Response {
    let service = service(&state).await;
    let kind = match kinds::get(Some(kind_name)) {
        Ok(kind) => kind,
        Err(error) => return error_response(error),
    };
    let params_key = match kind_name {
        kinds::POLL => "poll",
        kinds::WIDGET => "widget",
        _ => "params",
    };
    let mut normalized = json!({
        params_key:body[params_key],"chat_id":body["chat_id"],"chat_ids":body["chat_ids"],"at":body["at"],
        "pin":body["pin"].as_bool().unwrap_or(false),
    });
    if kind_name != kinds::POLL {
        normalized["kind"] = body["kind"].clone();
    }
    let scope = match kind_name {
        kinds::POLL => "poll",
        kinds::WIDGET => "widget",
        _ => "media-post",
    };
    let actor = actor(&state, &headers);
    let action_actor = actor.clone();
    let action_body = normalized.clone();
    let result = Idempotency::new(state.account.clone())
        .run(key(&headers), scope, &normalized, &actor, || async move {
            service
                .send_kind(
                    kind,
                    &action_body[params_key],
                    &action_body["chat_id"],
                    &action_body["chat_ids"],
                    &action_body["at"],
                    action_body["pin"].as_bool().unwrap_or(false),
                    &action_actor,
                )
                .await
        })
        .await;
    match result {
        Ok(result) => Json(result).into_response(),
        Err(error) => error_response(error),
    }
}
