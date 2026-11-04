//! Utoipa inventory comes from the real handlers; preserved wire schemas remain explicit.
use axum::{
    Json,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde_json::Value;
use utoipa::OpenApi as _;
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    super::account_tokens::listing,
    super::account_tokens::create,
    super::account_tokens::revoke,
    super::telegram_auth::config,
    super::telegram_auth::start,
    super::channels::list,
    super::channels::check,
    super::channels::remove,
    super::channels::topics,
    super::channels::check_query,
    super::config::get,
    super::drafts::list,
    super::drafts::create,
    super::drafts::delete,
    super::drafts::get,
    super::drafts::update,
    super::editing::edit_message,
    super::operations::export,
    super::health::report,
    super::imports::forward_begin,
    super::imports::forward_get,
    super::imports::status,
    super::imports::import_post,
    super::community::list_links,
    super::community::edit_link,
    super::community::create_link,
    super::community::revoke_link,
    super::operations::read_logs,
    super::media::list,
    super::media::upload,
    super::posts::media,
    super::media::cleanup,
    super::media::fetch,
    super::media::delete,
    super::posts::poll,
    super::editing::listing,
    super::editing::delete,
    super::editing::get,
    super::editing::buttons,
    super::editing::edit,
    super::editing::unpin,
    super::editing::pin,
    super::editing::reschedule,
    super::editing::stop_poll,
    super::editing::versions,
    super::posts::preview,
    super::raw::call,
    super::preview::render,
    super::posts::schedule,
    super::posts::send,
    super::auth_session::logout,
    super::auth_session::whoami,
    super::auth_session::login,
    super::specs::summary,
    super::specs::raw,
    super::specs::refresh,
    super::specs::definition,
    super::community::list_suggested,
    super::community::approve,
    super::community::decline,
    super::community::reopen,
    super::templates::list,
    super::templates::create,
    super::templates::delete,
    super::templates::get,
    super::templates::update,
    super::templates::create_draft,
    super::webhook::status,
    super::specs::validate,
    super::telegram_auth::callback,
    super::preview::draft
))]
struct ApiDoc;
pub fn document() -> Result<Value, String> {
    let generated = ApiDoc::openapi();
    let mut wire: Value = serde_json::from_str(include_str!("compatibility.json"))
        .map_err(|error| error.to_string())?;
    let operations = serde_json::to_value(generated).map_err(|error| error.to_string())?;
    let paths = operations["paths"]
        .as_object()
        .ok_or("missing Utoipa paths")?;
    let old = wire["paths"]
        .as_object_mut()
        .ok_or("missing contract paths")?;
    if paths.len() != old.len() {
        return Err("Utoipa route inventory differs from compatibility contract".to_owned());
    }
    for (path, methods) in paths {
        let current = old
            .get_mut(path)
            .ok_or_else(|| format!("undocumented route {path}"))?;
        for (method, operation) in methods.as_object().ok_or("invalid path")? {
            let target = current
                .get_mut(method)
                .ok_or_else(|| format!("undocumented operation {method} {path}"))?;
            for field in ["operationId", "summary", "description", "tags"] {
                if let Some(value) = operation.get(field) {
                    target[field] = value.clone();
                }
            }
        }
        if current.as_object().map_or(0, serde_json::Map::len)
            != methods.as_object().map_or(0, serde_json::Map::len)
        {
            return Err(format!("missing operation {path}"));
        }
    }
    Ok(wire)
}
pub async fn schema() -> Response {
    match document() {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}
pub async fn docs() -> Html<&'static str> {
    Html(include_str!("api-docs.html"))
}
pub async fn redoc() -> Html<&'static str> {
    Html(include_str!("redoc.html"))
}
