use crate::security::ScopedState;
use axum::Json;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::telegram::uploads::{UploadError, Uploads, collect_uploads};

const SAFE_PREFIXES: &[&str] = &["get", "send", "edit", "answer", "copy", "forward"];
const SAFE_METHODS: &[&str] = &[
    "stopPoll",
    "pinChatMessage",
    "unpinChatMessage",
    "setMessageReaction",
];

pub fn needs_confirmation(method: &str) -> bool {
    method == "getUpdates"
        || !(SAFE_METHODS.contains(&method)
            || SAFE_PREFIXES
                .iter()
                .any(|prefix| method.starts_with(prefix)))
}

#[utoipa::path(post, path = "/api/raw/{method}", operation_id = "raw_api_raw__method__post", summary = "Любой метод Bot API как есть (для функций, которых ещё нет в студии)", description = "Только для уровня admin; `RAW_API=off` выключает маршрут совсем.", tag = "telegram", responses((status = 200, description = "Successful Response")))]
pub async fn call(
    ScopedState(state): ScopedState,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if !state.settings.raw_api {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"/api/raw выключен (RAW_API=off)"})),
        )
            .into_response();
    }
    if method.is_empty()
        || !method
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"bad method"})),
        )
            .into_response();
    }
    if needs_confirmation(&method)
        && headers
            .get("x-confirm")
            .and_then(|value| value.to_str().ok())
            != Some("yes")
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail":format!("{method} не только читает и отправляет сообщения: повторите с заголовком X-Confirm: yes")}))).into_response();
    }
    let payload = body.map_or_else(|| json!({}), |body| body.0);
    let media_dir = state.account.media_dir.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let mut uploads = Uploads::default();
        let payload = collect_uploads(&payload, &media_dir, &mut uploads)?;
        Ok::<_, UploadError>((payload, uploads))
    })
    .await;
    let (payload, uploads) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let transport = state.transport();
    match transport
        .call(&state.account, &method, payload, uploads)
        .await
    {
        Ok(result) => Json(json!({"result":result})).into_response(),
        Err(error) => (
            super::channels::telegram_error(&error),
            Json(json!({"detail":error.to_string()})),
        )
            .into_response(),
    }
}
