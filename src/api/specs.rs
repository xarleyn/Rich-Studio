use crate::security::ScopedState;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::spec::validator::Validator;

#[utoipa::path(get, path = "/api/spec", operation_id = "get_spec_api_spec_get", summary = "Сводка спецификации для редактора (rich-типы, методы, изменения)", tag = "spec", responses((status = 200, description = "Successful Response")))]
pub async fn summary(ScopedState(state): ScopedState) -> Response {
    Json(state.spec.read().await.summary()).into_response()
}

#[utoipa::path(get, path = "/api/spec/raw", operation_id = "get_spec_raw_api_spec_raw_get", summary = "Полный api.json", tag = "spec", responses((status = 200, description = "Successful Response")))]
pub async fn raw(ScopedState(state): ScopedState) -> Response {
    Json(state.spec.read().await.data.clone()).into_response()
}

#[utoipa::path(get, path = "/api/spec/type/{name}", operation_id = "get_spec_type_api_spec_type__name__get", summary = "Точные поля типа или метода Bot API из текущей спецификации", tag = "spec", responses((status = 200, description = "Successful Response")))]
pub async fn definition(ScopedState(state): ScopedState, Path(name): Path<String>) -> Response {
    let spec = state.spec.read().await;
    match spec.definition(&name) {
        Some(definition) => Json(definition).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("{name} нет в спецификации {}", spec.version())})),
        )
            .into_response(),
    }
}

#[utoipa::path(post, path = "/api/spec/refresh", operation_id = "refresh_spec_api_spec_refresh_post", summary = "Скачать свежую спецификацию и показать, что изменилось", tag = "spec", responses((status = 200, description = "Successful Response")))]
pub async fn refresh(ScopedState(state): ScopedState) -> Response {
    let report = crate::services::specs::refresh(&state, true).await;
    Json(report).into_response()
}

#[utoipa::path(post, path = "/api/validate", operation_id = "validate_api_validate_post", summary = "Проверить пост по спецификации: ошибки, предупреждения, символы", tag = "posts", responses((status = 200, description = "Successful Response")))]
pub async fn validate(ScopedState(state): ScopedState, Json(post): Json<Value>) -> Response {
    let spec = state.spec.read().await;
    Json(Validator::new(&spec).check_post(&post)).into_response()
}
