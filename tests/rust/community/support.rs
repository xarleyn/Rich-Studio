use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    services::{
        invite_links::InviteLinks, posts::PostService, suggested_posts::SuggestedPosts, updates,
    },
    settings::Settings,
    spec::source::Spec,
    store::{auth, suggested_posts as suggestions},
    telegram::client::TelegramTransport,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};
use tempfile::TempDir;
use tower::ServiceExt as _;

#[derive(Clone, Default)]
pub(crate) struct Mock {
    pub(crate) calls: Arc<Mutex<Vec<(String, Value)>>>,
    pub(crate) mode: Arc<AtomicU8>,
    pub(crate) entered: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
    links: Arc<Mutex<BTreeMap<String, Value>>>,
    link_ids: Arc<AtomicU64>,
    /// A getUpdates update delivered while the named Bot API call is still on the wire, before it answers.
    pub(crate) outcome_during: Arc<Mutex<Option<(String, AppState, Value)>>>,
}
impl Mock {
    pub(crate) fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock {e}"))
            .clone()
    }
    pub(crate) fn count(&self, name: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(method, _)| method == name)
            .count()
    }
    pub(crate) fn last(&self, name: &str) -> Value {
        self.calls()
            .into_iter()
            .rev()
            .find(|(method, _)| method == name)
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("no {name} call"))
    }
    fn invite_call(&self, method: &str, body: Value) -> Value {
        let mut links = self
            .links
            .lock()
            .unwrap_or_else(|error| panic!("lock {error}"));
        let link = if method.starts_with("create") {
            let id = self.link_ids.fetch_add(1, Ordering::SeqCst) + 1;
            let link = format!("https://t.me/+fixture{id}");
            links.insert(link.clone(), json!({}));
            link
        } else {
            body["invite_link"].as_str().unwrap_or_default().to_owned()
        };
        let stored = links.get(&link).cloned().unwrap_or_else(|| json!({}));
        if method == "revokeChatInviteLink" {
            let mut record = stored;
            record["invite_link"] = json!(link.clone());
            record["is_revoked"] = json!(true);
            links.insert(link, record.clone());
            return record;
        }
        let mut record = json!({
            "invite_link": link.clone(),
            "creator": {"id": 42, "is_bot": true, "first_name": "Bot"},
            "creates_join_request": false,
            "is_primary": false,
            "is_revoked": false
        });
        for key in ["subscription_period", "subscription_price"] {
            if let Some(value) = stored.get(key) {
                record[key] = value.clone();
            }
        }
        for key in [
            "name",
            "expire_date",
            "member_limit",
            "creates_join_request",
            "subscription_period",
            "subscription_price",
        ] {
            if let Some(value) = body.get(key)
                && !value.is_null()
                && value.as_str() != Some("")
            {
                record[key] = value.clone();
            }
        }
        links.insert(link, record.clone());
        record
    }
}
pub(crate) async fn mock()
-> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let mock = Mock::default();
    let app=axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async|State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>|{
        mock.calls.lock().unwrap_or_else(|e|panic!("lock {e}")).push((method.clone(),body.clone()));
        if ["approveSuggestedPost","declineSuggestedPost"].contains(&method.as_str()){
            let mode=mock.mode.load(Ordering::SeqCst);
            if [3,4].contains(&mode){mock.entered.notify_one();mock.release.notified().await;}
            if [1,4].contains(&mode){return(StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"not enough rights"})))}
            if mode==2{return(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"error_code":502,"description":"lost acknowledgement"})))}
        }
        let outcome={let mut slot=mock.outcome_during.lock().unwrap_or_else(|e|panic!("lock {e}"));if slot.as_ref().is_some_and(|(target,_,_)|target==&method){slot.take()}else{None}};
        if let Some((_,state,update))=outcome{let _=updates::handle_update(&state,&update).await;}
        let result=if method=="getChatMember"{json!({"status":if body["user_id"]==22{"member"}else{"creator"}})}else if method.contains("InviteLink"){
            mock.invite_call(&method,body)
        }else{json!(true)};
        (StatusCode::OK,Json(json!({"ok":true,"result":result})))
    })).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), mock, server))
}
pub(crate) fn settings(dir: &TempDir, base: &str) -> Settings {
    let mut settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), "1:fixture".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@test_channel".to_owned()),
        ("STUDIO_API_KEY".to_owned(), "owner-secret".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|e| panic!("settings {e}"));
    settings.api_base = base.to_owned();
    settings
}
pub(crate) fn services(
    settings: &Settings,
    context: AccountContext,
) -> (InviteLinks, SuggestedPosts) {
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    let posts = PostService::new(
        context.clone(),
        transport.clone(),
        settings,
        Spec::load_initial(settings),
    );
    (
        InviteLinks::new(context.clone(), posts, transport.clone()),
        SuggestedPosts::new(context, transport),
    )
}
pub(crate) fn suggestion(
    context: &AccountContext,
    mid: i64,
) -> Result<String, Box<dyn std::error::Error>> {
    suggestions::save(
        context,
        -100500,
        mid,
        &json!({"chat":{"id":-100500,"title":"News Direct Messages"},"message_id":mid,"text":"Proposal","from":{"first_name":"First","last_name":"Last","username":"author"},"direct_messages_topic":{"topic_id":2}}),
        &json!({"state":"pending","price_amount":100,"price_currency":"XTR"}),
    )?;
    Ok(suggestions::key(-100500, mid))
}
pub(crate) fn direct_messages() -> Value {
    json!({"id":-1009,"type":"supergroup","title":"Тест Direct Messages","is_direct_messages":true})
}
pub(crate) fn dm_suggestion_text(mid: i64, info: Value, text: &str) -> Value {
    json!({"message":{
        "message_id":mid,
        "date":auth::now(),
        "chat":direct_messages(),
        "from":{"id":5,"first_name":"Аня","username":"anya"},
        "direct_messages_topic":{"topic_id":77,"user":{"id":5,"first_name":"Аня"}},
        "text":text,
        "suggested_post_info":info
    }})
}
pub(crate) fn dm_suggestion(mid: i64, info: Value) -> Value {
    dm_suggestion_text(mid, info, "Мой пост для канала")
}
/// The service message Telegram sends about a suggestion's outcome.
pub(crate) fn dm_service(mid: i64, event: &str, details: Value) -> Value {
    let mut message = serde_json::Map::new();
    message.insert("message_id".to_owned(), json!(mid));
    message.insert("date".to_owned(), json!(1));
    message.insert("chat".to_owned(), direct_messages());
    message.insert(event.to_owned(), details);
    json!({"message": Value::Object(message)})
}
pub(crate) async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    credential: &str,
    session: bool,
    key: Option<&str>,
) -> (StatusCode, Value) {
    request_with(app, method, path, body, credential, session, key, None).await
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request_with(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    credential: &str,
    session: bool,
    key: Option<&str>,
    client: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .header("content-type", "application/json");
    builder = if session {
        builder.header("cookie", format!("rs_session={credential}"))
    } else {
        builder.header("authorization", format!("Bearer {credential}"))
    };
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key)
    }
    if let Some(client) = client {
        builder = builder.header("x-studio-client", client)
    }
    let response = app
        .oneshot(
            builder
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|e| panic!("request {e}")),
        )
        .await
        .unwrap_or_else(|e| panic!("response {e}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

pub(crate) fn member(link: &str, old: &str, new: &str, extra: Value) -> Value {
    let mut new_member = json!({"status": new, "user": {"id": 7}});
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            new_member[key.as_str()] = value.clone();
        }
    }
    json!({
        "chat_member": {
            "chat": {"id": -1001, "type": "channel"},
            "from": {"id": 7},
            "date": auth::now(),
            "old_chat_member": {"status": old, "user": {"id": 7}},
            "new_chat_member": new_member,
            "invite_link": {"invite_link": link, "creator": {"id": 42}, "is_primary": false, "is_revoked": false}
        }
    })
}

pub(crate) async fn mcp_call(
    app: axum::Router,
    tool: &str,
    arguments: Value,
    credential: &str,
) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
    let (status, response) = request(app, "POST", "/mcp", body, credential, false, None).await;
    assert_eq!(status, StatusCode::OK, "mcp {tool}: {response}");
    response["result"].clone()
}

pub(crate) fn mcp_list(result: &Value) -> Vec<Value> {
    result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    serde_json::from_str::<Value>(item["text"].as_str().unwrap_or_default()).ok()
                })
                .collect()
        })
        .unwrap_or_default()
}
