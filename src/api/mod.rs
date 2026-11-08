pub mod account_tokens;
pub mod agent_docs;
pub mod auth_session;
pub mod channels;
pub mod community;
pub mod config;
pub mod drafts;
pub mod editing;
pub mod emoji;
pub mod health;
pub mod imports;
pub mod landing;
pub mod media;
pub mod openapi;
pub mod operations;
pub mod posts;
pub mod preview;
pub mod pwa;
pub mod raw;
pub mod specs;
pub mod static_assets;
pub mod telegram_auth;
pub mod templates;
pub mod validation;
pub mod webhook;
pub mod widgets;

use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::json;
use tower_http::services::ServeDir;

use crate::{AppState, mcp_tools};

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(landing::router())
        .merge(agent_docs::router())
        .merge(operations::router())
        .merge(mcp_tools::router(&state))
        .route("/openapi.json", get(openapi::schema))
        .route("/docs", get(openapi::docs))
        .route("/redoc", get(openapi::redoc))
        .route("/api/config", get(config::get))
        .route("/api/updates", get(webhook::status))
        .route("/api/telegram/webhook", post(webhook::receive))
        .route(
            "/api/imports/telegram",
            get(imports::status).post(imports::import_post),
        )
        .route("/api/imports/forward", post(imports::forward_begin))
        .route("/api/imports/forward/{token}", get(imports::forward_get))
        .route(
            "/api/invite-links",
            get(community::list_links)
                .post(community::create_link)
                .patch(community::edit_link),
        )
        .route("/api/invite-links/revoke", post(community::revoke_link))
        .route("/api/suggested-posts", get(community::list_suggested))
        .route(
            "/api/suggested-posts/{sid}/approve",
            post(community::approve),
        )
        .route(
            "/api/suggested-posts/{sid}/decline",
            post(community::decline),
        )
        .route("/api/suggested-posts/{sid}/reopen", post(community::reopen))
        .route("/api/edit", post(editing::edit_message))
        .route("/api/posts", get(editing::listing))
        .route("/api/posts/{id}", get(editing::get).delete(editing::delete))
        .route("/api/posts/{id}/edit", post(editing::edit))
        .route("/api/posts/{id}/versions", get(editing::versions))
        .route("/api/posts/{id}/reschedule", post(editing::reschedule))
        .route(
            "/api/posts/{id}/pin",
            post(editing::pin).delete(editing::unpin),
        )
        .route("/api/posts/{id}/buttons", post(editing::buttons))
        .route("/api/posts/{id}/widget-data", post(editing::widget_data))
        .route("/api/posts/{id}/stop-poll", post(editing::stop_poll))
        .route("/readyz", get(health::ready))
        .route("/api/health", get(health::report))
        .route(
            "/api/session",
            get(auth_session::whoami)
                .post(auth_session::login)
                .delete(auth_session::logout),
        )
        .route("/api/auth/config", get(telegram_auth::config))
        .route("/api/auth/telegram/start", post(telegram_auth::start))
        .route("/auth/telegram/callback", get(telegram_auth::callback))
        .route(
            "/api/account/tokens",
            get(account_tokens::listing).post(account_tokens::create),
        )
        .route(
            "/api/account/tokens/{token_id}",
            axum::routing::delete(account_tokens::revoke),
        )
        .route(
            "/api/templates",
            get(templates::list).post(templates::create),
        )
        .route(
            "/api/templates/{tid}",
            get(templates::get)
                .put(templates::update)
                .delete(templates::delete),
        )
        .route("/api/templates/{tid}/drafts", post(templates::create_draft))
        .route(
            "/api/emoji/packs",
            get(emoji::packs_list).post(emoji::packs_add),
        )
        .route(
            "/api/emoji/packs/{name}",
            axum::routing::delete(emoji::packs_remove),
        )
        .route("/api/emoji/img/{custom_emoji_id}", get(emoji::img))
        .route("/api/emoji/file/{custom_emoji_id}", get(emoji::file))
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/drafts", get(drafts::list).post(drafts::create))
        .route(
            "/api/drafts/{id}",
            get(drafts::get).put(drafts::update).delete(drafts::delete),
        )
        .route("/api/spec", get(specs::summary))
        .route("/api/spec/raw", get(specs::raw))
        .route("/api/spec/type/{name}", get(specs::definition))
        .route("/api/spec/refresh", post(specs::refresh))
        .route("/api/validate", post(specs::validate))
        .route("/api/channels", get(channels::list).post(channels::check))
        .route("/api/check", get(channels::check_query))
        .route(
            "/api/channels/{chat_id}/topics",
            axum::routing::put(channels::topics),
        )
        .route(
            "/api/channels/{chat_id}",
            axum::routing::delete(channels::remove),
        )
        .route("/api/send", post(posts::send))
        .route("/api/preview-send", post(posts::preview))
        .route("/api/schedule", post(posts::schedule))
        .route("/api/poll", post(posts::poll))
        .route("/api/media-post", post(posts::media))
        .route("/api/widget", post(posts::widget))
        .route("/api/widget/validate", post(posts::widget_validate))
        .route("/api/raw/{method}", post(raw::call))
        .route("/api/media", get(media::list).post(media::upload))
        .route("/api/media/fetch", post(media::fetch))
        .route("/api/media/cleanup", post(media::cleanup))
        .route("/api/media/{name}", axum::routing::delete(media::delete))
        .route("/media/{name}", get(media::file))
        .route("/api/render", post(preview::render))
        .route("/render/draft/{id}", get(preview::draft))
        .route("/render/widget", post(widgets::preview))
        .route("/sw.js", get(pwa::service_worker))
        .route("/", get(landing::index))
        .nest_service(
            "/static",
            ServeDir::new(&state.static_dir).precompressed_br(),
        )
        .fallback(landing::not_found)
        .layer(axum::extract::DefaultBodyLimit::max(
            ((state.settings.max_upload_mb * 1024.0 * 1024.0 * 1.4) as usize)
                .saturating_add(2 * 1024 * 1024),
        ))
        .layer(axum::middleware::from_fn(validation::validate))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            static_assets::cache,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::security::guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            operations::observe,
        ))
        .with_state(state)
}
