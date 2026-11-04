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
    services::{accounts, bot_dialog, bot_notifications, updates},
    settings::Settings,
    store::{auth, bot_dialog as db, channels, meta, posts},
    telegram::client::TelegramTransport,
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
}
async fn mock() -> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let mock = Mock::default();
    let app=axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async|State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>|{
        mock.calls.lock().unwrap_or_else(|e|panic!("lock: {e}")).push((method.clone(),body.clone()));
        mock.changed.notify_one();
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
    json!({"message":{"from":{"id":id,"language_code":"en"},"chat":{"id":id,"type":"private"},"text":text}})
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
