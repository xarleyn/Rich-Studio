//! Official MCP Streamable HTTP adapter. Every request receives its authenticated account scope.
pub mod preview_app;
pub mod tools;

use crate::AppState;
use axum::{Router, http::request::Parts};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

#[derive(Deserialize)]
struct Contract {
    instructions: String,
    tools: Vec<Tool>,
}
fn contract() -> Result<&'static Contract, ErrorData> {
    static CONTRACT: OnceLock<Result<Contract, String>> = OnceLock::new();
    CONTRACT
        .get_or_init(|| {
            serde_json::from_str(include_str!("contract.json")).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| ErrorData::internal_error(error.clone(), None))
}
pub fn definitions() -> Result<Vec<Tool>, ErrorData> {
    Ok(contract()?.tools.clone())
}
fn scoped(context: &RequestContext<RoleServer>) -> Result<AppState, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<AppState>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("authenticated request scope is missing", None))
}
#[derive(Clone)]
struct Studio;
impl ServerHandler for Studio {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("rich-studio", env!("CARGO_PKG_VERSION")).with_title("Rich Studio"),
        );
        if let Ok(contract) = contract() {
            info = info.with_instructions(contract.instructions.clone());
        }
        info
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        contract()
            .ok()?
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let state = scoped(&context)?;
        Ok(ListToolsResult::with_all_items(
            definitions()?
                .into_iter()
                .filter(|tool| crate::security::tool_visible(&state, &tool.name))
                .collect(),
        ))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let state = scoped(&context)?;
        let name = request.name.as_ref();
        let tool = self
            .get_tool(name)
            .ok_or_else(|| ErrorData::invalid_params(format!("unknown tool {name}"), None))?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let outcome = async {
            crate::security::check_tool(&state, name).map_err(|error| error.to_string())?;
            let schema = Value::Object((*tool.input_schema).clone());
            let arguments = crate::services::contracts::normalize(&schema, &schema, &arguments)?;
            dispatch(state, name, arguments).await
        }
        .await;
        let result = match outcome {
            Ok(value) => {
                let content = match &value {
                    Value::Array(items) => items
                        .iter()
                        .map(|item| ContentBlock::text(item.to_string()))
                        .collect(),
                    _ => vec![ContentBlock::text(value.to_string())],
                };
                let mut result = CallToolResult::success(content);
                if value.is_object() {
                    result.structured_content = Some(value);
                }
                result
            }
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        };
        Ok(result.into())
    }
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        scoped(&context)?;
        let resource = Resource::new(preview_app::URI, "Предпросмотр поста")
            .with_description("Пост так, как его покажет Telegram")
            .with_mime_type("text/html;profile=mcp-app");
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        scoped(&context)?;
        if request.uri != preview_app::URI {
            return Err(ErrorData::invalid_params("resource not found", None));
        }
        let content = ResourceContents::text(preview_app::PreviewPage::html(), preview_app::URI)
            .with_mime_type("text/html;profile=mcp-app")
            .with_meta(MetaObject(serde_json::Map::from_iter([(
                "ui".to_owned(),
                json!({"prefersBorder":false,"csp":{"connectDomains":[],"resourceDomains":[]}}),
            )])));
        Ok(ReadResourceResult::new(vec![content]).into())
    }
}
pub fn router(state: &AppState) -> Router<AppState> {
    let mut config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(state.settings.allowed_hosts.iter().cloned());
    config.legacy_session_mode = false;
    config.json_response = true;
    config.max_request_body_bytes = ((state.settings.max_upload_mb * 1024.0 * 1024.0 * 1.4)
        as usize)
        .saturating_add(2 * 1024 * 1024);
    let service = StreamableHttpService::new(
        || Ok(Studio),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new().nest_service("/mcp", service)
}

pub async fn dispatch(state: AppState, name: &str, mut arguments: Value) -> Result<Value, String> {
    if [
        "preview_post",
        "validate_post",
        "save_draft",
        "send_preview_to_me",
        "publish_post",
        "schedule_post",
        "edit_post",
    ]
    .contains(&name)
        && let Some(post) = arguments.get("post").filter(|post| !post.is_null())
    {
        arguments["post"] = crate::services::contracts::parse_post(post)?;
    }
    match name {
        "list_invite_links"
        | "create_invite_link"
        | "edit_invite_link"
        | "revoke_invite_link"
        | "list_suggested_posts"
        | "approve_suggested_post"
        | "decline_suggested_post" => tools::community::call(state, name, arguments).await,
        "telegram_import_status" | "import_telegram_post" => {
            tools::imports::call(state, name, arguments).await
        }
        "studio_status" | "studio_health" | "studio_logs" => {
            tools::studio::call(state, name, arguments).await
        }
        "list_posts"
        | "get_post"
        | "post_history"
        | "edit_post"
        | "reschedule_post"
        | "cancel_scheduled_post" => tools::published::call(state, name, arguments).await,
        "list_post_templates"
        | "get_post_template"
        | "save_post_template"
        | "draft_from_template" => tools::templates::call(state, name, arguments).await,
        "validate_post" => {
            let spec = state.spec.read().await;
            Ok(tools::bot_api::validate(&spec, &arguments["post"]))
        }
        "bot_api_status" => {
            let spec = state.spec.read().await;
            Ok(tools::bot_api::status(&spec))
        }
        "describe_type" => {
            let spec = state.spec.read().await;
            tools::bot_api::describe(&spec, arguments["name"].as_str().unwrap_or_default())
        }
        "refresh_bot_api" => Ok(tools::bot_api::refresh(&state).await),
        "save_draft" | "list_drafts" | "get_draft" | "send_preview_to_me" => {
            tools::drafts::call(state, name, arguments).await
        }
        "upload_media" | "list_media" => tools::files::call(state, name, arguments).await,
        "check_channel" | "list_channels" => tools::chats::call(state, name, arguments).await,
        "publish_post" | "schedule_post" | "send_poll" => {
            tools::publishing::call(state, name, arguments).await
        }
        "send_media_post" => tools::media_posts::call(state, arguments).await,
        "preview_post" => preview(state, arguments).await,
        _ => Err(format!("unknown tool {name}")),
    }
}

async fn preview(state: AppState, arguments: Value) -> Result<Value, String> {
    let draft_id = arguments
        .get("draft_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if arguments["post"].is_null() == draft_id.is_none() {
        return Err("передайте post или draft_id, одно из двух".to_owned());
    }
    let (post, version) = if let Some(id) = draft_id.clone() {
        let draft =
            crate::services::drafts::run(state.account.clone(), move |service| service.get(&id))
                .await
                .map_err(|error| error.to_string())?;
        (
            crate::services::contracts::parse_post(&draft["payload"])?,
            Some(draft["version"].clone()),
        )
    } else {
        (arguments.get("post").cloned().unwrap_or(Value::Null), None)
    };
    if post.is_null() {
        return Err("передайте post или draft_id".to_owned());
    }
    let validation = {
        let spec = state.spec.read().await;
        tools::bot_api::validate(&spec, &post)
    };
    let mut result = json!({"post":post,"validation":validation});
    if let (Some(version), Some(id)) = (version, draft_id) {
        result["version"] = version;
        if let Some(links) =
            crate::services::drafts::draft_links(&state.settings.public_url, &id).as_object()
            && let Some(object) = result.as_object_mut()
        {
            object.extend(links.clone());
        }
    }
    Ok(result)
}
