use crate::security::ScopedState;
use axum::Json;
use axum::extract::{Multipart, Path, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::services::media::{MediaError, MediaService, inline};

#[derive(Deserialize)]
pub struct ForceQuery {
    #[serde(default)]
    force: bool,
}

fn service(state: &AppState) -> MediaService {
    MediaService::new(
        state.account.clone(),
        state.settings.max_upload_mb,
        state.settings.account_storage_mb,
    )
}

pub fn error_response(error: MediaError) -> Response {
    let status = match &error {
        MediaError::NotFound => StatusCode::NOT_FOUND,
        MediaError::BadRequest(_) | MediaError::Fetch(_) => StatusCode::BAD_REQUEST,
        MediaError::TooLarge | MediaError::Capacity => StatusCode::PAYLOAD_TOO_LARGE,
        MediaError::Store(
            crate::errors::StoreError::StorageFull | crate::errors::StoreError::Quota(_),
        ) => StatusCode::PAYLOAD_TOO_LARGE,
        MediaError::Conflict(_) => StatusCode::CONFLICT,
        MediaError::Io(_) | MediaError::Sqlite(_) | MediaError::Store(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    // The reference API returns the list of users so the editor can show them.
    let detail = match &error {
        MediaError::Conflict(users) => json!({"message":error.to_string(),"used_by":users}),
        _ => json!(error.to_string()),
    };
    (status, Json(json!({"detail":detail}))).into_response()
}

#[utoipa::path(get, path = "/api/media", operation_id = "media_list_api_media_get", summary = "Загруженные файлы; used — нужен ли файл черновику или посту", tag = "media", responses((status = 200, description = "Successful Response")))]
pub async fn list(ScopedState(state): ScopedState) -> Response {
    match tokio::task::spawn_blocking(move || service(&state).list()).await {
        Ok(Ok(files)) => Json(files).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(post, path = "/api/media", operation_id = "media_upload_api_media_post", summary = "Загрузить файл; в посте он указывается как upload:<name>", tag = "media", responses((status = 200, description = "Successful Response")))]
pub async fn upload(ScopedState(state): ScopedState, mut multipart: Multipart) -> Response {
    let limit = (state.settings.max_upload_mb * 1024.0 * 1024.0) as usize;
    let mut filename = None;
    let mut data = Vec::new();
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() != Some("file") {
                    continue;
                }
                filename = field.file_name().map(str::to_owned);
                match field.bytes().await {
                    Ok(bytes) => data.extend_from_slice(&bytes),
                    Err(error) => {
                        return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
                    }
                }
                if data.len() > limit {
                    return error_response(MediaError::TooLarge);
                }
            }
            Ok(None) => break,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        }
    }
    let Some(filename) = filename else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"file: обязательное поле"})),
        )
            .into_response();
    };
    match tokio::task::spawn_blocking(move || service(&state).upload(&filename, &data)).await {
        Ok(Ok(file)) => Json(file).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(post, path = "/api/media/fetch", operation_id = "media_fetch_api_media_fetch_post", summary = "Скачать файл по ссылке в загрузки; дальше он upload:<name>", description = "Ходит только на публичные адреса и проверяет каждый редирект; локальная сеть: `MEDIA_FETCH_PRIVATE=1`.", tag = "media", responses((status = 200, description = "Successful Response")))]
pub async fn fetch(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    let service = service(&state);
    match service
        .fetch(
            body.get("url").and_then(Value::as_str).unwrap_or_default(),
            body.get("filename").and_then(Value::as_str),
            false,
            state.settings.media_fetch_private,
        )
        .await
    {
        Ok(file) => Json(file).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(post, path = "/api/media/cleanup", operation_id = "media_cleanup_api_media_cleanup_post", summary = "Удалить файлы, которые не упоминает ни один черновик и ни один живой пост", tag = "media", responses((status = 200, description = "Successful Response")))]
pub async fn cleanup(ScopedState(state): ScopedState) -> Response {
    match tokio::task::spawn_blocking(move || service(&state).delete_unused()).await {
        Ok(Ok(deleted)) => Json(json!({"deleted":deleted})).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[utoipa::path(delete, path = "/api/media/{name}", operation_id = "media_delete_api_media__name__delete", summary = "Удалить файл; если он нужен отложенному посту или черновику, 409 (force=true удаляет всё равно)", tag = "media", responses((status = 200, description = "Successful Response")))]
pub async fn delete(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
    Query(query): Query<ForceQuery>,
) -> Response {
    match tokio::task::spawn_blocking(move || service(&state).delete(&name, query.force)).await {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

pub async fn file(ScopedState(state): ScopedState, Path(name): Path<String>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let service = service(&state);
        let path = service.path_of(&name)?;
        let data = service.download(&name)?;
        Ok::<_, MediaError>((path, data))
    })
    .await;
    match result {
        Ok(Ok((path, data))) => {
            let content_type = if inline(path.to_string_lossy().as_ref()) {
                match path
                    .extension()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
                    .as_str()
                {
                    "jpg" | "jpeg" => "image/jpeg",
                    "png" => "image/png",
                    "webp" => "image/webp",
                    "gif" => "image/gif",
                    "mp4" => "video/mp4",
                    "mov" => "video/quicktime",
                    "webm" => "video/webm",
                    "mp3" => "audio/mpeg",
                    "m4a" => "audio/mp4",
                    "ogg" | "oga" | "opus" => "audio/ogg",
                    "wav" => "audio/wav",
                    "flac" => "audio/flac",
                    _ => "application/octet-stream",
                }
            } else {
                "application/octet-stream"
            };
            let disposition = if content_type == "application/octet-stream" {
                format!(
                    "attachment; filename=\"{}\"",
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .replace('"', "")
                )
            } else {
                "inline".to_owned()
            };
            (
                [
                    (header::CONTENT_TYPE, content_type.to_owned()),
                    (header::CONTENT_DISPOSITION, disposition),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "sandbox; default-src 'none'".to_owned(),
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                ],
                data,
            )
                .into_response()
        }
        Ok(Err(error)) => error_response(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}
