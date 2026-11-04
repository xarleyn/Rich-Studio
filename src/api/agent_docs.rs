use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;

use crate::{AppState, settings::Settings};

const LLMS: &str = include_str!("../../app/bundled/agent-docs/llms.txt");
const SETUP: &str = include_str!("../../app/bundled/agent-docs/setup.md");
const SKILL: &str = include_str!("../../app/bundled/agent-docs/SKILL.md");

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/llms.txt", get(llms))
        .route("/llms-full.txt", get(full))
        .route("/agents.txt", get(agents))
        .route("/agents.json", get(catalog))
        .route("/agents.md", get(setup))
        .route("/skills/richpost/SKILL.md", get(skill))
}

fn public(settings: &Settings) -> &str {
    if settings.landing_url.is_empty() {
        &settings.public_url
    } else {
        &settings.landing_url
    }
}
fn document(settings: &Settings, template: &str) -> String {
    let instructions = if settings.public {
        format!(
            "Open {}/ and sign in through Telegram. Open your account menu → ‘Токены API и MCP’ (English: ‘API and MCP tokens’). Create a separate token for this client, choose ‘Черновики’ / ‘Drafts’ (draft) and an appropriate expiry. The secret is shown once. Use read for read-only work; choose publish only when you want the agent to publish. Keep the token in your client's secret setting or the RICHPOST_TOKEN environment variable.",
            settings.public_url
        )
    } else {
        "Ask the operator of this private installation for a separate named token with the draft role (or read for read-only work). Only the operator manages these tokens with the studio CLI. Keep the token in your client's secret setting or the RICHPOST_TOKEN environment variable.".to_owned()
    };
    template
        .replace("\r\n", "\n")
        .replace("__SITE_URL__", public(settings))
        .replace("__APP_URL__", &settings.public_url)
        .replace("__TOKEN_STEPS__", &instructions)
}

fn headers(markdown: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    headers.insert(
        header::LINK,
        HeaderValue::from_static("</llms.txt>; rel=\"describedby\""),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if markdown {
            "text/markdown; charset=utf-8"
        } else {
            "text/plain; charset=utf-8"
        }),
    );
    headers
}
async fn llms(State(state): State<AppState>) -> Response {
    (headers(true), document(&state.settings, LLMS)).into_response()
}
async fn setup(State(state): State<AppState>) -> Response {
    (headers(true), document(&state.settings, SETUP)).into_response()
}
async fn skill(State(state): State<AppState>) -> Response {
    (headers(true), document(&state.settings, SKILL)).into_response()
}
async fn full(State(state): State<AppState>) -> Response {
    (
        headers(true),
        format!(
            "{}\n---\n\n{}",
            document(&state.settings, SETUP),
            document(&state.settings, SKILL)
        ),
    )
        .into_response()
}
async fn agents(State(state): State<AppState>) -> Response {
    let public = public(&state.settings);
    (headers(false), format!("# RichPost agent discovery\n# Format: https://agents-txt.com (draft)\n# JSON: {public}/agents.json\n# Setup and human-issued token: {public}/agents.md\nMCP: {}/mcp\n\nSkills: {public}/skills/richpost/SKILL.md\n", state.settings.public_url)).into_response()
}
async fn catalog(State(state): State<AppState>) -> Response {
    let public = public(&state.settings);
    let mut response = Json(json!({
        "$schema":"https://agents-txt.com/schema/agents-json/v1.0.json", "version":"1.0", "standard":"https://agents-txt.com",
        "site":{"name":"RichPost","url":public,"description":format!("Setup instructions: {public}/agents.md")},
        "mcp":[{"url":format!("{}/mcp",state.settings.public_url),"type":"streamable-http","description":"Telegram post drafts, previews and templates. Requires a human-issued bearer token; read the setup instructions before connecting. Publishing depends on token permissions."}],
        "skills":[{"url":format!("{public}/skills/richpost/SKILL.md"),"description":"Connect and use RichPost safely."}]
    })).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response.headers_mut().insert(
        header::LINK,
        HeaderValue::from_static("</agents.md>; rel=\"describedby\""),
    );
    response
}
