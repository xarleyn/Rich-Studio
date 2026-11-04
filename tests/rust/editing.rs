use std::collections::BTreeMap;

use rich_studio::account_context::AccountContext;
use rich_studio::services::editing::EditingService;
use rich_studio::services::listing;
use rich_studio::settings::Settings;
use rich_studio::spec::source::Spec;
use rich_studio::store::{editing, posts};
use rich_studio::telegram::client::TelegramTransport;
use serde_json::{Value, json};
use tempfile::TempDir;

fn config(dir: &TempDir) -> Settings {
    Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

fn post(text: &str) -> Value {
    json!({"rich_message":{"markdown":text}})
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
