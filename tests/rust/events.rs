use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    errors::ServiceError,
    services::{
        accounts, bot_dialog, bot_notifications, import_forward, in_flight, kinds,
        publisher::Publisher, scheduler as scheduler_service, updates,
    },
    settings::Settings,
    store::{
        auth, bot_dialog as db, channels, editing, import_inbox, meta, posts,
        scheduler as scheduler_store,
    },
    telegram::{TelegramError, client::TelegramTransport, uploads::Uploads},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};
use tempfile::TempDir;
use tower::ServiceExt as _;

#[derive(Clone, Default)]
struct Mock {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    queue: Arc<Mutex<Vec<(String, Value)>>>,
    mode: Arc<AtomicU8>,
    updates: Arc<Mutex<Value>>,
    changed: Arc<tokio::sync::Notify>,
}
impl Mock {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clone()
    }
    fn count(&self, method: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(name, _)| name == method)
            .count()
    }
    fn respond(&self, method: &str, response: Value) {
        self.queue
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .push((method.to_owned(), response));
    }
    fn clear_calls(&self) {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clear();
    }
}
async fn mock() -> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
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
fn settings(dir: &TempDir, base: &str) -> Settings {
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
fn add(context: &AccountContext, chat: i64, mid: i64, payload: Value) -> Value {
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
fn callback(id: i64, chat: i64, mid: i64, data: &str) -> Value {
    json!({"update_id":id,"callback_query":{"id":format!("cq{id}"),"from":{"id":chat},"message":{"chat":{"id":chat,"type":if chat>0{"private"}else{"channel"}},"message_id":mid},"data":data}})
}
fn private(id: i64, text: &str) -> Value {
    private_in(id, text, "en")
}
fn private_in(id: i64, text: &str, language: &str) -> Value {
    json!({"message":{"message_id":1,"from":{"id":id,"language_code":language},"chat":{"id":id,"type":"private"},"text":text}})
}
fn private_ru(id: i64, text: &str) -> Value {
    private_in(id, text, "ru")
}
fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}
fn notices(mock: &Mock) -> Vec<Value> {
    mock.calls()
        .into_iter()
        .filter(|(method, _)| method == "sendMessage")
        .map(|(_, body)| body)
        .collect()
}
fn notice_texts(mock: &Mock) -> Vec<String> {
    notices(mock)
        .into_iter()
        .filter_map(|body| body["text"].as_str().map(str::to_owned))
        .collect()
}
fn last_body(mock: &Mock, method: &str) -> Value {
    mock.calls()
        .into_iter()
        .filter(|(name, _)| name == method)
        .next_back()
        .map(|(_, body)| body)
        .unwrap_or(Value::Null)
}
fn subscribe_with(
    settings: &Settings,
    uid: i64,
    mode: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    db::touch(settings, uid, "ru", Some(mode), false)?;
    Ok(())
}
fn stored(
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
fn publisher_for(state: &AppState, context: &AccountContext) -> Publisher {
    Publisher::new(
        context.clone(),
        state.transport(),
        state.settings.updates_on,
    )
    .with_settings(&state.settings)
}
fn now() -> i64 {
    auth::now()
}
async fn request(app: axum::Router, body: Value, secret: Option<&str>) -> (StatusCode, Value) {
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

#[tokio::test]
async fn webhook_secret_strict_id_and_persistent_claim_prevent_duplicate_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    config.updates_mode = "webhook".to_owned();
    config.webhook_secret = "verified-secret".to_owned();
    let state = AppState::new(config.clone());
    let post = add(
        &state.account,
        11,
        7,
        json!({"rich_message":{"markdown":"Hi"},"callbacks":{"a":"answer"}}),
    );
    let app = build_router(state.clone());
    let update = callback(8, 11, 7, "a");
    assert_eq!(
        request(app.clone(), update.clone(), None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(app.clone(), update.clone(), Some("wrong")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            json!({"update_id":true}),
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(app.clone(), update.clone(), Some("verified-secret"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            build_router(AppState::new(config.clone())),
            update,
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("answerCallbackQuery"), 1);
    assert_eq!(
        posts::get(&state.account, post["id"].as_str().ok_or("id")?)?.ok_or("post")?["clicks"]["a"],
        1
    );
    mock.mode.store(1, Ordering::SeqCst);
    let failed = callback(9, 11, 7, "a");
    assert_eq!(
        request(app.clone(), failed.clone(), Some("verified-secret"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app, failed, Some("verified-secret")).await.0,
        StatusCode::OK
    );
    assert_eq!(mock.count("answerCallbackQuery"), 2);
    assert_eq!(updates::public_status(&state).await["mode"], "webhook");
    config.updates_mode = "polling".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            json!({"update_id":10}),
            Some("verified-secret")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
    Ok(())
}

#[test]
fn webhook_configuration_requires_secret_and_https_public_url() {
    let config = |pairs: &[(&str, &str)]| {
        Settings::from_map(
            &pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    assert!(
        config(&[
            ("TG_UPDATES_MODE", "webhook"),
            ("STUDIO_URL", "https://example.org"),
        ])
        .is_err()
    );
    assert!(
        config(&[
            ("TG_UPDATES_MODE", "webhook"),
            ("TG_WEBHOOK_SECRET", "valid"),
            ("STUDIO_URL", "http://example.org"),
        ])
        .is_err()
    );
    let valid = config(&[
        ("TG_UPDATES_MODE", "webhook"),
        ("TG_WEBHOOK_SECRET", "valid"),
        ("STUDIO_URL", "https://example.org"),
    ])
    .unwrap_or_else(|error| panic!("settings: {error}"));
    assert_eq!(valid.updates_mode, "webhook");
}

#[tokio::test]
async fn preview_callback_keeps_user_meaning_and_newest_twenty_answers()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    // Lexicographic order differs from insertion order around message 9/10.
    for message in 1..=21 {
        updates::remember_preview(
            &state.account,
            &json!(11),
            message,
            &json!({"rsbot:stop":"content answer"}),
        )
        .await?;
    }
    let saved: Value = serde_json::from_str(
        &meta::get(&state.account, updates::PREVIEW_KEY)?.ok_or("callbacks")?,
    )?;
    assert_eq!(saved.as_object().ok_or("object")?.len(), 20);
    assert!(saved.get("11:1").is_none());
    assert!(saved.get("11:21").is_some());
    assert_eq!(
        updates::handle_update(&state, &callback(1, 11, 21, "rsbot:stop")).await?,
        Some("callback")
    );
    assert_eq!(mock.count("sendMessage"), 0);
    assert!(db::contact(&state.settings, 11)?.is_none());
    assert_eq!(mock.calls()[0].1["text"], "content answer");
    server.abort();
    Ok(())
}

fn link(context: &AccountContext, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    channels::save(
        context,
        &json!({"chat_id":chat.to_string(),"chat_num":chat,"type":"channel","title":"Test","can_post":1,"rights":{},"checked_at":0}),
    )?;
    Ok(())
}
#[tokio::test]
async fn public_events_check_current_membership_and_update_only_own_storage()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    for uid in [11, 22] {
        accounts::initialize(&config, uid)?;
        auth::save_account(&config, &json!({"id":uid,"first_name":"test"}))?;
    }
    let a = AccountContext::account(&config, 11, Role::Publish);
    let b = AccountContext::account(&config, 22, Role::Publish);
    link(&a, -100500)?;
    link(&b, -100500)?;
    let pa = add(&a, -100500, 4, json!({"rich_message":{"markdown":"A"}}));
    let pb = add(&b, -100500, 4, json!({"rich_message":{"markdown":"B"}}));
    for (context, post) in [(&a, &pa), (&b, &pb)] {
        posts::update_delivery(
            context,
            post["id"].as_str().ok_or("id")?,
            Some(-100500),
            Some("poll-shared"),
            None,
        )?;
    }
    let state = AppState::new(config);
    updates::handle_update(&state,&json!({"message_reaction_count":{"chat":{"id":-100500},"message_id":4,"reactions":[{"type":{"type":"emoji","emoji":"🔥"},"total_count":3}]}})).await?;
    updates::handle_update(&state,&json!({"poll":{"id":"poll-shared","total_voter_count":8,"options":[{"text":"yes","voter_count":8}]}})).await?;
    assert_eq!(
        posts::get(&a, pa["id"].as_str().ok_or("id")?)?.ok_or("post")?["reactions"]["🔥"],
        3
    );
    assert_eq!(
        posts::get(&a, pa["id"].as_str().ok_or("id")?)?.ok_or("post")?["poll_results"]["total"],
        8
    );
    let bpost = posts::get(&b, pb["id"].as_str().ok_or("id")?)?.ok_or("post")?;
    assert!(bpost["reactions"].is_null());
    assert!(bpost["poll_results"].is_null());
    assert_eq!(mock.count("getChatMember"), 4);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn first_comment_checks_link_and_uncertain_send_is_never_repeated()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    accounts::initialize(&config, 11)?;
    auth::save_account(&config, &json!({"id":11,"first_name":"test"}))?;
    let context = AccountContext::account(&config, 11, Role::Publish);
    link(&context, -100500)?;
    let post = add(
        &context,
        -100500,
        6,
        json!({"rich_message":{"markdown":"main"},"comment":{"markdown":"first"}}),
    );
    let id = post["id"].as_str().ok_or("id")?;
    posts::update_delivery(&context, id, Some(-100500), None, Some("waiting"))?;
    let state = AppState::new(config);
    let event = json!({"update_id":1,"message":{"message_id":60,"chat":{"id":-100600},"is_automatic_forward":true,"forward_origin":{"type":"channel","chat":{"id":-100500},"message_id":6}}});
    mock.mode.store(1, Ordering::SeqCst);
    assert_eq!(
        updates::handle_update(&state, &event).await?,
        Some("comment")
    );
    let saved = posts::get(&context, id)?.ok_or("post")?;
    assert_eq!(saved["comment_status"], "failed");
    assert!(saved["error"].as_str().ok_or("error")?.contains("мог уйти"));
    updates::handle_update(&state, &event).await?;
    assert_eq!(mock.count("sendRichMessage"), 1);
    let second = add(
        &context,
        -100500,
        8,
        json!({"rich_message":{"markdown":"main"},"comment":{"markdown":"first"}}),
    );
    let sid = second["id"].as_str().ok_or("id")?;
    posts::update_delivery(&context, sid, Some(-100500), None, Some("waiting"))?;
    mock.mode.store(3, Ordering::SeqCst);
    let mut event = event;
    event["message"]["forward_origin"]["message_id"] = json!(8);
    updates::handle_update(&state, &event).await?;
    assert_eq!(mock.count("sendRichMessage"), 1);
    assert_eq!(
        posts::get(&context, sid)?.ok_or("post")?["comment_status"],
        "failed"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn notification_revision_scope_and_attempt_claim_prevent_duplicates()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let context = AccountContext::root(&config);
    let transport = TelegramTransport::new(base, "1:fixture".to_owned());
    db::touch(&config, 11, "en", None, true)?;
    let post = add(
        &context,
        -100500,
        1,
        json!({"rich_message":{"markdown":"<b>safe</b>"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    let old = db::due(&context, 10)?.remove(0);
    db::disable(&config, 11)?;
    db::touch(&config, 11, "en", Some("all"), false)?;
    bot_notifications::deliver_one(&context, &config, &transport, &old).await?;
    assert_eq!(mock.count("sendMessage"), 0);
    let post = add(
        &context,
        -100500,
        2,
        json!({"rich_message":{"markdown":"Title"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    let event = db::due(&context, 10)?.remove(0);
    mock.mode.store(1, Ordering::SeqCst);
    bot_notifications::deliver_one(&context, &config, &transport, &event).await?;
    bot_notifications::deliver_one(&context, &config, &transport, &event).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(db::due(&context, 10)?.is_empty());
    let outbox_state: String = rich_studio::store::Store::connect(&context)?.query_row(
        "select state from bot_outbox where event_key=?",
        [event["event_key"].as_str().ok_or("key")?],
        |row| row.get(0),
    )?;
    assert_eq!(outbox_state, "attempted");
    let other = AccountContext::account(&config, 22, Role::Publish);
    accounts::initialize(&config, 22)?;
    db::enqueue(&other, "tampered", 11, "error", &event["body"])?;
    let tampered = db::due(&other, 10)?.remove(0);
    bot_notifications::deliver_one(&other, &config, &transport, &tampered).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_sender_binding_and_unsubscribe_survive_reply_rate_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    assert!(!bot_dialog::handle(&state, &private(22, "/start")).await?);
    let mut forged = private(11, "/start");
    forged["message"]["chat"]["id"] = json!(22);
    assert!(!bot_dialog::handle(&state, &forged).await?);
    assert!(bot_dialog::handle(&state, &private(11, "/start")).await?);
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "all"
    );
    for _ in 0..12 {
        auth::take_budget(&state.settings, "bot-dialog:11", 12, 60)?;
    }
    assert!(bot_dialog::handle(&state, &private(11, "/stop")).await?);
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "off"
    );
    assert_eq!(mock.count("sendMessage"), 1);
    // Repeated /start preserves an explicit opt-out, including after a restart.
    db::touch(&state.settings, 11, "en", None, true)?;
    assert_eq!(
        db::contact(&state.settings, 11)?.ok_or("contact")?["mode"],
        "off"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn polling_persists_offset_before_failed_handler_and_webhook_registration_is_exact()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    config.webhook_secret = "hook-secret".to_owned();
    let state = AppState::new(config);
    let post = add(
        &state.account,
        11,
        1,
        json!({"rich_message":{"markdown":"Hi"},"callbacks":{"x":"value"}}),
    );
    *mock.updates.lock().unwrap_or_else(|e| panic!("lock: {e}")) =
        json!([callback(44, 11, 1, "x")]);
    mock.mode.store(1, Ordering::SeqCst);
    updates::poll_once(&state).await?;
    updates::poll_once(&state).await?;
    assert_eq!(
        meta::get(&state.account, updates::OFFSET_KEY)?.as_deref(),
        Some("45")
    );
    assert_eq!(mock.count("answerCallbackQuery"), 1);
    assert_eq!(
        posts::get(&state.account, post["id"].as_str().ok_or("id")?)?.ok_or("post")?["clicks"]["x"],
        1
    );
    updates::register_webhook(&state).await?;
    let registration = mock
        .calls()
        .into_iter()
        .find(|(method, _)| method == "setWebhook")
        .ok_or("registration")?
        .1;
    assert_eq!(
        registration,
        json!({"url":"https://studio.example/api/telegram/webhook","secret_token":"hook-secret","max_connections":1,"allowed_updates":updates::ALLOWED_UPDATES})
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn webhook_loop_registers_once_with_exact_payload_and_stops()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    config.updates_mode = "webhook".to_owned();
    config.webhook_secret = "test_secret-123".to_owned();
    config.public_url = "https://example.org".to_owned();
    let state = AppState::new(config);
    let stopping = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(updates::run(state, stopping.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while mock.count("setWebhook") == 0 {
            mock.changed.notified().await;
        }
    })
    .await?;
    stopping.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    let registrations: Vec<Value> = mock
        .calls()
        .into_iter()
        .filter(|(method, _)| method == "setWebhook")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(
        registrations,
        [
            json!({"url":"https://example.org/api/telegram/webhook","secret_token":"test_secret-123","allowed_updates":updates::ALLOWED_UPDATES,"max_connections":1})
        ]
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn notifications_retry_only_known_non_delivery_and_blocked_recipient_opts_out()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let context = AccountContext::root(&config);
    db::touch(&config, 11, "en", Some("errors"), false)?;
    let mut post = add(
        &context,
        -100500,
        1,
        json!({"rich_message":{"markdown":"A"}}),
    );
    bot_notifications::record(&context, &config, &post, None).await?;
    assert!(db::due(&context, 10)?.is_empty());
    post["status"] = json!("failed");
    post["error"] = json!("fixture refusal");
    bot_notifications::record(&context, &config, &post, None).await?;
    let event = db::due(&context, 10)?.remove(0);
    let unreachable =
        TelegramTransport::new("http://127.0.0.1:0".to_owned(), "1:fixture".to_owned());
    bot_notifications::deliver_one(&context, &config, &unreachable, &event).await?;
    let (state, retry_at): (String, i64) = rich_studio::store::Store::connect(&context)?
        .query_row(
            "select state,retry_at from bot_outbox where event_key=?",
            [event["event_key"].as_str().ok_or("key")?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    assert_eq!(state, "pending");
    assert!(retry_at >= auth::now() + 59);
    assert!(db::due(&context, 10)?.is_empty());
    db::finish(
        &context,
        event["event_key"].as_str().ok_or("key")?,
        "pending",
        0,
    )?;
    let due = db::due(&context, 10)?.remove(0);
    mock.mode.store(2, Ordering::SeqCst);
    bot_notifications::deliver_one(
        &context,
        &config,
        &TelegramTransport::new(base, "1:fixture".to_owned()),
        &due,
    )
    .await?;
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(db::due(&context, 10)?.is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn leader_delivers_reports_even_when_updates_are_disabled_and_joins_on_stop()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.background = true;
    config.updates_on = false;
    config.spec_pinned = true;
    config.backup_keep = 0;
    config.shutdown_grace = 3.0;
    let state = AppState::new(config);
    db::touch(&state.settings, 11, "en", None, true)?;
    let post = add(
        &state.account,
        -100500,
        8,
        json!({"rich_message":{"markdown":"Published"}}),
    );
    bot_notifications::record(&state.account, &state.settings, &post, None).await?;
    let loops = rich_studio::services::loops::Loops::start(state.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if mock.count("sendMessage") == 1 {
                break;
            }
            mock.changed.notified().await;
        }
    })
    .await?;
    loops.request_stop();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !state.loop_status.read().await.stopping {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(
        meta::lease_holder(&state.account, "loops")?,
        Some(state.loop_status.read().await.owner.clone())
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), loops.stop()).await??;
    assert_eq!(mock.count("setMyCommands"), 3);
    assert_eq!(mock.count("getUpdates"), 0);
    assert_eq!(state.loop_status.read().await.role, "off");
    assert!(meta::lease_holder(&state.account, "loops")?.is_none());
    assert!(db::due(&state.account, 10)?.is_empty());
    let calls = mock.calls().len();
    tokio::task::yield_now().await;
    assert_eq!(mock.calls().len(), calls);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_start_help_menu_preferences_and_registered_commands()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    let state = AppState::new(config.clone());
    updates::handle_update(&state, &private_ru(11, "/help")).await?;
    // Help does not opt in and does not prevent a later /start.
    assert!(db::contact(&config, 11)?.is_none());
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("help notice")?
            .contains("/notifications")
    );
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start")).await?,
        Some("bot_command")
    );
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
    assert_eq!(
        last_body(&mock, "sendMessage")["reply_markup"]["inline_keyboard"][0][0]["url"],
        "https://studio.example"
    );
    for command in ["/status", "/studio", "/notifications", "/unknown", "Привет"] {
        assert_eq!(
            updates::handle_update(&state, &private_ru(11, command)).await?,
            Some("bot_command")
        );
    }
    let last = notice_texts(&mock).last().cloned().unwrap_or_default();
    assert!(!last.contains("<b>") || last.contains("Rich"));
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    updates::handle_update(&state, &private_ru(11, "/start")).await?;
    // /start never undoes an explicit opt-out.
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    updates::handle_update(&state, &private_ru(11, "/notifications errors")).await?;
    assert_eq!(
        db::contact(&config, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    // Migration/restart preserves the subscription.
    let restarted = AppState::new(config.clone());
    assert_eq!(
        db::contact(&restarted.settings, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    bot_dialog::register_commands(&state).await?;
    let commands: Vec<Value> = mock
        .calls()
        .into_iter()
        .filter(|(method, _)| method == "setMyCommands")
        .map(|(_, body)| body)
        .collect();
    let mut languages: Vec<&str> = commands
        .iter()
        .filter_map(|command| command["language_code"].as_str())
        .collect();
    languages.sort_unstable();
    assert_eq!(languages, ["", "en", "ru"]);
    assert!(
        commands
            .iter()
            .all(|command| command["scope"] == json!({"type":"all_private_chats"}))
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_commands_are_ignored_outside_own_private_chat()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let mut supergroup = private_ru(11, "/start");
    supergroup["message"]["chat"] = json!({"id":-1009,"type":"supergroup"});
    let mut other_private = private_ru(11, "/start");
    other_private["message"]["chat"] = json!({"id":888,"type":"private"});
    let mut bot = private_ru(11, "/start");
    bot["message"]["from"] = json!({"id":11,"is_bot":true});
    let mention = private_ru(11, "/start@SomeOtherBot");
    let mut forwarded = private_ru(11, "/start");
    forwarded["message"]["forward_origin"] = json!({"type":"hidden_user"});
    for update in [supergroup, other_private, bot, mention, forwarded] {
        assert!(updates::handle_update(&state, &update).await?.is_none());
    }
    assert_eq!(mock.count("sendMessage"), 0);
    assert!(db::contact(&config, 11)?.is_none());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_owner_only_in_private_mode_and_username_mentions()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    assert!(
        updates::handle_update(&state, &private_ru(888, "/start"))
            .await?
            .is_none()
    );
    assert!(db::contact(&config, 888)?.is_none());
    assert_eq!(mock.count("sendMessage"), 0);
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start@FiXtUrE")).await?,
        Some("bot_command")
    );
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn bot_dialog_callback_preferences_and_reply_budget() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let update = callback(1, 11, 1, "rsbot:notifications errors");
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("bot_command")
    );
    assert_eq!(
        db::contact(&config, 11)?.ok_or("contact")?["mode"],
        "errors"
    );
    assert_eq!(
        last_body(&mock, "answerCallbackQuery")["callback_query_id"],
        "cq1"
    );
    for _ in 0..20 {
        updates::handle_update(&state, &private_ru(11, "/help")).await?;
    }
    assert_eq!(mock.count("sendMessage"), 12);
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "off");
    assert_eq!(mock.count("sendMessage"), 12);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn public_bot_commands_do_not_create_or_read_another_account()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    let state = AppState::new(config.clone());
    stored(&state.account, &json!("@operator"), "sent", Some(1), None);
    assert_eq!(
        updates::handle_update(&state, &private_ru(101, "/start")).await?,
        Some("bot_command")
    );
    updates::handle_update(&state, &private_ru(101, "/status 777")).await?;
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("status notice")?
            .contains("Сначала")
    );
    assert!(auth::account_ids(&config)?.is_empty());
    assert!(!config.account_dir(101).exists());
    for uid in [101, 202] {
        auth::save_account(&config, &json!({"id":uid,"first_name":"Person"}))?;
        accounts::initialize(&config, uid)?;
        let context = AccountContext::account(&config, uid, Role::Publish);
        stored(&context, &json!("@own"), "failed", None, None);
        if uid == 202 {
            stored(&context, &json!("@secret"), "failed", None, None);
        }
    }
    // Another person's id in the text never selects their database.
    updates::handle_update(&state, &private_ru(101, "/status 202")).await?;
    let text = notice_texts(&mock).last().cloned().unwrap_or_default();
    assert!(text.contains("Требуют проверки: 1"), "{text}");
    assert!(text.contains("Опубликовано: 0"), "{text}");
    assert!(AccountContext::root(&config).account_id.is_none());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn private_dm_rejects_other_recipients_and_arbitrary_sends()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    let transport = state.transport();
    assert!(matches!(
        bot_dialog::send_private(&context, &config, &transport, 202, "secret", json!({})).await,
        Err(ServiceError::BadRequest(_))
    ));
    bot_dialog::send_private(&context, &config, &transport, 101, "hello", json!({})).await?;
    assert!(matches!(
        transport
            .call(
                &context,
                "sendMessage",
                json!({"chat_id":101,"text":"arbitrary"}),
                Uploads::default()
            )
            .await,
        Err(TelegramError::Policy(_))
    ));
    assert_eq!(mock.count("sendMessage"), 1);
    assert_eq!(last_body(&mock, "sendMessage")["chat_id"], 101);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reply_transport_failure_restores_account_policy() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let context = AccountContext::account(&config, 101, Role::Publish);
    let transport = state.transport();
    mock.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        bot_dialog::send_private(&context, &config, &transport, 101, "hello", json!({})).await,
        Err(ServiceError::Telegram(TelegramError::NoAnswer(_)))
    ));
    assert!(matches!(
        transport
            .call(
                &context,
                "sendMessage",
                json!({"chat_id":101,"text":"leak"}),
                Uploads::default()
            )
            .await,
        Err(TelegramError::Policy(_))
    ));
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn public_notifications_are_scoped_and_private() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    let state = AppState::new(config.clone());
    for uid in [101, 202] {
        auth::save_account(&config, &json!({"id":uid,"first_name":"Person"}))?;
        accounts::initialize(&config, uid)?;
        subscribe_with(&config, uid, "all")?;
        let context = AccountContext::account(&config, uid, Role::Publish);
        let post = stored(
            &context,
            &json!(format!("@channel_{uid}")),
            "sent",
            Some(10),
            None,
        );
        bot_notifications::record(&context, &config, &post, None).await?;
    }
    // A corrupted destination cannot leak this account's report.
    let context = AccountContext::account(&config, 101, Role::Publish);
    db::enqueue(
        &context,
        "forged",
        202,
        "success",
        &json!({"title":"private"}),
    )?;
    bot_notifications::deliver(&state).await?;
    let notices = notices(&mock);
    let mut chats = notices
        .iter()
        .map(|body| body["chat_id"].as_i64().ok_or("chat"))
        .collect::<Result<Vec<_>, _>>()?;
    chats.sort_unstable();
    assert_eq!(chats, [101, 202]);
    for body in &notices {
        let text = body["text"].as_str().unwrap_or_default();
        assert!(
            text.contains(&format!("@channel_{}", body["chat_id"])),
            "{text}"
        );
        assert!(!text.contains("private"), "{text}");
    }
    server.abort();
    Ok(())
}

#[tokio::test]
async fn publication_report_is_deduplicated_and_linked() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public_url = "https://studio.example".to_owned();
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@test_channel")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let post = results[0]["post"].clone();
    assert_eq!(post["status"], "sent");
    assert_eq!(mock.count("sendMessage"), 0);
    bot_notifications::record(&state.account, &config, &post, None).await?;
    bot_notifications::deliver(&state).await?;
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    let notice = last_body(&mock, "sendMessage");
    assert!(
        notice["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Пост опубликован")
    );
    let link = notice["reply_markup"]["inline_keyboard"][0][0]["url"]
        .as_str()
        .unwrap_or_default();
    assert!(link.starts_with("https://t.me/test_channel/"), "{link}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn notification_preferences_filter_and_discard_historical_reports()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let publisher = publisher_for(&state, &state.account);
    let kind = kinds::get(Some(kinds::RICH))?;
    publisher
        .publish(&[json!("@old")], &blocks_post(), None, kind, "test")
        .await?;
    subscribe_with(&config, 11, "errors")?;
    publisher
        .publish(&[json!("@ok")], &blocks_post(), None, kind, "test")
        .await?;
    assert!(db::due(&state.account, 10)?.is_empty());
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"Bad Request: no posting rights"}),
    );
    let refused = publisher
        .publish(&[json!("@refused")], &blocks_post(), None, kind, "test")
        .await;
    assert!(matches!(
        refused,
        Err(ServiceError::Telegram(TelegramError::Refused { .. }))
    ));
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("no posting rights")
    );
    subscribe_with(&config, 11, "all")?;
    publisher
        .publish(&[json!("@queued")], &blocks_post(), None, kind, "test")
        .await?;
    db::disable(&config, 11)?;
    bot_notifications::deliver(&state).await?;
    subscribe_with(&config, 11, "all")?;
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn uncertain_publication_reports_may_have_gone_without_retrying()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":502,"description":"gateway down"}),
    );
    let result = publisher_for(&state, &state.account)
        .publish(
            &[json!("@maybe")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await;
    assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));
    bot_notifications::deliver(&state).await?;
    let text = last_body(&mock, "sendMessage")["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(text.contains("Пост мог уйти"), "{text}");
    assert!(text.contains("Проверьте канал перед повтором"), "{text}");
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn failed_private_report_never_changes_or_duplicates_publication()
-> Result<(), Box<dyn std::error::Error>> {
    for (mode, expected) in [(2, "off"), (1, "all")] {
        let dir = TempDir::new()?;
        let (base, mock, server) = mock().await?;
        let config = settings(&dir, &base);
        let state = AppState::new(config.clone());
        subscribe_with(&config, 11, "all")?;
        let (results, _) = publisher_for(&state, &state.account)
            .publish(
                &[json!("@ok")],
                &blocks_post(),
                None,
                kinds::get(Some(kinds::RICH))?,
                "test",
            )
            .await?;
        let id = results[0]["post"]["id"]
            .as_str()
            .ok_or("post id")?
            .to_owned();
        mock.mode.store(mode, Ordering::SeqCst);
        bot_notifications::deliver(&state).await?;
        bot_notifications::deliver(&state).await?;
        assert_eq!(
            posts::get(&state.account, &id)?.ok_or("post")?["status"],
            "sent"
        );
        assert_eq!(mock.count("sendMessage"), 1);
        assert_eq!(
            db::contact(&config, 11)?.ok_or("contact")?["mode"],
            expected
        );
        server.abort();
    }
    Ok(())
}

#[tokio::test]
async fn definite_private_report_failure_is_retried_only_after_release()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let event = db::due(&state.account, 10)?.remove(0);
    let key = event["event_key"].as_str().ok_or("key")?.to_owned();
    let offline = TelegramTransport::new("http://127.0.0.1:0".to_owned(), config.bot_token.clone());
    bot_notifications::deliver_one(&state.account, &config, &offline, &event).await?;
    let (state_name, retry_at): (String, i64) = rich_studio::store::Store::connect(&state.account)?
        .query_row(
            "select state,retry_at from bot_outbox where event_key=?",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    assert_eq!(state_name, "pending");
    assert!(retry_at > now());
    db::finish(&state.account, &key, "pending", 0)?;
    let due = db::due(&state.account, 10)?.remove(0);
    bot_notifications::deliver_one(&state.account, &config, &state.transport(), &due).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn post_send_warning_is_reported_without_leaking_the_token()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let mut post = blocks_post();
    post["pin"] = json!(true);
    mock.respond(
        "pinChatMessage",
        json!({"ok":false,"error_code":400,"description":format!("<bad> {}", config.bot_token)}),
    );
    let (results, warnings) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &post,
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert!(!warnings.is_empty());
    assert_eq!(results[0]["post"]["status"], "sent");
    bot_notifications::deliver(&state).await?;
    let text = last_body(&mock, "sendMessage")["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(text.contains("опубликован с предупреждением"), "{text}");
    assert!(text.contains("&lt;bad&gt;"), "{text}");
    assert!(!text.contains(&config.bot_token), "{text}");
    let body: String = rich_studio::store::Store::connect(&state.account)?.query_row(
        "select body from bot_outbox",
        [],
        |row| row.get(0),
    )?;
    assert!(!body.contains(&config.bot_token), "{body}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn outbox_failure_cannot_change_a_successful_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger fail_outbox before insert on bot_outbox begin select raise(abort, 'disk full'); end;",
    )?;
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(results[0]["post"]["status"], "sent");
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduler_reports_sent_missed_retry_and_terminal_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.schedule_late_min = 1.0;
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let sent = stored(
        &state.account,
        &json!("@sent"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    let missed = stored(
        &state.account,
        &json!("@missed"),
        "scheduled",
        None,
        Some(now() - 1000),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    assert_eq!(
        posts::get(&state.account, sent["id"].as_str().ok_or("sent id")?)?.ok_or("sent")?["status"],
        "sent"
    );
    assert_eq!(
        posts::get(&state.account, missed["id"].as_str().ok_or("missed id")?)?.ok_or("missed")?["status"],
        "missed"
    );
    bot_notifications::deliver(&state).await?;
    assert_eq!(mock.count("sendMessage"), 2);
    let retry = stored(
        &state.account,
        &json!("@retry"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("задерживается")
    );
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set retry_at=0 where id=?",
        [retry["id"].as_str().ok_or("retry id")?],
    )?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"forbidden"}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        last_body(&mock, "sendMessage")["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Не удалось")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn crash_recovery_and_expired_comment_are_reported() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let uncertain = stored(&state.account, &json!("@uncertain"), "sending", None, None);
    let uncertain_id = uncertain["id"].as_str().ok_or("uncertain id")?.to_owned();
    let recovered = in_flight::recover_ids(&state.account)?;
    assert_eq!(recovered, [uncertain_id.clone()]);
    assert_eq!(
        posts::get(&state.account, &uncertain_id)?.ok_or("post")?["status"],
        "failed"
    );
    publisher_for(&state, &state.account)
        .notify(&uncertain_id, Some("uncertain"), None)
        .await;
    let comment = stored(&state.account, &json!("@comment"), "sent", Some(42), None);
    let comment_id = comment["id"].as_str().ok_or("comment id")?;
    posts::update_delivery(&state.account, comment_id, None, None, Some("waiting"))?;
    rich_studio::store::Store::connect(&state.account)?
        .execute("update posts set sent_at=1 where id=?", [comment_id])?;
    bot_notifications::expire_comments(&state.account, &config, 100, "Пересылка не пришла").await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts.iter().any(|text| text.contains("мог уйти")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("комментарий")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn import_start_is_not_replaced_by_the_welcome_message()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    let ticket = import_inbox::create(&state.account, "https://t.me/test_channel/10", 11, now())?;
    let token = ticket["id"].as_str().ok_or("ticket id")?.to_owned();
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, &format!("/start import_{token}"))).await?,
        Some("import")
    );
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("import notice")?
            .contains("перешлите")
    );
    assert_eq!(
        import_forward::get(&state.account, &token)?["state"],
        "armed"
    );
    assert!(db::contact(&config, 11)?.is_none());
    assert_eq!(
        updates::handle_update(&state, &private_ru(11, "/start import_missing")).await?,
        Some("bot_command")
    );
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("expired notice")?
            .contains("устарел")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn webhook_deduplicates_bot_commands() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config);
    let mut update = private_ru(11, "/start");
    update["update_id"] = json!(42);
    updates::receive_webhook(&state, &update).await?;
    updates::receive_webhook(&state, &update).await?;
    assert_eq!(mock.count("sendMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn english_bot_commands_and_reports() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    updates::handle_update(&state, &private_in(11, "/start", "en-US")).await?;
    assert!(notice_texts(&mock).last().ok_or("welcome")?.contains("Hi!"));
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    bot_notifications::deliver(&state).await?;
    assert!(
        notice_texts(&mock)
            .last()
            .ok_or("report")?
            .contains("Post published")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn preview_never_queues_a_publication_notice() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::services::posts::from_state(&state)
        .await
        .preview(&blocks_post(), &Value::Null)
        .await?;
    assert!(db::due(&state.account, 10)?.is_empty());
    assert_eq!(mock.count("sendRichMessage"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn partial_publication_reports_each_channel_once() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"refused"}),
    );
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@bad"), json!("@good")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(
        results
            .iter()
            .map(|result| result["ok"].clone())
            .collect::<Vec<_>>(),
        [json!(false), json!(true)]
    );
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts
            .iter()
            .any(|text| text.contains("@bad") && text.contains("Не удалось")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("@good") && text.contains("опубликован")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn outbox_claim_survives_restart_and_is_atomic() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@ok")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let event = db::due(&state.account, 10)?.remove(0);
    let key = event["event_key"].as_str().ok_or("key")?.to_owned();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let context = state.account.clone();
        let key = key.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            db::claim(&context, &key)
        }));
    }
    let mut claimed = Vec::new();
    for task in tasks {
        claimed.push(task.await??);
    }
    claimed.sort_unstable();
    assert_eq!(claimed, [false, true]);
    let restarted = AppState::new(config.clone());
    bot_notifications::deliver(&restarted).await?;
    assert_eq!(mock.count("sendMessage"), 0);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn first_comment_failure_is_reported_in_real_time() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.updates_on = true;
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let mut post = blocks_post();
    post["comment"] = json!({"rich_message":{"markdown":"first"}});
    mock.respond(
        "sendRichMessage",
        json!({"ok":true,"result":{"message_id":99,"chat":{"id":-1001}}}),
    );
    let (results, _) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@channel")],
            &post,
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    let saved = results[0]["post"].clone();
    assert_eq!(saved["comment_status"], "waiting");
    let update = json!({"message":{
        "message_id":999,
        "chat":{"id":-500,"type":"supergroup"},
        "is_automatic_forward":true,
        "forward_origin":{
            "type":"channel",
            "chat":{"id":saved["chat_num"]},
            "message_id":saved["message_id"]
        }
    }});
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"discussion unavailable"}),
    );
    assert_eq!(
        updates::handle_update(&state, &update).await?,
        Some("comment")
    );
    bot_notifications::deliver(&state).await?;
    assert!(
        notice_texts(&mock)
            .iter()
            .any(|text| text.contains("первый комментарий"))
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn after_send_failure_keeps_the_publication_and_warns()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger fail_delivery before update of chat_num on posts \
         begin select raise(abort, 'disk full'); end;",
    )?;
    let (results, warnings) = publisher_for(&state, &state.account)
        .publish(
            &[json!("@one"), json!("@two")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result["ok"] == true));
    assert!(!warnings.is_empty());
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts.iter().all(|text| text.contains("с предупреждением")),
        "{texts:?}"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn unsubscribe_then_resubscribe_does_not_replay_old_reports()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@old")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    updates::handle_update(&state, &private_ru(11, "/stop")).await?;
    updates::handle_update(&state, &private_ru(11, "/notifications all")).await?;
    mock.clear_calls();
    publisher_for(&state, &state.account)
        .publish(
            &[json!("@new")],
            &blocks_post(),
            None,
            kinds::get(Some(kinds::RICH))?,
            "test",
        )
        .await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("@new"), "{texts:?}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn delayed_retry_notice_is_discarded_after_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "scheduled",
        None,
        Some(now() - 1),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    mock.respond(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}}),
    );
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    let retry = db::due(&state.account, 10)?.remove(0);
    assert_eq!(retry["body"]["event"], "retry");
    rich_studio::store::Store::connect(&state.account)?
        .execute("update posts set retry_at=0 where id=?", [&id])?;
    scheduler_service::run_due(
        &state,
        state.account.clone(),
        now(),
        Arc::new(AtomicBool::new(false)),
    )
    .await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("опубликован"), "{texts:?}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn new_schedule_can_report_a_new_missed_deadline() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "scheduled",
        None,
        Some(100),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    for at in [100, 200] {
        if at != 100 {
            assert!(editing::reschedule(&state.account, &id, at)?);
        }
        assert!(scheduler_store::claim(&state.account, &id, at)?);
        assert!(scheduler_store::missed(&state.account, &id, "missed")?);
        let missed = posts::get(&state.account, &id)?.ok_or("missed post")?;
        bot_notifications::record(&state.account, &config, &missed, None).await?;
        bot_notifications::record(&state.account, &config, &missed, None).await?;
        bot_notifications::deliver(&state).await?;
    }
    assert_eq!(mock.count("sendMessage"), 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn rescheduled_post_discards_the_old_missed_report() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    let state = AppState::new(config.clone());
    subscribe_with(&config, 11, "all")?;
    let post = stored(
        &state.account,
        &json!("@channel"),
        "missed",
        None,
        Some(100),
    );
    let id = post["id"].as_str().ok_or("post id")?.to_owned();
    bot_notifications::record(&state.account, &config, &post, None).await?;
    assert!(editing::reschedule(&state.account, &id, 200)?);
    assert!(scheduler_store::claim(&state.account, &id, 200)?);
    assert!(scheduler_store::missed(
        &state.account,
        &id,
        "new deadline"
    )?);
    let current = posts::get(&state.account, &id)?.ok_or("rescheduled post")?;
    bot_notifications::record(&state.account, &config, &current, None).await?;
    bot_notifications::deliver(&state).await?;
    let texts = notice_texts(&mock);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("new deadline"), "{texts:?}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn preview_callbacks_take_priority_over_bot_commands()
-> Result<(), Box<dyn std::error::Error>> {
    for public in [false, true] {
        for data in ["rsbot:help", "rsbot:notifications off"] {
            let dir = TempDir::new()?;
            let (base, mock, server) = mock().await?;
            let mut config = settings(&dir, &base);
            config.public = public;
            let state = AppState::new(config.clone());
            subscribe_with(&config, 11, "all")?;
            if public {
                auth::save_account(&config, &json!({"id":11,"first_name":"Person"}))?;
                accounts::initialize(&config, 11)?;
            }
            let context = if public {
                AccountContext::account(&config, 11, Role::Publish)
            } else {
                AccountContext::root(&config)
            };
            let mut callbacks = serde_json::Map::new();
            callbacks.insert(data.to_owned(), json!({"text":"custom preview answer"}));
            updates::remember_preview(&context, &json!(11), 42, &Value::Object(callbacks)).await?;
            assert_eq!(
                updates::handle_update(&state, &callback(1, 11, 42, data)).await?,
                Some("callback")
            );
            assert_eq!(
                last_body(&mock, "answerCallbackQuery")["text"],
                "custom preview answer"
            );
            assert_eq!(mock.count("sendMessage"), 0);
            assert_eq!(db::contact(&config, 11)?.ok_or("contact")?["mode"], "all");
            server.abort();
        }
    }
    Ok(())
}
