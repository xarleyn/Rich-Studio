use axum::Json;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::security::{self, AuthError, ScopedState};
use crate::services::auth;

pub async fn run<T, F>(action: F) -> Result<T, AuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
{
    tokio::task::spawn_blocking(action)
        .await
        .map_err(|_| AuthError::new(500, "authentication worker failed"))?
}

pub fn session_cookie(secret: &str, ttl: i64, secure: bool) -> String {
    format!(
        "{}={secret}; Path=/; Max-Age={ttl}; HttpOnly; SameSite=Strict{}",
        security::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

pub fn set_cookie(response: &mut Response, value: &str) -> Result<(), AuthError> {
    let value = header::HeaderValue::from_str(value)
        .map_err(|_| AuthError::new(500, "invalid session cookie"))?;
    response.headers_mut().append(header::SET_COOKIE, value);
    Ok(())
}

#[utoipa::path(post, path = "/api/session", operation_id = "login_api_session_post", summary = "Войти в редактор: ключ или токен меняется на cookie сессии", description = "Cookie `rs_session`: HttpOnly, SameSite=Strict, Secure по HTTPS, живёт `SESSION_DAYS` дней. Её же берут\nстраницы предпросмотра и картинки из загрузок.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn login(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AuthError> {
    let key = body["key"]
        .as_str()
        .ok_or_else(|| AuthError::new(422, "key: обязательная строка"))?
        .to_owned();
    let ttl = (state.settings.session_days * 86400.0) as i64;
    let (secret, result) = run(move || auth::login(&state.settings, &key)).await?;
    let mut response = Json(result).into_response();
    set_cookie(
        &mut response,
        &session_cookie(&secret, ttl, security::https(&headers)),
    )?;
    Ok(response)
}

#[utoipa::path(get, path = "/api/session", operation_id = "whoami_api_session_get", summary = "Кто я: имя токена и его уровень", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn whoami(ScopedState(state): ScopedState) -> Result<Json<Value>, AuthError> {
    Ok(Json(
        run(move || auth::whoami(&state.settings, &state.principal)).await?,
    ))
}

#[utoipa::path(delete, path = "/api/session", operation_id = "logout_api_session_delete", summary = "Выйти из редактора: сессия и её cookie удаляются", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn logout(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let secret = security::cookie(&headers, security::SESSION_COOKIE);
    let result = run(move || auth::logout(&state.settings, secret.as_deref())).await?;
    let mut response = Json(result).into_response();
    set_cookie(
        &mut response,
        &session_cookie("", 0, security::https(&headers)),
    )?;
    Ok(response)
}
