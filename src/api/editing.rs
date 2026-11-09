use axum::Json;
use axum::extract::{Path, Query};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::errors::ServiceError;
use crate::security::ScopedState;
use crate::services::editing::EditingService;
use crate::services::{blocking, listing as lists};

async fn service(state: &AppState) -> EditingService {
    EditingService::new(
        state.account.clone(),
        state.transport(),
        &state.settings,
        state.spec.read().await.clone(),
    )
}

fn response(result: Result<Value, ServiceError>) -> Response {
    match result {
        Ok(result) => Json(result).into_response(),
        Err(error) => super::posts::error_response(error),
    }
}

fn actor(state: &AppState, headers: &HeaderMap) -> String {
    let client = headers
        .get("x-studio-client")
        .and_then(|header| header.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 40
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._:-".contains(character)
                })
        })
        .unwrap_or("");
    state.principal.actor(client)
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
    status: Option<String>,
    chat: Option<String>,
}

#[derive(Deserialize)]
pub struct VersionQuery {
    post_version: Option<i64>,
}

#[utoipa::path(get, path = "/api/posts", operation_id = "list_posts_api_posts_get", summary = "Опубликованные, отложенные и неудачные посты: первая строка, миниатюра, ссылка", description = "Для каждого поста: `title`, `thumb`, `link`, нажатия (`clicks`) и текст кнопки каждого `callback_data`\n(`buttons`). Отправка, которой Telegram отказал, — статус `failed` с ошибкой.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn listing(ScopedState(state): ScopedState, Query(query): Query<ListQuery>) -> Response {
    let limit = query.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return response(Err(ServiceError::Unprocessable(
            "limit: от 1 до 500".to_owned(),
        )));
    }
    response(
        blocking::run(&state.account, move |context| {
            Ok(json!(lists::list_posts(
                context,
                limit,
                query.status.as_deref(),
                query.chat.as_deref()
            )?))
        })
        .await,
    )
}

#[utoipa::path(get, path = "/api/posts/{pid}", operation_id = "post_api_posts__pid__get", summary = "Пост целиком: payload, статус, канал, version, реакции, нажатия", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn get(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    response(service(&state).await.get(&id).await)
}
#[utoipa::path(get, path = "/api/posts/{pid}/versions", operation_id = "post_versions_api_posts__pid__versions_get", summary = "Как выглядел пост до каждой замены, новые сверху", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn versions(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    response(service(&state).await.versions(&id).await)
}
#[utoipa::path(delete, path = "/api/posts/{pid}", operation_id = "delete_post_api_posts__pid__delete", summary = "Отменить отложенный или пропущенный пост, или удалить пост из канала", description = "`DELETE` неудачной отправки убирает её из списка.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn delete(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    response(service(&state).await.delete(&id).await)
}

#[utoipa::path(post, path = "/api/posts/{pid}/edit", operation_id = "edit_post_api_posts__pid__edit_post", summary = "Заменить содержимое поста, опубликованного через студию", description = "`?post_version=` — поле `version` поста из `GET /api/posts/{id}`: если пост с тех пор заменили, 409.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn edit(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
    headers: HeaderMap,
    Json(post): Json<Value>,
) -> Response {
    response(
        service(&state)
            .await
            .edit(&id, &post, &actor(&state, &headers), query.post_version)
            .await,
    )
}

#[utoipa::path(post, path = "/api/edit", operation_id = "edit_api_edit_post", summary = "Заменить опубликованный пост (editMessageText + rich_message)", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn edit_message(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(message_id) = body["message_id"].as_i64() else {
        return response(Err(ServiceError::Unprocessable(
            "message_id: обязательное число".to_owned(),
        )));
    };
    response(
        service(&state)
            .await
            .edit_message(
                &body["post"],
                &body["chat_id"],
                message_id,
                &actor(&state, &headers),
            )
            .await,
    )
}

#[utoipa::path(post, path = "/api/posts/{pid}/reschedule", operation_id = "reschedule_api_posts__pid__reschedule_post", summary = "Перенести отложенный пост или вернуть пропущенный в очередь на новое время", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn reschedule(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    response(service(&state).await.reschedule(&id, &body["at"]).await)
}

#[utoipa::path(post, path = "/api/posts/{pid}/pin", operation_id = "pin_post_api_posts__pid__pin_post", summary = "Закрепить пост в канале", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn pin(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let disable = body
        .as_ref()
        .and_then(|body| body["disable_notification"].as_bool())
        .unwrap_or(true);
    response(service(&state).await.pin(&id, true, disable).await)
}

#[utoipa::path(delete, path = "/api/posts/{pid}/pin", operation_id = "unpin_post_api_posts__pid__pin_delete", summary = "Открепить пост", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn unpin(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    response(service(&state).await.pin(&id, false, true).await)
}

#[utoipa::path(post, path = "/api/posts/{pid}/buttons", operation_id = "set_buttons_api_posts__pid__buttons_post", summary = "Заменить только кнопки под постом (editMessageReplyMarkup), текст не трогается", description = "Прежние кнопки попадают в историю поста (`/versions`), `version` поста растёт.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn buttons(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    response(
        service(&state)
            .await
            .buttons(
                &id,
                &body["reply_markup"],
                &body["callbacks"],
                &actor(&state, &headers),
            )
            .await,
    )
}

#[utoipa::path(post, path = "/api/posts/{pid}/widget-data", operation_id = "widget_data_api_posts__pid__widget_data_post", summary = "Изменить данные виджета (editMessageWidgetData): HTML неизменяем", description = "`chat_data`/`public_data` — строки до 1 024 байт UTF-8 без NUL; пустая строка стирает, отсутствующий ключ поле не трогает, null отклоняется. `?post_version=` из GET поста защищает от потери параллельной правки (409). Отложенному виджету данные записываются в payload и уйдут вместе с `sendWidget`.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn widget_data(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    response(
        service(&state)
            .await
            .widget_data(&id, &body, &actor(&state, &headers), query.post_version)
            .await,
    )
}

#[utoipa::path(post, path = "/api/posts/{pid}/stop-poll", operation_id = "stop_poll_api_posts__pid__stop_poll_post", summary = "Закрыть опрос и сохранить итоги", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn stop_poll(ScopedState(state): ScopedState, Path(id): Path<String>) -> Response {
    response(service(&state).await.stop_poll(&id).await)
}
