use crate::security::ScopedState;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;
use crate::api::drafts;

#[derive(Deserialize)]
pub struct ThemeQuery {
    #[serde(default = "auto")]
    theme: String,
}

fn auto() -> String {
    "auto".to_owned()
}

pub fn render_page(template: &str, post: &Value, theme: &str, title: &str) -> String {
    let theme = if ["auto", "light", "dark"].contains(&theme) {
        theme
    } else {
        "auto"
    };
    let payload = serde_json::to_string(post)
        .unwrap_or_else(|_| "null".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let mut output = template.replace("__PAYLOAD__", &payload);
    output = output.replace("__THEME__", theme);
    output.replace(
        "__TITLE__",
        &title
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;"),
    )
}

fn page(state: &AppState, post: &Value, theme: &str, title: &str) -> Response {
    match std::fs::read_to_string(state.static_dir.join("render.html")) {
        Ok(template) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_page(&template, post, theme, title),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[utoipa::path(post, path = "/api/render", operation_id = "render_api_render_post", summary = "Отрисовать пост как в Telegram: отдельная HTML-страница", tag = "preview", responses((status = 200, description = "Successful Response")))]
pub async fn render(
    ScopedState(state): ScopedState,
    Query(query): Query<ThemeQuery>,
    Json(post): Json<Value>,
) -> Response {
    page(&state, &post, &query.theme, "Предпросмотр")
}

#[utoipa::path(get, path = "/render/draft/{did}", operation_id = "render_draft_render_draft__did__get", summary = "Страница предпросмотра черновика (preview_path из save_draft)", tag = "preview", responses((status = 200, description = "Successful Response")))]
pub async fn draft(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Query(query): Query<ThemeQuery>,
) -> Response {
    let result = drafts::run(state.clone(), move |service| service.get(&id)).await;
    match result {
        Ok(draft) => page(
            &state,
            &draft["payload"],
            &query.theme,
            draft["title"]
                .as_str()
                .filter(|title| !title.is_empty())
                .unwrap_or("Черновик"),
        ),
        Err(error) => drafts::error_response(error),
    }
}
