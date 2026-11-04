use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::channels::ChannelService;
use rich_studio::settings::Settings;
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn settings(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@main".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

async fn chat_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap_or(0);
                if count == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..count]);
                if data.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data).to_lowercase();
                    let length = headers
                        .split("content-length:")
                        .nth(1)
                        .and_then(|tail| tail.lines().next())
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let end = data
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap_or(0)
                        + 4;
                    if data.len() >= end + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            let method = request
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default();
            let result = match method {
                "getMe" => json!({"id":42,"username":"studio_bot"}),
                "getChat" => {
                    json!({"id":-1002627012139i64,"type":"supergroup","title":"МОИ ЗАМЕТКИ","is_forum":true})
                }
                "getChatMember" => {
                    json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
                }
                _ => json!(true),
            };
            let body = json!({"ok":true,"result":result}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn channels_remember_rights_topics_and_default() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = settings(&dir, &chat_server().await);
    let context = AccountContext::root(&settings);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    let service = ChannelService::new(context.clone(), transport, settings.default_chat.clone());
    let listed = service.list()?;
    assert_eq!(listed[0]["chat_id"], "@main");
    assert_eq!(listed[0]["is_default"], true);
    let checked = service.check(json!("-1002627012139")).await?;
    assert_eq!(checked["channel"]["title"], "МОИ ЗАМЕТКИ");
    assert_eq!(checked["channel"]["is_forum"], 1);
    assert_eq!(checked["channel"]["rights"]["status"], "administrator");
    let topics = service.set_topics("-1002627012139", vec![json!({"id":7,"name":"Анонсы"})])?;
    assert_eq!(topics["topics"], json!([{"id":7,"name":"Анонсы"}]));
    assert!(
        service
            .set_topics("-1002627012139", vec![json!({"id":0})])
            .is_err()
    );
    assert!(service.set_topics("@missing", vec![]).is_err());
    service.remove("-1002627012139")?;
    assert_eq!(service.list()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn channel_alias_survives_personal_rechecks_while_accounts_store_numeric_ids()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = settings(&dir, &chat_server().await);
    let context = AccountContext::root(&settings);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    let service = ChannelService::new(context, transport.clone(), "@kept_chan".into());
    let checked = service.check(json!(" @kept_chan ")).await?;
    assert_eq!(checked["channel"]["chat_id"], "@kept_chan");
    assert_eq!(checked["channel"]["chat_num"], -1002627012139i64);
    assert_eq!(checked["can_post"], true);
    assert_eq!(checked["channel"]["can_post"], 1);
    service.set_topics("@kept_chan", vec![json!({"id":7,"name":"Saved topic"})])?;
    let checked = service.check(json!(-1002627012139i64)).await?;
    assert_eq!(checked["channel"]["chat_id"], "@kept_chan");
    let listed = service.list()?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["chat_id"], "@kept_chan");
    assert_eq!(listed[0]["is_default"], true);
    assert_eq!(listed[0]["is_forum"], true);
    assert_eq!(listed[0]["rights"]["can_post_messages"], true);
    assert_eq!(listed[0]["topics"][0]["name"], "Saved topic");

    let account = AccountContext::account(&settings, 777, Role::Publish);
    let service = ChannelService::new(account, transport, String::new());
    let checked = service.check(json!("@kept_chan")).await?;
    assert_eq!(checked["channel"]["chat_id"], "-1002627012139");
    Ok(())
}

#[tokio::test]
async fn account_policy_cannot_send_to_an_unlinked_channel()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = settings(&dir, "http://127.0.0.1:9");
    let context = AccountContext::account(&settings, 777, Role::Publish);
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    let outcome = transport
        .call(
            &context,
            "sendRichMessage",
            json!({"chat_id":"-100999","rich_message":{"markdown":"x"}}),
            Uploads::default(),
        )
        .await;
    assert!(outcome.is_err());
    assert!(outcome.unwrap_err().to_string().contains("привяжите"));
    Ok(())
}

#[tokio::test]
async fn raw_methods_require_confirmation_for_changes() {
    assert!(!rich_studio::api::raw::needs_confirmation("getMe"));
    assert!(!rich_studio::api::raw::needs_confirmation("pinChatMessage"));
    assert!(rich_studio::api::raw::needs_confirmation("getUpdates"));
    assert!(rich_studio::api::raw::needs_confirmation("deleteMessage"));
    assert!(rich_studio::api::raw::needs_confirmation(
        "futureDangerousMethod"
    ));
    let dir = TempDir::new().unwrap_or_else(|error| panic!("temp: {error}"));
    let state = AppState::new(settings(&dir, "http://127.0.0.1:9"));
    let _ = state;
}

async fn http_request(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt as _;

    let mut builder = Request::builder()
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json: {error}")),
    )
}

#[tokio::test]
async fn channel_http_and_mcp_routes_share_the_service_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, &chat_server().await)));
    let (status, checked) = http_request(
        app.clone(),
        "POST",
        "/api/channels",
        Some(json!({"chat_id":"-1002627012139"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(checked["channel"]["title"], "МОИ ЗАМЕТКИ");
    let (status, topics) = http_request(
        app.clone(),
        "PUT",
        "/api/channels/-1002627012139/topics",
        Some(json!({"topics":[{"id":8,"name":"Новости"}]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(topics["topics"][0]["id"], 8);
    let (status, listed) = http_request(app.clone(), "GET", "/api/channels", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[1]["is_forum"], true);
    let (status, mcp) = http_request(app.clone(), "POST", "/mcp", Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_channels","arguments":{}}})), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(
        mcp["result"]["content"][1]["text"]
            .as_str()
            .ok_or("channel MCP content")?,
    )?;
    assert_eq!(listed["topics"][0]["name"], "Новости");
    let (status, mcp) = http_request(app.clone(), "POST", "/mcp", Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"check_channel","arguments":{"chat_id":"-1002627012139"}}})), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mcp["result"]["structuredContent"]["channel"]["chat_num"],
        -1002627012139i64
    );
    let (status, _) = http_request(app, "DELETE", "/api/channels/-1002627012139", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn check_endpoint_uses_default_chat_when_query_is_missing_or_empty()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, &chat_server().await)));
    for uri in ["/api/check", "/api/check?chat_id="] {
        let (status, checked) = http_request(app.clone(), "GET", uri, None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(checked["channel"]["chat_id"], "@main");
        assert_eq!(checked["can_post"], true);
    }
    Ok(())
}

#[tokio::test]
async fn raw_http_enforces_confirmation_and_maps_telegram_errors()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, &chat_server().await)));
    let (status, result) =
        http_request(app.clone(), "POST", "/api/raw/getMe", Some(json!({})), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["username"], "studio_bot");
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/raw/getUpdates",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, result) = http_request(
        app,
        "POST",
        "/api/raw/deleteMessage",
        Some(json!({"chat_id":"@main","message_id":1})),
        &[("x-confirm", "yes")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"], true);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|error| panic!("accept: {error}"));
        let mut buffer = [0u8; 8192];
        let _ = socket.read(&mut buffer).await;
        let body =
            json!({"ok":false,"error_code":400,"description":"Bad Request: refused"}).to_string();
        let response = format!(
            "HTTP/1.1 400 ERROR\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("write: {error}"));
    });
    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, &format!("http://{address}"))));
    let (status, error) = http_request(
        app,
        "POST",
        "/api/raw/sendMessage",
        Some(json!({"chat_id":"@main"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        error["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("refused")
    );
    Ok(())
}
