use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::http::StatusCode;
use rich_studio::account_context::AccountContext;
use rich_studio::services::editing::EditingService;
use rich_studio::services::{listing, scheduler};
use rich_studio::settings::Settings;
use rich_studio::spec::source::Spec;
use rich_studio::store::{editing, posts};
use rich_studio::telegram::client::TelegramTransport;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn config(dir: &TempDir) -> Settings {
    Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

fn configured(dir: &TempDir, base: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:fixture".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "@channel".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

fn now() -> i64 {
    rich_studio::store::auth::now()
}

async fn http(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (axum::http::StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(axum::body::Body::empty, |value| {
            axum::body::Body::from(value.to_string())
        }))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("json {bytes:?}: {error}"))
    };
    (status, value)
}

async fn served(
    dir: &TempDir,
) -> Result<(axum::Router, AppState, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>>
{
    let (base, mock, server) = mock().await?;
    let state = AppState::new(configured(dir, &base));
    let app = build_router(state.clone());
    Ok((app, state, mock, server))
}

fn last_call(mock: &Mock, method: &str) -> Option<Value> {
    mock.calls
        .lock()
        .unwrap_or_else(|error| panic!("mock lock: {error}"))
        .iter()
        .rev()
        .find(|(name, _)| name == method)
        .map(|(_, body)| body.clone())
}

fn call_count(mock: &Mock, method: &str) -> usize {
    mock.calls
        .lock()
        .unwrap_or_else(|error| panic!("mock lock: {error}"))
        .iter()
        .filter(|(name, _)| name == method)
        .count()
}

fn post(text: &str) -> Value {
    json!({"rich_message":{"markdown":text}})
}

fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

fn add(context: &AccountContext, status: &str, payload: &Value) -> Value {
    posts::add(
        context,
        posts::NewPost {
            chat_id: &json!("@channel"),
            payload,
            draft_id: None,
            status,
            message_id: Some(10),
            scheduled_at: Some(4070930400),
            error: None,
            kind: "rich",
            actor: "fixture",
        },
    )
    .unwrap_or_else(|error| panic!("post: {error}"))
}

#[tokio::test]
async fn waiting_edits_keep_history_enforce_version_and_never_reach_telegram()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let before = post("Before");
    let record = add(&context, "scheduled", &before);
    let id = record["id"].as_str().ok_or("id")?;
    let service = EditingService::new(
        context.clone(),
        TelegramTransport::new("http://127.0.0.1:1".to_owned(), "1:fixture".to_owned()),
        &settings,
        Spec::load_initial(&settings),
    );
    let changed = service.edit(id, &post("After"), "ui", Some(0)).await?;
    assert_eq!(changed["version"], 1);
    assert_eq!(changed["payload"], post("After"));
    assert!(matches!(
        service.edit(id, &post("Stale"), "other", Some(0)).await,
        Err(rich_studio::errors::ServiceError::Conflict(_))
    ));
    let history = service.versions(id).await?;
    assert_eq!(history.as_array().map(Vec::len), Some(1));
    assert_eq!(history[0]["payload"], before);
    assert_eq!(history[0]["actor"], "ui");
    assert_eq!(service.delete(id).await?["status"], "cancelled");
    assert!(
        service
            .reschedule(id, &json!(4070930500_i64))
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn claims_are_atomic_and_scheduler_race_cannot_replace_in_flight_content()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let old = post("Old");
    let record = add(&context, "scheduled", &old);
    let id = record["id"].as_str().ok_or("id")?;
    assert_eq!(editing::claim_version(&context, id, Some(0))?, Some(1));
    assert_eq!(editing::claim_version(&context, id, Some(0))?, None);
    assert_eq!(editing::claim_version(&context, id, None)?, Some(2));
    editing::release_version(&context, id, 1)?;
    assert_eq!(posts::get(&context, id)?.ok_or("post")?["version"], 2);
    assert_eq!(editing::claim_version(&context, "nope", None)?, None);
    rich_studio::store::Store::connect(&context)?
        .execute("update posts set status='sending' where id=?", [id])?;
    assert!(!editing::replace_scheduled(
        &context,
        id,
        &post("New"),
        &old,
        "api",
        1
    )?);
    assert_eq!(posts::get(&context, id)?.ok_or("post")?["payload"], old);
    assert!(editing::versions(&context, id)?.is_empty());
    Ok(())
}

#[test]
fn listing_preserves_titles_thumbnails_buttons_and_links() {
    assert_eq!(
        listing::title(
            &json!({"rich_message":{"html":"<h1>Привет &amp; &#x1f680;</h1><p>Вторая строка</p>"}}),
            Some("rich")
        )
        .unwrap_or_default(),
        "Привет & 🚀"
    );
    assert_eq!(
        listing::title(
            &json!({"rich_message":{"markdown":"## **Заголовок** [ссылка](https://example.org)"}}),
            Some("rich")
        )
        .unwrap_or_default(),
        "Заголовок ссылка"
    );
    let payload = json!({"live_photo":{"photo":"upload:still.jpg","caption":"Caption","reply_markup":{"inline_keyboard":[[{"text":"Go","callback_data":"cb"}]]}}});
    assert_eq!(
        listing::thumb(&payload),
        Some("/media/still.jpg".to_owned())
    );
    assert_eq!(
        listing::button_texts(&payload, Some("live_photo")).unwrap_or(Value::Null),
        json!({"cb":"Go"})
    );
    assert_eq!(
        listing::button_texts(
            &json!({"reply_markup":{"inline_keyboard":[[{"text":"Да","callback_data":"yes"}]]}}),
            None,
        )
        .unwrap_or(Value::Null),
        json!({"yes":"Да"})
    );
    assert_eq!(
        listing::title(&json!({"poll":{"question":"Как?"}}), Some("poll")).unwrap_or_default(),
        "Как?"
    );
    assert_eq!(
        listing::title(&json!({"paid_media":{}}), Some("paid_media")).unwrap_or_default(),
        "Платный контент"
    );
    assert_eq!(
        listing::link(&json!("-1001234"), &json!(8)),
        Some("https://t.me/c/1234/8".to_owned())
    );
}

#[derive(Clone)]
struct Mock {
    mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

async fn mock() -> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    use axum::{
        Json,
        extract::{Path, State},
        http::StatusCode,
    };
    let state = Mock {
        mode: Default::default(),
        calls: Default::default(),
        entered: Default::default(),
        release: Default::default(),
    };
    let app = axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async |State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>| {
        mock.calls.lock().unwrap_or_else(|error| panic!("mock lock: {error}")).push((method,body));
        let mode = mock.mode.load(std::sync::atomic::Ordering::SeqCst);
        if mode == 4 { mock.entered.notify_one(); mock.release.notified().await; }
        match mode {
            1 => (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"refused"}))),
            2 => (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}))),
            3 => (StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"error_code":502,"description":"gateway"}))),
            _ => (StatusCode::OK,Json(json!({"ok":true,"result":{"message_id":10}}))),
        }
    })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), state, server))
}

