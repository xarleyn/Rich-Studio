//! The editor's account-local template API.

use axum::Json;
use axum::extract::{
    Path, Query,
    rejection::{JsonRejection, QueryRejection},
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::StoreError;
use crate::security::ScopedState;
use crate::services::templates::{self, FillInput, TemplateError, TemplateInput, TemplateService};

pub fn error_response(error: TemplateError) -> Response {
    let status = match &error {
        TemplateError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        TemplateError::Conflict(_) => StatusCode::CONFLICT,
        TemplateError::NotFound => StatusCode::NOT_FOUND,
        TemplateError::TooLarge(_)
        | TemplateError::Store(StoreError::StorageFull | StoreError::Quota(_)) => {
            StatusCode::PAYLOAD_TOO_LARGE
        }
        TemplateError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"detail":error.to_string()}))).into_response()
}

fn json_input<T>(input: Result<Json<T>, JsonRejection>) -> Result<T, TemplateError> {
    input
        .map(|Json(value)| value)
        .map_err(|error| TemplateError::Invalid(error.body_text()))
}

fn respond(result: Result<Value, TemplateError>, status: StatusCode) -> Response {
    match result {
        Ok(result) => (status, Json(result)).into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(get, path = "/api/templates", operation_id = "list_templates_api_templates_get", summary = "Шаблоны постов текущей учётки и их заполняемые поля", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn list(ScopedState(state): ScopedState) -> Response {
    respond(
        templates::run(state.account, |service| Ok(json!(service.list()?))).await,
        StatusCode::OK,
    )
}

#[utoipa::path(get, path = "/api/templates/{tid}", operation_id = "get_template_api_templates__tid__get", summary = "Шаблон целиком: пост, поля и version", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn get(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    respond(
        templates::run(state.account, move |service| service.get(&id)).await,
        StatusCode::OK,
    )
}

#[utoipa::path(post, path = "/api/templates", operation_id = "create_template_api_templates_post", summary = "Сохранить пост как шаблон с переменными {{key}}", description = "Переменные заменяются только в строковых значениях. Все переменные должны иметь описание в fields.", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn create(
    ScopedState(state): ScopedState,
    input: Result<Json<TemplateInput>, JsonRejection>,
) -> Response {
    let input = match json_input(input) {
        Ok(input) => input,
        Err(error) => return error_response(error),
    };
    respond(
        templates::run(state.account, move |service| service.save(None, input)).await,
        StatusCode::CREATED,
    )
}

#[utoipa::path(put, path = "/api/templates/{tid}", operation_id = "update_template_api_templates__tid__put", summary = "Изменить шаблон; version защищает от перезаписи чужой правки", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn update(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    input: Result<Json<TemplateInput>, JsonRejection>,
) -> Response {
    let input = match json_input(input) {
        Ok(input) => input,
        Err(error) => return error_response(error),
    };
    respond(
        templates::run(state.account, move |service| service.save(Some(&id), input)).await,
        StatusCode::OK,
    )
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    version: i64,
}

#[utoipa::path(delete, path = "/api/templates/{tid}", operation_id = "delete_template_api_templates__tid__delete", summary = "Удалить шаблон; созданные из него черновики остаются", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn delete(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(error) => return error_response(TemplateError::Invalid(error.body_text())),
    };
    respond(
        templates::run(state.account, move |service| {
            service.delete(&id, query.version)?;
            Ok(json!({"ok":true}))
        })
        .await,
        StatusCode::OK,
    )
}

#[utoipa::path(post, path = "/api/templates/{tid}/drafts", operation_id = "create_draft_api_templates__tid__drafts_post", summary = "Заполнить поля шаблона и создать новый черновик", description = "Проверяет обязательные поля и URL; экранирует значения для HTML/Markdown. Не публикует пост и не копирует канал.", tag = "templates", responses((status = 200, description = "Successful Response")))]
pub async fn create_draft(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    input: Result<Json<FillInput>, JsonRejection>,
) -> Response {
    let input = match json_input(input) {
        Ok(input) => input,
        Err(error) => return error_response(error),
    };
    respond(
        templates::run(state.account, move |service: &TemplateService| {
            service.create_draft(&id, input)
        })
        .await,
        StatusCode::CREATED,
    )
}
