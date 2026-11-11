use std::collections::BTreeMap;

use axum::Json;
use axum::extract::RawQuery;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::{Value, json};

use crate::api::auth_session::{run, session_cookie, set_cookie};
use crate::security::{self, AuthError, ScopedState};
use crate::services::accounts;

pub const STATE_COOKIE: &str = "rs_telegram_login";

#[utoipa::path(get, path = "/api/auth/config", operation_id = "config_api_auth_config_get", summary = "Публичные настройки входа через Telegram", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn config(ScopedState(state): ScopedState) -> Json<Value> {
    Json(
        json!({"telegram":state.settings.public,"bot_username":if state.settings.public { Some(&state.settings.bot_username) } else { None },"owner_login":state.settings.owner_telegram_id > 0}),
    )
}

#[utoipa::path(post, path = "/api/auth/telegram/start", operation_id = "start_api_auth_telegram_start_post", summary = "Начать вход через Telegram с одноразовой проверкой браузера", description = "Challenge действует пять минут. Cookie SameSite=Lax нужна для возврата с telegram.org.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn start(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let (secret, result) = run(move || accounts::start_login(&state.settings)).await?;
    let mut response = Json(result).into_response();
    let cookie = format!(
        "{STATE_COOKIE}={secret}; Path=/auth/telegram/callback; Max-Age={}; HttpOnly; SameSite=Lax{}",
        accounts::LOGIN_TTL,
        if security::https(&headers) {
            "; Secure"
        } else {
            ""
        }
    );
    set_cookie(&mut response, &cookie)?;
    Ok(response)
}

async fn complete(
    state: crate::AppState,
    headers: HeaderMap,
    query: String,
) -> Result<Response, AuthError> {
    let mut data = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if data.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(AuthError::new(401, "Повторяющиеся параметры входа"));
        }
    }
    let cookie = security::cookie(&headers, STATE_COOKIE).unwrap_or_default();
    let ttl = (state.settings.session_days * 86400.0) as i64;
    let secret = run(move || accounts::finish_login(&state.settings, &data, &cookie)).await?;
    let mut response = Redirect::to("/").into_response();
    set_cookie(
        &mut response,
        &session_cookie(&secret, ttl, security::https(&headers)),
    )?;
    set_cookie(
        &mut response,
        &format!("{STATE_COOKIE}=; Path=/auth/telegram/callback; Max-Age=0; SameSite=Lax"),
    )?;
    Ok(response)
}

#[utoipa::path(get, path = "/auth/telegram/callback", operation_id = "callback_auth_telegram_callback_get", summary = "Проверить подпись Telegram и открыть сессию пользователя", description = "Telegram id берётся только из подписанных данных; повторы и вход из другого браузера отклоняются.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn callback(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    match complete(state,headers,query.unwrap_or_default()).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE,"text/html; charset=utf-8"),(header::CONTENT_SECURITY_POLICY,security::CSP),(header::CACHE_CONTROL,"no-store")],
            "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Вход в RichPost</title><link rel=\"stylesheet\" href=\"/static/app.css\"><body><main class=\"login-screen\"><div class=\"login-card\"><h1>Не удалось войти</h1><p>Данные входа не прошли проверку или устарели. Начните вход в этом браузере заново.</p><a href=\"/\">Вернуться ко входу</a></div></main></body></html>",
        ).into_response(),
    }
}
