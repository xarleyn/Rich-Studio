use crate::security::ScopedState;
use axum::Json;
use axum::extract::{Path, RawQuery};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[utoipa::path(get, path = "/api/spec", operation_id = "get_spec_api_spec_get", summary = "Сводка спецификации для редактора (rich-типы, методы, изменения)", tag = "spec", responses((status = 200, description = "Successful Response")))]
pub async fn summary(ScopedState(state): ScopedState) -> Response {
    let spec = state.spec.read().await;
    let mut summary = spec.summary();
    let contract = match crate::services::contracts::http_document() {
        Ok(contract) => contract,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    };
    let post = &contract["components"]["schemas"]["Post"];
    let fields = post["properties"].as_object().into_iter().flatten().map(|(name, field)| {
        json!({
            "name":name,
            "types":field.get("x-bot-api-types").cloned().unwrap_or_else(||json!([])),
            "required":post["required"].as_array().is_some_and(|required|required.iter().any(|field|field==name)),
            "description":field["description"],
            "enum":null,
            "range":null,
        })
    }).collect::<Vec<_>>();
    let mut envelope =
        json!({"root":"Post","types":{"Post":{"description":"Пост студии","fields":fields}}});
    for (name, method, omitted) in [
        (
            "PostOptions",
            "sendRichMessage",
            crate::services::publisher::RESERVED_PARAMS,
        ),
        ("PollAfter", "sendPoll", &["chat_id"][..]),
    ] {
        let fields = summary["methods"][method]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|field| !omitted.contains(&field["name"].as_str().unwrap_or("")))
            .cloned()
            .collect::<Vec<_>>();
        envelope["types"][name] =
            json!({"description":format!("Параметры {method}"),"fields":fields});
    }
    summary["post"] = envelope;
    Json(summary).into_response()
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
pub async fn validate(
    ScopedState(state): ScopedState,
    RawQuery(query): RawQuery,
    Json(post): Json<Value>,
) -> Response {
    let chat_ids = query.map(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .filter(|(key, _)| key == "chat_ids")
            .map(|(_, value)| value.into_owned())
            .collect()
    });
    match crate::services::validation_context::check(&state, &post, chat_ids, None).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":error})),
        )
            .into_response(),
    }
}
