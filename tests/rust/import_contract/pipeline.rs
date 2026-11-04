use futures_util::{StreamExt, stream};
use rich_studio::{
    account_context::{AccountContext, Role},
    errors::ImportError,
    services::imports::{self, SavedMedia},
    store::{db, drafts},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;

use crate::support::*;

#[test]
fn saved_result_creates_new_unpublished_draft_with_all_editor_buffers() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    db::migrate(&mut db::Store::connect(&context)?)?;
    let link = link()?;
    let existing = drafts::save(
        &context,
        None,
        "Existing",
        &json!({"rich_message":{"html":"Keep"}}),
        None,
        None,
        None,
    )?
    .ok_or("existing draft")?;
    let one = imports::save_result(
        &context,
        vec![
            json!({"type":"heading","size":2,"text":"Title"}),
            json!({"type":"paragraph","text":"Body"}),
        ],
        vec![],
        &link,
        "mtproto",
        "",
    )?;
    let one_id = one["draft"]["id"].as_str().ok_or("draft id")?;
    let existing_id = existing["id"].as_str().ok_or("existing id")?;
    assert_ne!(one_id, existing_id);
    assert_eq!(
        drafts::get(&context, existing_id)?.ok_or("existing gone")?["payload"]["rich_message"]["html"],
        "Keep"
    );
    assert_eq!(one["draft"]["payload"]["options"], json!({}));
    let buffers = &one["draft"]["payload"]["_studio"]["buffers"];
    assert!(
        buffers["html"]
            .as_str()
            .unwrap_or_default()
            .contains("<h2>Title</h2>")
    );
    assert!(
        buffers["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("## Title")
    );
    let two = imports::save_result(
        &context,
        vec![
            json!({"type":"photo","photo":{"type":"photo","media":"upload:photo.jpg"}}),
            json!({"type":"paragraph","text":"Second"}),
        ],
        vec!["Inspect".into(), "Inspect".into()],
        &link,
        "mtproto",
        "",
    )?;
    assert_ne!(one["draft"]["id"], two["draft"]["id"]);
    assert_eq!(drafts::list(&context)?.len(), 3);
    assert_eq!(two["warnings"], json!(["Inspect"]));
    assert_eq!(two["media_count"], 1);
    let payload = &two["draft"]["payload"];
    assert_eq!(payload["options"], json!({}));
    assert!(two["draft"]["post_id"].is_null());
    assert!(two["draft"]["chat_ids"].is_null());
    assert_eq!(payload["_studio"]["format"], "blocks");
    assert!(payload["_studio"]["buffers"]["html"].is_string());
    assert!(payload["_studio"]["buffers"]["markdown"].is_string());
    let count: i64 =
        db::Store::connect(&context)?
            .query_row("select count(*) from posts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn downloads_enforce_limits_and_clean_completed_and_partial_files_on_failure() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    settings.max_upload_mb = 0.00001;
    {
        let mut saved = SavedMedia::default();
        let chunks = stream::iter(vec![Ok(vec![1; 5]), Ok(vec![2; 100])]);
        let error =
            imports::save_download(&context, &settings, &mut saved, "photo", None, 0, chunks)
                .await
                .err()
                .ok_or("accepted large stream")?;
        assert_eq!(error.status, 413);
        assert_eq!(saved.count(), 0);
    }
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    {
        let mut saved = SavedMedia::default();
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            5,
            stream::iter(vec![Ok(vec![1; 5])]),
        )
        .await?;
        let result = imports::save_download(
            &context,
            &settings,
            &mut saved,
            "document",
            Some("../../danger.exe"),
            0,
            stream::iter(vec![Err(ImportError::new(502, "network"))]),
        )
        .await;
        assert!(result.is_err());
    }
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn failed_draft_save_cleans_downloaded_media_and_success_keeps_it() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::account(&settings, 123, Role::Drafts);
    db::migrate(&mut db::Store::connect(&context)?)?;
    let connection = db::Store::connect(&context)?;
    connection.execute("with recursive seq(n) as (select 1 union all select n+1 from seq where n<1000) insert into drafts(id,title,payload,created_at,updated_at) select 'seed-'||n,'','{}',0,0 from seq",[])?;
    let mut saved = SavedMedia::default();
    let photo = imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![1; 5])]),
    )
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    let error = imports::save_import(
        context.clone(),
        vec![json!({"type":"photo","photo":photo})],
        vec![],
        link()?,
        "mtproto",
        saved,
    )
    .await
    .err()
    .ok_or("draft quota ignored")?;
    assert_eq!(error.status, 413);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    assert_eq!(drafts::list(&context)?.len(), 1000);

    connection.execute("delete from drafts where id='seed-1'", [])?;
    let mut saved = SavedMedia::default();
    let photo = imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![2; 5])]),
    )
    .await?;
    let result = imports::save_import(
        context.clone(),
        vec![json!({"type":"photo","photo":photo})],
        vec![],
        link()?,
        "mtproto",
        saved,
    )
    .await?;
    assert_eq!(result["media_count"], 1);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    assert_eq!(drafts::list(&context)?.len(), 1000);
    Ok(())
}

