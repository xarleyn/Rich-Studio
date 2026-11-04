use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde_json::Value;

use crate::api::auth_session::run;
use crate::security::{AuthError, ScopedState};
use crate::services::account_tokens;

#[utoipa::path(get, path = "/api/account/tokens", operation_id = "listing_api_account_tokens_get", summary = "Мои токены API/MCP и адрес подключения", description = "Только Telegram-сессия владельца учётки. Секреты и хэши не возвращаются; даты — Unix seconds.\nПри создании токена история сокращается до 100 последних неактивных; всего не более 120 записей.\nТокены других учёток и сервера не видны.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn listing(ScopedState(state): ScopedState) -> Result<Json<Value>, AuthError> {
    Ok(Json(
        run(move || account_tokens::listing(&state.settings, &state.principal)).await?,
    ))
}

#[utoipa::path(post, path = "/api/account/tokens", operation_id = "create_api_account_tokens_post", summary = "Создать персональный токен API/MCP", description = "Секрет возвращается только здесь, один раз. До 20 действующих токенов и 60 попыток создания в час.\nУровень admin недоступен; выданный токен не может управлять токенами или входить в редактор.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), AuthError> {
    Ok((
        StatusCode::CREATED,
        Json(run(move || account_tokens::create(&state.settings, &state.principal, &body)).await?),
    ))
}

#[utoipa::path(delete, path = "/api/account/tokens/{token_id}", operation_id = "revoke_api_account_tokens__token_id__delete", summary = "Отозвать свой токен API/MCP", description = "После отзыва новые запросы с этим токеном отклоняются. Уже принятые операции и расписание сохраняются.\nПовторный отзыв безопасен; чужой или неизвестный id даёт 404. Telegram-сессия остаётся открытой.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn revoke(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
) -> Result<Json<Value>, AuthError> {
    Ok(Json(
        run(move || account_tokens::revoke(&state.settings, &state.principal, &id)).await?,
    ))
}
