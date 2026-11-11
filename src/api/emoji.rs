//! Custom emoji packs for the editor panel and preview pictures.

use axum::Json;
use axum::extract::{Path, rejection::JsonRejection};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::errors::StoreError;
use crate::security::ScopedState;
use crate::services::emoji::{self, EmojiError, EmojiService};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PackRequest {
    name: String,
}

fn service(state: &crate::AppState) -> EmojiService {
    emoji::from_state(state)
}

pub fn error_response(error: EmojiError) -> Response {
    let status = match &error {
        // The Python reference refuses a malformed request with 422, like every
        // other validated model.
        EmojiError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        EmojiError::NotFound(_) => StatusCode::NOT_FOUND,
        EmojiError::Upstream(_) => StatusCode::BAD_GATEWAY,
        EmojiError::Store(StoreError::StorageFull | StoreError::Quota(_)) => {
            StatusCode::PAYLOAD_TOO_LARGE
        }
        EmojiError::Store(_) | EmojiError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"detail": error.to_string()}))).into_response()
}

#[utoipa::path(get, path = "/api/emoji/packs", operation_id = "packs_list_api_emoji_packs_get", summary = "Паки кастомных эмодзи учётки для панели редактора", tag = "emoji", responses((status = 200, description = "Successful Response")))]
pub async fn packs_list(ScopedState(state): ScopedState) -> Response {
    let service = service(&state);
    match tokio::task::spawn_blocking(move || service.list_packs()).await {
        Ok(Ok(packs)) => Json(json!({"packs": packs})).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(post, path = "/api/emoji/packs", operation_id = "packs_add_api_emoji_packs_post", summary = "Добавить пак: короткое имя набора или ссылка t.me/addemoji/<код>", tag = "emoji", responses((status = 200, description = "Successful Response")))]
pub async fn packs_add(
    ScopedState(state): ScopedState,
    input: Result<Json<PackRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match input {
        Ok(input) => input,
        Err(error) => return error_response(EmojiError::Invalid(error.body_text())),
    };
    let service = service(&state);
    let added = match service.add_pack(&request.name).await {
        Ok(added) => added,
        Err(error) => return error_response(error),
    };
    match tokio::task::spawn_blocking(move || service.list_packs()).await {
        Ok(Ok(packs)) => Json(json!({"packs": packs, "added": added})).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(delete, path = "/api/emoji/packs/{name}", operation_id = "packs_remove_api_emoji_packs__name__delete", summary = "Убрать пак из панели; скачанные картинки остаются в кэше на случай возврата", tag = "emoji", responses((status = 200, description = "Successful Response")))]
pub async fn packs_remove(ScopedState(state): ScopedState, Path(name): Path<String>) -> Response {
    let service = service(&state);
    match tokio::task::spawn_blocking(move || {
        service.remove_pack(&name)?;
        service.list_packs()
    })
    .await
    {
        Ok(Ok(packs)) => Json(json!({"packs": packs})).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(get, path = "/api/emoji/img/{custom_emoji_id}", operation_id = "emoji_img_api_emoji_img__custom_emoji_id__get", summary = "Статичная картинка эмодзи (webp-миниатюра); скачивается при первом запросе", tag = "emoji", responses((status = 200, description = "Successful Response")))]
pub async fn img(ScopedState(state): ScopedState, Path(custom_emoji_id): Path<String>) -> Response {
    picture(service(&state), &custom_emoji_id, false).await
}

#[utoipa::path(get, path = "/api/emoji/file/{custom_emoji_id}", operation_id = "emoji_file_api_emoji_file__custom_emoji_id__get", summary = "Полный файл эмодзи: webm-видео, tgs-анимация или webp", tag = "emoji", responses((status = 200, description = "Successful Response")))]
pub async fn file(
    ScopedState(state): ScopedState,
    Path(custom_emoji_id): Path<String>,
) -> Response {
    picture(service(&state), &custom_emoji_id, true).await
}

async fn picture(service: EmojiService, custom_emoji_id: &str, full: bool) -> Response {
    let result = if full {
        service.full_file(custom_emoji_id).await
    } else {
        service.static_file(custom_emoji_id).await
    };
    let (path, media_type) = match result {
        Ok(file) => file,
        Err(error) => return error_response(error),
    };
    match tokio::fs::read(&path).await {
        Ok(data) => (
            [
                (header::CONTENT_TYPE, media_type.to_owned()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "sandbox; default-src 'none'".to_owned(),
                ),
                (
                    header::CACHE_CONTROL,
                    "private, immutable, max-age=31536000".to_owned(),
                ),
            ],
            data,
        )
            .into_response(),
        Err(error) => {
            let status = if error.kind() == std::io::ErrorKind::NotFound {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({"detail":"картинка эмодзи недоступна"}))).into_response()
        }
    }
}
