use crate::security::ScopedState;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::AppState;
use crate::errors::StoreError;
use crate::services::drafts::{DraftError, DraftService, SaveDraft};

pub fn error_response(error: DraftError) -> Response {
    let (status, detail) = match error {
        DraftError::NotFound(_) => (StatusCode::NOT_FOUND, json!("Not Found")),
        DraftError::Invalid(message) => (StatusCode::BAD_REQUEST, json!(message)),
        DraftError::Conflict { current } => (
            StatusCode::CONFLICT,
            json!({"message": if current.is_some() { "черновик изменили в другой вкладке или агентом" } else { "черновик удалили в другой вкладке" }, "current": current}),
        ),
        DraftError::Store(StoreError::StorageFull) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!(StoreError::StorageFull.to_string()),
        ),
        DraftError::Store(StoreError::Quota(message)) => {
            (StatusCode::PAYLOAD_TOO_LARGE, json!(message))
        }
        DraftError::Store(error) => (StatusCode::INTERNAL_SERVER_ERROR, json!(error.to_string())),
    };
    (status, Json(json!({"detail": detail}))).into_response()
}

pub async fn run<T, F>(state: AppState, action: F) -> Result<T, DraftError>
where
    T: Send + 'static,
    F: FnOnce(&DraftService) -> Result<T, DraftError> + Send + 'static,
{
    crate::services::drafts::run(state.account, action).await
}

#[utoipa::path(get, path = "/api/drafts", operation_id = "drafts_api_drafts_get", summary = "Черновики студии, последние сверху", tag = "drafts", responses((status = 200, description = "Successful Response")))]
pub async fn list(ScopedState(state): ScopedState) -> Response {
    match run(state, move |service| service.list()).await {
        Ok(drafts) => Json(drafts).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(get, path = "/api/drafts/{did}", operation_id = "draft_api_drafts__did__get", summary = "Черновик целиком: пост, название, version, каналы", tag = "drafts", responses((status = 200, description = "Successful Response")))]
pub async fn get(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    match run(state, move |service| service.get(&id)).await {
        Ok(draft) => Json(draft).into_response(),
        Err(error) => error_response(error),
    }
}

fn save_input(id: Option<String>, body: Value) -> SaveDraft {
    SaveDraft {
        id,
        title: body
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        post: body.get("post").cloned().unwrap_or(Value::Null),
        version: body.get("version").and_then(Value::as_i64),
        post_id: body
            .get("post_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        chat_ids: body.get("chat_ids").and_then(Value::as_array).map(|chats| {
            chats
                .iter()
                .map(|chat| match chat {
                    Value::String(chat) => chat.clone(),
                    other => other.to_string(),
                })
                .collect()
        }),
    }
}

#[utoipa::path(post, path = "/api/drafts", operation_id = "create_draft_api_drafts_post", summary = "Новый черновик; он сразу открывается в редакторе", tag = "drafts", responses((status = 200, description = "Successful Response")))]
pub async fn create(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    match run(state, move |service| service.save(save_input(None, body))).await {
        Ok(draft) => Json(draft).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(put, path = "/api/drafts/{did}", operation_id = "put_draft_api_drafts__did__put", summary = "Сохранить черновик; с version — 409, если его сохранили в другом месте", description = "`chat_ids` — каналы черновика: публикация из него без `chat_id` уходит туда. Без поля остаются прежние.", tag = "drafts", responses((status = 200, description = "Successful Response")))]
pub async fn update(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match run(state, move |service| {
        service.save(save_input(Some(id), body))
    })
    .await
    {
        Ok(draft) => Json(draft).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(delete, path = "/api/drafts/{did}", operation_id = "del_draft_api_drafts__did__delete", summary = "Удалить черновик", tag = "drafts", responses((status = 200, description = "Successful Response")))]
pub async fn delete(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    match run(state, move |service| service.delete(&id)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => error_response(error),
    }
}
