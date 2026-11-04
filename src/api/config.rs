use crate::{
    security::ScopedState,
    services::{kinds, updates},
};
use axum::Json;
use serde_json::{Value, json};

#[utoipa::path(get, path = "/api/config", operation_id = "config_api_config_get", summary = "Что нужно редактору: канал по умолчанию, чат для «Мне в личку», задан ли токен, версия спецификации", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn get(ScopedState(state): ScopedState) -> Json<Value> {
    let spec = state.spec.read().await;
    Json(json!({
        "default_chat": if state.account.root {json!(state.settings.default_chat)} else {json!("")},
        "preview_chat": if state.account.root {json!(state.settings.preview_chat)} else {json!(state.account.account_id.map(|uid| uid.to_string()).unwrap_or_default())},
        "token_set":!state.settings.bot_token.is_empty(),
        "api_base":if state.settings.public {Value::Null}else{json!(state.settings.api_base)},
        "spec_version":spec.version(),
        "auth":state.settings.public||!state.settings.api_key.is_empty(),
        "public":state.settings.public,
        "bot_username":if state.settings.public{json!(state.settings.bot_username)}else{Value::Null},
        "updates":updates::public_status(&state).await,
        "kinds":kinds::describe()
    }))
}