#[tokio::test]
async fn failed_import_rolls_back_downloaded_files_and_never_saves_a_draft() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let mut saved = SavedMedia::default();
    imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![1; 5])]),
    )
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    let error = imports::save_import(context.clone(), vec![], vec![], link()?, "mtproto", saved)
        .await
        .err()
        .ok_or("empty import accepted")?;
    assert_eq!(error.status, 422);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    db::migrate(&mut db::Store::connect(&context)?)?;
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn cancellation_cleans_download_and_releases_active_account() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let signal = Arc::new(tokio::sync::Notify::new());
    let notified = signal.clone();
    let context2 = context.clone();
    let task = tokio::spawn(async move {
        let _active = imports::ActiveImport::acquire(Some(991))?;
        let mut saved = SavedMedia::default();
        let chunks = stream::once(async move {
            notified.notify_one();
            Ok(vec![1; 5])
        })
        .chain(stream::pending());
        imports::save_download(&context2, &settings, &mut saved, "photo", None, 0, chunks).await
    });
    tokio::time::timeout(Duration::from_secs(3), signal.notified()).await?;
    task.abort();
    let _ = task.await;
    // An already-running blocking write cannot be aborted. Its owned guard must remove the
    // file as soon as that bounded write returns, even after the request future is gone.
    tokio::time::timeout(Duration::from_secs(2), async {
        while std::fs::read_dir(&context.media_dir).is_ok_and(|entries| entries.count() > 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    let _again = imports::ActiveImport::acquire(Some(991))?;
    Ok(())
}

#[tokio::test]
async fn download_count_total_and_account_quota_boundaries() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    let context = AccountContext::account(&settings, 123, Role::Owner);
    settings.account_storage_mb = 0.00001;
    {
        let mut saved = SavedMedia::default();
        let error = imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            0,
            stream::iter(vec![Ok(vec![1; 20])]),
        )
        .await
        .err()
        .ok_or("quota ignored")?;
        assert_eq!(error.status, 413);
    }
    settings.account_storage_mb = 1.0;
    let mut saved = SavedMedia::default();
    for _ in 0..30 {
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            1,
            stream::iter(vec![Ok(vec![1])]),
        )
        .await?;
    }
    assert_eq!(
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            1,
            stream::iter(vec![Ok(vec![1])])
        )
        .await
        .err()
        .ok_or("31 files")?
        .status,
        413
    );
    drop(saved);
    let mut saved = SavedMedia::default();
    saved.total = imports::MAX_TOTAL_BYTES - 2;
    assert_eq!(
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            3,
            stream::iter(vec![Ok(vec![1; 3])])
        )
        .await
        .err()
        .ok_or("total ignored")?
        .status,
        413
    );
    Ok(())
}
