use crate::{
    errors::ImportError,
    security::ScopedState,
    services::{import_forward, imports},
};
use axum::{
    Json,
    extract::{Path, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ImportInput {
    pub url: String,
}
fn respond(result: Result<Value, ImportError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"detail":error.detail})),
        )
            .into_response(),
    }
}
#[utoipa::path(get, path = "/api/imports/telegram", operation_id = "import_status_api_imports_telegram_get", summary = "Доступные способы импорта из Telegram", description = "Только состояние подключения текущей учётки, без секретов и сессий.", tag = "imports", responses((status = 200, description = "Successful Response")))]
pub async fn status(ScopedState(state): ScopedState) -> Response {
    let settings = state.settings;
    let context = state.account;
    respond(
        tokio::task::spawn_blocking(move || Ok(imports::status(&settings, &context)))
            .await
            .unwrap_or_else(|_| Err(ImportError::new(500, "Проверка импорта прервана."))),
    )
}
#[utoipa::path(post, path = "/api/imports/telegram", operation_id = "import_telegram_api_imports_telegram_post", summary = "Импортировать сообщение Telegram в новый черновик", description = "Проверяет ссылку, читает сообщение и сохраняет визуальные блоки, HTML и Rich Markdown.\n\nНичего не публикует. MTProto использует сессию текущей учётки либо бота; без подключения пробует\nпубличный веб-виджет. Заглушка, приватная ссылка без сессии и неполный rich-пост дают ошибку.\nПотери преобразования перечислены в warnings. Существующие черновики не меняются.", tag = "imports", responses((status = 200, description = "Successful Response")))]
pub async fn import_post(
    ScopedState(state): ScopedState,
    input: Result<Json<ImportInput>, JsonRejection>,
) -> Response {
    match input {
        Ok(Json(input)) => {
            respond(imports::import_post(&state.settings, &state.account, &input.url).await)
        }
        Err(error) => respond(Err(ImportError::new(422, error.body_text()))),
    }
}
#[utoipa::path(post, path = "/api/imports/forward", operation_id = "begin_forward_api_imports_forward_post", summary = "Подготовить импорт выбранной пересылки в личном чате бота", description = "Откройте bot_url, нажмите Start и перешлите сообщение. Запрос действует 10 минут, принимает\nтолько следующую пересылку от текущего пользователя. Результат: GET /api/imports/forward/{id}.", tag = "imports", responses((status = 200, description = "Successful Response")))]
pub async fn forward_begin(
    ScopedState(state): ScopedState,
    input: Result<Json<ImportInput>, JsonRejection>,
) -> Response {
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => return respond(Err(ImportError::new(422, error.body_text()))),
    };
    respond(
        tokio::task::spawn_blocking(move || {
            import_forward::begin(&state.settings, &state.account, &input.url)
        })
        .await
        .unwrap_or_else(|_| Err(ImportError::new(500, "Начало импорта прервано."))),
    )
}
#[utoipa::path(get, path = "/api/imports/forward/{token}", operation_id = "forward_status_api_imports_forward__token__get", summary = "Результат импорта пересланного сообщения текущей учётки", tag = "imports", responses((status = 200, description = "Successful Response")))]
pub async fn forward_get(ScopedState(state): ScopedState, Path(token): Path<String>) -> Response {
    respond(
        tokio::task::spawn_blocking(move || import_forward::get(&state.account, &token))
            .await
            .unwrap_or_else(|_| Err(ImportError::new(500, "Проверка импорта прервана."))),
    )
}
