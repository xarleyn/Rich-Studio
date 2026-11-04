use rich_studio::account_context::AccountContext;
use rich_studio::services::editing::EditingService;
use rich_studio::spec::source::Spec;
use rich_studio::telegram::client::TelegramTransport;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{add, config, mock, post};

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
