use super::posts::error_response;
use crate::services::community::{links, suggestions};
use crate::{AppState, security::ScopedState, services::idempotency::Idempotency};
use axum::{
    Json,
    extract::{Path, Query},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
fn response(result: Result<Value, crate::errors::ServiceError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(error),
    }
}
pub fn actor(state: &AppState, headers: &HeaderMap) -> String {
    let client = headers
        .get("x-studio-client")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.len() <= 40
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
        })
        .unwrap_or("api");
    state.principal.actor(client)
}
#[derive(Deserialize)]
pub struct LinkQuery {
    chat_id: Option<String>,
}
#[utoipa::path(get, path = "/api/invite-links", operation_id = "list_links_api_invite_links_get", summary = "Пригласительные ссылки, созданные студией, со счётчиком вступлений", description = "`joined` и `requests` студия считает сама по событиям `chat_member` и `chat_join_request`: Telegram боту\nтаких счётчиков не даёт. Вступления, пока студия была выключена больше суток, не посчитаются.", tag = "invite links", responses((status = 200, description = "Successful Response")))]
pub async fn list_links(ScopedState(state): ScopedState, Query(q): Query<LinkQuery>) -> Response {
    response(links(&state).await.listing(&json!(q.chat_id)).await)
}
#[utoipa::path(post, path = "/api/invite-links", operation_id = "create_link_api_invite_links_post", summary = "Создать именную пригласительную ссылку (createChatInviteLink) или платную подписку", description = "Бот должен быть админом канала с правом «Пригласительные ссылки» (`can_invite_users`).", tag = "invite links", responses((status = 200, description = "Successful Response")))]
pub async fn create_link(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    response(
        links(&state)
            .await
            .create(&body, &actor(&state, &headers))
            .await,
    )
}
#[utoipa::path(patch, path = "/api/invite-links", operation_id = "edit_link_api_invite_links_patch", summary = "Изменить название, срок или предел ссылки", description = "Непереданные поля остаются как были, снять их: `name: \"\"`, `expire_date: 0`, `member_limit: 0`,\n`creates_join_request: false`. У платной подписки меняется только название.", tag = "invite links", responses((status = 200, description = "Successful Response")))]
pub async fn edit_link(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    response(links(&state).await.edit(&body).await)
}
#[utoipa::path(post, path = "/api/invite-links/revoke", operation_id = "revoke_link_api_invite_links_revoke_post", summary = "Отозвать ссылку: по ней больше не вступить", tag = "invite links", responses((status = 200, description = "Successful Response")))]
pub async fn revoke_link(ScopedState(state): ScopedState, Json(body): Json<Value>) -> Response {
    response(
        links(&state)
            .await
            .revoke(body["invite_link"].as_str().unwrap_or(""))
            .await,
    )
}
#[derive(Deserialize)]
pub struct SuggestionQuery {
    state: Option<String>,
}
#[utoipa::path(get, path = "/api/suggested-posts", operation_id = "list_suggested_api_suggested_posts_get", summary = "Предложенные посты из личных сообщений канала", description = "`state`: pending (ждёт решения), approved, declined, paid, refunded, failed. Студия видит предложения,\nпришедшие, пока она запущена: бот — админ канала с правом «Личные сообщения канала».", tag = "suggested posts", responses((status = 200, description = "Successful Response")))]
pub async fn list_suggested(
    ScopedState(state): ScopedState,
    Query(q): Query<SuggestionQuery>,
) -> Response {
    response(suggestions(&state).listing(q.state).await)
}
#[utoipa::path(post, path = "/api/suggested-posts/{sid}/approve", operation_id = "approve_api_suggested_posts__sid__approve_post", summary = "Принять предложенный пост: Telegram опубликует его в канале (approveSuggestedPost)", description = "Платное предложение списывает звёзды с автора. Боту нужно право публиковать в канале.", tag = "suggested posts", responses((status = 200, description = "Successful Response")))]
pub async fn approve(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map_or_else(|| json!({}), |Json(v)| v);
    let actor = actor(&state, &headers);
    let key = headers.get("idempotency-key").and_then(|v| v.to_str().ok());
    let normalized = json!({"sid":id,"send_date":body["send_date"]});
    response(
        Idempotency::new(state.account.clone())
            .run(key, "suggested-approve", &normalized, &actor, || async {
                suggestions(&state)
                    .approve(&id, &body["send_date"], &actor)
                    .await
            })
            .await,
    )
}
#[utoipa::path(post, path = "/api/suggested-posts/{sid}/decline", operation_id = "decline_api_suggested_posts__sid__decline_post", summary = "Отклонить предложенный пост с комментарием автору (declineSuggestedPost)", tag = "suggested posts", responses((status = 200, description = "Successful Response")))]
pub async fn decline(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map_or_else(|| json!({}), |Json(v)| v);
    response(
        suggestions(&state)
            .decline(&id, &body["comment"], &actor(&state, &headers))
            .await,
    )
}
#[utoipa::path(post, path = "/api/suggested-posts/{sid}/reopen", operation_id = "reopen_api_suggested_posts__sid__reopen_post", summary = "Решить снова: решение без ответа Telegram (approving/declining) возвращается в ожидание", description = "Только человек (уровень admin; в MCP этого нет) и только после проверки в личных сообщениях канала, что\nрешение не дошло.\nДошедшее решение Telegram, скорее всего, второй раз не примет, а его итог всё равно придёт и обновит состояние.", tag = "suggested posts", responses((status = 200, description = "Successful Response")))]
pub async fn reopen(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    response(
        suggestions(&state)
            .reopen(&id, &actor(&state, &headers))
            .await,
    )
}