#[tokio::test]
async fn refused_edit_releases_but_uncertain_edit_keeps_claim_and_original_history()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let original = json!({"rich_message":{"markdown":"Old"},"callbacks":{"a":"answer"},"comment":{"markdown":"first"},"pin":true});
    let record = add(&context, "sent", &original);
    let id = record["id"].as_str().ok_or("id")?;
    let (base, mock, server) = mock().await?;
    let service = EditingService::new(
        context.clone(),
        TelegramTransport::new(base, "1:fixture".to_owned()),
        &settings,
        Spec::load_initial(&settings),
    );
    mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        service.edit(id, &post("New"), "ui", Some(0)).await,
        Err(rich_studio::errors::ServiceError::Telegram(_))
    ));
    assert_eq!(service.get(id).await?["version"], 0);
    mock.mode.store(3, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        service.edit(id, &post("New"), "ui", Some(0)).await,
        Err(rich_studio::errors::ServiceError::MayHaveGone(_))
    ));
    let retained = service.get(id).await?;
    assert_eq!(retained["version"], 1);
    assert_eq!(retained["payload"], original);
    assert_eq!(service.versions(id).await?, json!([]));
    assert!(matches!(
        service.edit(id, &post("New"), "ui", Some(0)).await,
        Err(rich_studio::errors::ServiceError::Conflict(_))
    ));
    assert_eq!(
        mock.calls
            .lock()
            .unwrap_or_else(|error| panic!("mock lock: {error}"))
            .len(),
        2
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn concurrent_expected_version_only_sends_one_edit_and_keeps_callbacks()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let original = json!({"rich_message":{"markdown":"Old"},"callbacks":{"a":"answer"},"comment":{"markdown":"first"},"reply_markup":{"inline_keyboard":[[{"text":"Old","callback_data":"a"}]]}});
    let record = add(&context, "sent", &original);
    let id = record["id"].as_str().ok_or("id")?.to_owned();
    let (base, mock, server) = mock().await?;
    let service = EditingService::new(
        context.clone(),
        TelegramTransport::new(base, "1:fixture".to_owned()),
        &settings,
        Spec::load_initial(&settings),
    );
    mock.mode.store(4, std::sync::atomic::Ordering::SeqCst);
    let first_service = service.clone();
    let first_id = id.clone();
    let first = tokio::spawn(async move {
        first_service
            .edit(&first_id, &post("New"), "first", Some(0))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    assert!(matches!(
        service.edit(&id, &post("Other"), "second", Some(0)).await,
        Err(rich_studio::errors::ServiceError::Conflict(_))
    ));
    mock.release.notify_one();
    let changed = first.await??;
    assert_eq!(changed["payload"]["callbacks"], original["callbacks"]);
    assert_eq!(changed["payload"]["comment"], original["comment"]);
    assert!(changed["payload"].get("reply_markup").is_none());
    assert_eq!(changed["version"], 1);
    assert_eq!(service.versions(&id).await?[0]["payload"], original);
    assert_eq!(
        mock.calls
            .lock()
            .unwrap_or_else(|error| panic!("mock lock: {error}"))
            .len(),
        1
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn not_modified_is_success_and_cancelled_edit_cannot_reuse_version()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let record = add(&context, "sent", &post("Old"));
    let id = record["id"].as_str().ok_or("id")?.to_owned();
    let (base, mock, server) = mock().await?;
    let service = EditingService::new(
        context.clone(),
        TelegramTransport::new(base, "1:fixture".to_owned()),
        &settings,
        Spec::load_initial(&settings),
    );
    mock.mode.store(2, std::sync::atomic::Ordering::SeqCst);
    let changed = json!({"rich_message":{"markdown":"Old"},"callbacks":{"a":"new answer"}});
    assert_eq!(
        service.edit(&id, &changed, "ui", Some(0)).await?["payload"]["callbacks"],
        changed["callbacks"]
    );
    mock.mode.store(4, std::sync::atomic::Ordering::SeqCst);
    let first_service = service.clone();
    let first_id = id.clone();
    let first = tokio::spawn(async move {
        first_service
            .edit(&first_id, &post("Unknown"), "ui", Some(1))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    first.abort();
    let _ = first.await;
    assert_eq!(service.get(&id).await?["version"], 2);
    assert!(matches!(
        service.edit(&id, &post("Duplicate"), "ui", Some(1)).await,
        Err(rich_studio::errors::ServiceError::Conflict(_))
    ));
    mock.release.notify_one();
    server.abort();
    Ok(())
}

#[tokio::test]
async fn raw_unchanged_edit_does_not_consume_a_new_history_version()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let payload = post("Unchanged");
    let record = add(&context, "sent", &payload);
    let id = record["id"].as_str().ok_or("id")?;
    let (base, mock, server) = mock().await?;
    mock.mode.store(2, std::sync::atomic::Ordering::SeqCst);
    let service = EditingService::new(
        context,
        TelegramTransport::new(base, "1:fixture".to_owned()),
        &settings,
        Spec::load_initial(&settings),
    );
    assert_eq!(
        service
            .edit_message(&payload, &json!("@channel"), 10, "api")
            .await?["unchanged"],
        true
    );
    assert_eq!(service.get(id).await?["version"], 0);
    assert_eq!(service.versions(id).await?, json!([]));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn http_buttons_reject_non_object_callbacks_before_any_telegram_call()
-> Result<(), Box<dyn std::error::Error>> {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt as _;
    let dir = TempDir::new()?;
    let config = config(&dir);
    let state = rich_studio::AppState::new(config);
    let record = add(&state.account, "sent", &post("Old"));
    let path = format!("/api/posts/{}/buttons", record["id"].as_str().ok_or("id")?);
    let app = rich_studio::build_router(state.clone());
    for callbacks in [json!(1), json!([])] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"reply_markup":null,"callbacks":callbacks}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(state.metrics.snapshot()["telegram_calls"], json!({}));
    assert_eq!(
        posts::get(&state.account, record["id"].as_str().ok_or("id")?)?.ok_or("post")?["version"],
        0
    );
    Ok(())
}

#[tokio::test]
async fn every_replacement_keeps_the_previous_content_and_actor() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post": original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let new = post("# Исправлено");
    let (status, first) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(new.clone()),
        &[("x-studio-client", "mcp:claude")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(post("третья")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let versions = versions.as_array().ok_or("versions")?;
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["payload"]["rich_message"], new["rich_message"]);
    assert_eq!(
        versions[1]["payload"]["rich_message"],
        original["rich_message"]
    );
    assert_eq!(versions[1]["actor"], "mcp:claude");
    assert!(
        versions[0]["replaced_at"].as_i64().ok_or("replaced_at")?
            >= versions[1]["replaced_at"].as_i64().ok_or("replaced_at")?
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn edit_by_message_id_also_keeps_a_version() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post": original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let sent = sent["post"].clone();
    let id = sent["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        "/api/edit",
        Some(json!({"post":post("# Исправлено"),"message_id":sent["message_id"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let versions = versions.as_array().ok_or("versions")?;
    assert_eq!(versions.len(), 1);
    assert_eq!(
        versions[0]["payload"]["rich_message"],
        original["rich_message"]
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduled_post_replacement_keeps_a_version() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(post("# Исправлено")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions.as_array().map(Vec::len), Some(1));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn versions_of_unknown_post_is_404() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, body) = http(app, "GET", "/api/posts/nope/versions", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reschedule_moves_a_waiting_post() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?;
    let at = now() + 7200;
    let (status, moved) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":at})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["scheduled_at"], json!(at));
    assert_eq!(moved["status"], "scheduled");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn missed_post_can_be_rescheduled_or_cancelled() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let mut records = Vec::new();
    for _ in 0..2 {
        let (status, record) = http(
            app.clone(),
            "POST",
            "/api/schedule",
            Some(json!({"post":blocks_post(),"at":now() + 3600})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{record}");
        records.push(record);
    }
    let connection = rich_studio::store::Store::connect(&state.account)?;
    for record in &records {
        connection.execute(
            "update posts set status='missed', error=?, retry_at=1 where id=?",
            rusqlite::params![
                "студия не работала",
                record["id"].as_str().ok_or("post id")?
            ],
        )?;
    }
    let first = records[0]["id"].as_str().ok_or("post id")?;
    let second = records[1]["id"].as_str().ok_or("post id")?;
    let (status, moved) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{first}/reschedule"),
        Some(json!({"at":"2099-01-01T09:00:00+03:00"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["status"], "scheduled");
    assert!(moved["error"].is_null());
    assert!(moved["retry_at"].is_null());
    let (status, cancelled) = http(app, "DELETE", &format!("/api/posts/{second}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn sent_post_cannot_be_rescheduled_and_past_time_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() + 60})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("sent")
    );
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?;
    let (status, refused) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() - 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("прошло")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn draft_remembers_the_post_it_edits() -> TestResult {
    let dir = TempDir::new()?;
    let (app, _state, _mock, server) = served(&dir).await?;
    let original = blocks_post();
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, draft) = http(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Правка","post":original,"post_id":pid})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["post_id"], pid);
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    let (status, updated) = http(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{draft_id}"),
        Some(json!({"title":"Правка","post":post("# Исправлено"),"version":draft["version"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert!(updated["post_id"].is_null());
    let (status, listed) = http(app, "GET", "/api/drafts", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed[0]["post_id"].is_null());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn rest_never_sends_an_edit_copy_as_a_second_post_unless_asked() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@x"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    let (status, draft) = http(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"правка","post":blocks_post(),"post_id":pid})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let draft_id = draft["id"].clone();
    for (path, at) in [("/api/send", None), ("/api/schedule", Some(now() + 3600))] {
        for dry in [false, true] {
            let mut body = json!({
                "post":blocks_post(),"chat_id":"@x","draft_id":draft_id,"dry_run":dry
            });
            if let Some(at) = at {
                body["at"] = json!(at);
            }
            let (status, refused) = http(app.clone(), "POST", path, Some(body), &[]).await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert!(
                refused["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&format!("/api/posts/{pid}/edit")),
                "{refused}"
            );
        }
    }
    assert_eq!(posts::list(&state.account)?.len(), 1);
    let (status, copied) = http(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_id":"@x","draft_id":draft_id,"as_new":true})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{copied}");
    assert_eq!(posts::list(&state.account)?.len(), 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn edit_based_on_an_older_version_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let original = blocks_post();
    let new = post("# Исправлено");
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":original})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let sent = sent["post"].clone();
    let id = sent["id"].as_str().ok_or("post id")?.to_owned();
    assert_eq!(sent["version"], 0);
    let (status, first) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["version"], 1);
    let edits = call_count(&mock, "editMessageText");
    let (status, late) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(post("чужая")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{late}");
    assert!(
        late["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("версия 1"),
        "{late}"
    );
    assert_eq!(call_count(&mock, "editMessageText"), edits);
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["payload"]["rich_message"],
        new["rich_message"]
    );
    let (status, second) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/edit"),
        Some(original),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["version"], 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn refused_edit_leaves_the_version_alone() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let new = post("# Исправлено");
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{refused}");
    assert_ne!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["version"],
        0
    );
    mock.mode.store(0, std::sync::atomic::Ordering::SeqCst);
    let (status, retry) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retry}");
    assert_eq!(retry["version"], 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduling_to_several_channels_is_all_or_nothing() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    posts::list(&state.account)?;
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger refuse_second_post before insert on posts
         when (select count(*) from posts) >= 1
         begin select raise(abort, 'disk full'); end;",
    )?;
    let (status, failed) = http(
        app,
        "POST",
        "/api/schedule",
        Some(json!({"post":blocks_post(),"at":now() + 3600,"chat_ids":["@a","@b"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{failed}");
    assert!(posts::list(&state.account)?.is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn missed_post_is_edited_before_it_is_scheduled_again() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let old = post("старый");
    let new = post("новый");
    let record = posts::add_scheduled(
        &state.account,
        &[json!("@chan")],
        &old,
        now() - 7200,
        None,
        "rich",
        "api",
    )?[0]
        .clone();
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set status='missed', error=? where id=?",
        rusqlite::params!["студия не работала", id],
    )?;
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/edit?post_version=0"),
        Some(new.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["status"], "missed");
    assert_eq!(changed["version"], 1);
    let (status, versions) = http(
        app.clone(),
        "GET",
        &format!("/api/posts/{id}/versions"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions[0]["payload"]["rich_message"], old["rich_message"]);
    let (status, rescheduled) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/reschedule"),
        Some(json!({"at":now() + 60})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rescheduled}");
    rich_studio::store::Store::connect(&state.account)?.execute(
        "update posts set scheduled_at=? where id=?",
        rusqlite::params![now() - 1, id],
    )?;
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await?,
        1
    );
    let sent = last_call(&mock, "sendRichMessage").ok_or("sendRichMessage")?;
    assert_eq!(sent["rich_message"], new["rich_message"]);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn scheduled_buttons_are_a_version_and_wait_for_the_scheduler() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, _mock, server) = served(&dir).await?;
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, record) = http(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":post("пост"),"at":now() + 3600})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().ok_or("post id")?.to_owned();
    let (status, changed) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[("x-studio-client", "ui")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["version"], 1);
    assert_eq!(changed["payload"]["reply_markup"], keyboard);
    let (status, versions) = http(
        app.clone(),
        "GET",
        &format!("/api/posts/{id}/versions"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let before = versions[0].clone();
    assert!(
        before["payload"]
            .get("reply_markup")
            .is_none_or(Value::is_null)
    );
    assert_eq!(before["actor"], "ui");
    let (status, same) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(same["version"], 1);
    rich_studio::store::Store::connect(&state.account)?.execute_batch(
        "create trigger studio_scheduler_claim after update of version on posts
         when new.version = old.version + 1 and old.status = 'scheduled'
         begin update posts set status = 'sending' where id = new.id; end;",
    )?;
    let (status, refused) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":null})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("уже отправляется"),
        "{refused}"
    );
    let stored = posts::get(&state.account, &id)?.ok_or("post")?;
    assert_eq!(stored["payload"]["reply_markup"], keyboard);
    assert_eq!(stored["version"], 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn refused_buttons_give_the_version_back() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post("пост")})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(1, std::sync::atomic::Ordering::SeqCst);
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, refused) = http(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let stored = posts::get(&state.account, &id)?.ok_or("post")?;
    assert_eq!(stored["version"], 0);
    assert!(
        stored["payload"]
            .get("reply_markup")
            .is_none_or(Value::is_null)
    );
    let (status, versions) =
        http(app, "GET", &format!("/api/posts/{id}/versions"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    assert_eq!(versions, json!([]));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn buttons_cut_off_mid_request_may_have_changed() -> TestResult {
    let dir = TempDir::new()?;
    let (app, state, mock, server) = served(&dir).await?;
    let (status, sent) = http(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post("пост")})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    mock.mode.store(3, std::sync::atomic::Ordering::SeqCst);
    let keyboard = json!({"inline_keyboard":[[{"text":"Сайт","url":"https://t.me/telegram"}]]});
    let (status, uncertain) = http(
        app,
        "POST",
        &format!("/api/posts/{id}/buttons"),
        Some(json!({"reply_markup":keyboard})),
        &[],
    )
    .await;
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uncertain}");
    assert!(
        uncertain["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Кнопки могли смениться"),
        "{uncertain}"
    );
    assert_eq!(
        posts::get(&state.account, &id)?.ok_or("post")?["version"],
        1
    );
    server.abort();
    Ok(())
}
