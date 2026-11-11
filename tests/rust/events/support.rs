use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    services::publisher::Publisher,
    settings::Settings,
    store::{auth, bot_dialog as db, channels, posts},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};
use tempfile::TempDir;
use tower::ServiceExt as _;

#[derive(Clone, Default)]
pub(crate) struct Mock {
    pub(crate) calls: Arc<Mutex<Vec<(String, Value)>>>,
    pub(crate) queue: Arc<Mutex<Vec<(String, Value)>>>,
    pub(crate) mode: Arc<AtomicU8>,
    pub(crate) updates: Arc<Mutex<Value>>,
    pub(crate) changed: Arc<tokio::sync::Notify>,
}
impl Mock {
    pub(crate) fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clone()
    }
    pub(crate) fn count(&self, method: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(name, _)| name == method)
            .count()
    }
    pub(crate) fn respond(&self, method: &str, response: Value) {
        self.queue
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .push((method.to_owned(), response));
    }
    pub(crate) fn clear_calls(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clear();
    }
}
pub(crate) async fn mock()
-> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let mock = Mock::default();
    let app=axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async|State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>|{
        mock.calls.lock().unwrap_or_else(|e|panic!("lock: {e}")).push((method.clone(),body.clone()));
        mock.changed.notify_one();
        let queued={let mut queue=mock.queue.lock().unwrap_or_else(|e|panic!("lock: {e}"));queue.iter().position(|(name,_)|name==&method).map(|index|queue.remove(index).1)};
        if let Some(response)=queued{return(StatusCode::OK,Json(response))}
        let mode=mock.mode.load(Ordering::SeqCst);
        if ["sendMessage","sendRichMessage","answerCallbackQuery"].contains(&method.as_str()) {
            if mode==1{return(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"error_code":502,"description":"lost response"})))}
            if mode==2{return(StatusCode::FORBIDDEN,Json(json!({"ok":false,"error_code":403,"description":"blocked"})))}
        }
        let result=match method.as_str(){
            "getChatMember"=>json!({"status":if body["user_id"]==22{"member"}else{"creator"}}),
            "getChat"=>json!({"id":body["chat_id"],"type":"channel","linked_chat_id":if mode==3{-100999}else{-100600}}),
            "getUpdates"=>mock.updates.lock().unwrap_or_else(|e|panic!("lock: {e}")).as_array().cloned().map(Value::Array).unwrap_or(json!([])),
            _=>json!({"message_id":99})
        };(StatusCode::OK,Json(json!({"ok":true,"result":result})))
    })).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), mock, task))
}
pub(crate) fn settings(dir: &TempDir, base: &str) -> Settings {
    let mut settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), "1:fixture".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|e| panic!("settings: {e}"));
    settings.api_base = base.to_owned();
    settings.preview_chat = "11".to_owned();
    settings.bot_username = "fixture".to_owned();
    settings
}
pub(crate) fn add(context: &AccountContext, chat: i64, mid: i64, payload: Value) -> Value {
    let post = posts::add(
        context,
        posts::NewPost {
            chat_id: &json!(chat),
            payload: &payload,
            draft_id: None,
            status: "sent",
            message_id: Some(mid),
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "test",
        },
    )
    .unwrap_or_else(|e| panic!("post: {e}"));
    posts::update_delivery(
        context,
        post["id"].as_str().unwrap_or(""),
        Some(chat),
        None,
        None,
    )
    .unwrap_or_else(|e| panic!("delivery: {e}"));
    post
}
pub(crate) fn callback(id: i64, chat: i64, mid: i64, data: &str) -> Value {
    json!({"update_id":id,"callback_query":{"id":format!("cq{id}"),"from":{"id":chat},"message":{"chat":{"id":chat,"type":if chat>0{"private"}else{"channel"}},"message_id":mid},"data":data}})
}
pub(crate) fn private(id: i64, text: &str) -> Value {
    private_in(id, text, "en")
}
pub(crate) fn private_in(id: i64, text: &str, language: &str) -> Value {
    json!({"message":{"message_id":1,"from":{"id":id,"language_code":language},"chat":{"id":id,"type":"private"},"text":text}})
}
pub(crate) fn private_ru(id: i64, text: &str) -> Value {
    private_in(id, text, "ru")
}
pub(crate) fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}
pub(crate) fn notices(mock: &Mock) -> Vec<Value> {
    mock.calls()
        .into_iter()
        .filter(|(method, _)| method == "sendMessage")
        .map(|(_, body)| body)
        .collect()
}
pub(crate) fn notice_texts(mock: &Mock) -> Vec<String> {
    notices(mock)
        .into_iter()
        .filter_map(|body| body["text"].as_str().map(str::to_owned))
        .collect()
}
pub(crate) fn last_body(mock: &Mock, method: &str) -> Value {
    mock.calls()
        .into_iter()
        .rfind(|(name, _)| name == method)
        .map(|(_, body)| body)
        .unwrap_or(Value::Null)
}
pub(crate) fn subscribe_with(
    settings: &Settings,
    uid: i64,
    mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    db::touch(settings, uid, "ru", Some(mode), false)?;
    Ok(())
}
pub(crate) fn stored(
    context: &AccountContext,
    chat: &Value,
    status: &str,
    message_id: Option<i64>,
    scheduled_at: Option<i64>,
) -> Value {
    let payload = blocks_post();
    posts::add(
        context,
        posts::NewPost {
            chat_id: chat,
            payload: &payload,
            draft_id: None,
            status,
            message_id,
            scheduled_at,
            error: None,
            kind: "rich",
            actor: "test",
        },
    )
    .unwrap_or_else(|e| panic!("post: {e}"))
}
pub(crate) fn publisher_for(state: &AppState, context: &AccountContext) -> Publisher {
    Publisher::new(
        context.clone(),
        state.transport(),
        state.settings.updates_on,
    )
    .with_settings(&state.settings)
}
pub(crate) fn now() -> i64 {
    auth::now()
}
pub(crate) async fn request(
    app: axum::Router,
    body: Value,
    secret: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/telegram/webhook")
        .header("host", "localhost")
        .header("content-type", "application/json");
    if let Some(secret) = secret {
        request = request.header("x-telegram-bot-api-secret-token", secret)
    }
    let response = app
        .oneshot(
            request
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
pub(crate) fn link(context: &AccountContext, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    channels::save(
        context,
        &json!({"chat_id":chat.to_string(),"chat_num":chat,"type":"channel","title":"Test","can_post":1,"rights":{},"checked_at":0}),
    )?;
    Ok(())
}
