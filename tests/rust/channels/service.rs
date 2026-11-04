use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::channels::ChannelService;
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::telegram::uploads::Uploads;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::support::{blocks_post, chat_server, fake_server, http_request, settings};

#[tokio::test]
async fn channels_remember_rights_topics_and_default() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir, &chat_server().await));
    let context = AccountContext::root(&state.settings);
    let transport = TelegramTransport::new(
        state.settings.api_base.clone(),
        state.settings.bot_token.clone(),
    );
    let service = ChannelService::new(
        context.clone(),
        transport,
        state.settings.default_chat.clone(),
    );
    let listed = service.list()?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["chat_id"], "@main");
    assert_eq!(listed[0]["is_default"], true);
    assert_eq!(listed[0]["title"], Value::Null);
    assert_eq!(listed[0]["topics"], json!([]));
    let checked = service.check(json!("-1002627012139")).await?;
    assert_eq!(checked["channel"]["title"], "МОИ ЗАМЕТКИ");
    assert_eq!(checked["channel"]["is_forum"], 1);
    assert_eq!(checked["channel"]["can_post"], 1);
    assert_eq!(checked["channel"]["rights"]["status"], "administrator");
    assert_eq!(checked["channel"]["rights"]["can_post_messages"], true);
    let stored = service
        .list()?
        .into_iter()
        .find(|channel| channel["chat_id"] == "-1002627012139")
        .ok_or("checked channel is not in the list")?;
    assert_eq!(stored["title"], "МОИ ЗАМЕТКИ");
    assert_eq!(stored["is_forum"], true);
    let topics = service.set_topics("-1002627012139", vec![json!({"id":7,"name":"Анонсы"})])?;
    assert_eq!(topics["topics"], json!([{"id":7,"name":"Анонсы"}]));
    assert!(
        service
            .set_topics("-1002627012139", vec![json!({"id":0})])
            .is_err()
    );
    assert!(service.set_topics("@missing", vec![]).is_err());
    let app = build_router(state);
    let (status, _) = http_request(
        app.clone(),
        "PUT",
        "/api/channels/-1002627012139/topics",
        Some(json!({"topics":[{"id":0}]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = http_request(
        app.clone(),
        "PUT",
        "/api/channels/@nope/topics",
        Some(json!({"topics":[]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, checked) =
        http_request(app.clone(), "GET", "/api/check?chat_id=@other", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(checked["channel"]["chat_id"], "@other");
    let (_, listed) = http_request(app, "GET", "/api/channels", None, &[]).await;
    assert!(listed.as_array().is_some_and(|channels| {
        channels
            .iter()
            .any(|channel| channel["chat_id"] == "@other")
    }));
    service.remove("-1002627012139")?;
    assert_eq!(service.list()?.len(), 2);
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

#[tokio::test]
async fn channel_list_is_normalized_and_an_empty_item_refused()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, telegram) = fake_server().await;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@Chan"," @chan ","-1001",-1001]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent = telegram
        .calls_to("sendRichMessage")
        .into_iter()
        .map(|call| call["body"]["chat_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(sent, vec![json!("@Chan"), json!(-1001)]);
    for bad in [
        json!(["@a", ""]),
        json!(["  "]),
        json!(["chan"]),
        json!(["@a b"]),
    ] {
        let (status, _) = http_request(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post(),"chat_ids":bad})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(telegram.calls_to("sendRichMessage").len(), 2);
    let (status, draft) = http_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"A","post":blocks_post(),"chat_ids":[" @x","@X","-100"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["chat_ids"], json!(["@x", "-100"]));
    let (status, _) = http_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"B","post":blocks_post(),"chat_ids":["@x",""]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = http_request(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":" @trim "})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        telegram.last("sendRichMessage").ok_or("send")?["body"]["chat_id"],
        "@trim"
    );
    Ok(())
}
