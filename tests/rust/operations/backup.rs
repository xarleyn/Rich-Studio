use std::io::Read as _;
use std::time::{Duration, SystemTime};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    services::{backup, housekeeping, media::MediaService},
    store::{db::Store, editing as editing_store, idempotency, posts as posts_store},
};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::support::{
    TestResult, blocks_post, initialize, json_request, photo, settings, telegram,
    telegram_settings, upload,
};

#[test]
fn zip_keeps_legacy_databases_media_and_hashes_but_excludes_session_files() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.public = true;
    let root = AccountContext::root(&settings);
    let account = AccountContext::account(&settings, 42, Role::Publish);
    let database = initialize(&root)?;
    database.execute(
        "insert into accounts(telegram_id,first_name,created_at,updated_at)values(42,'user',0,0)",
        [],
    )?;
    database.execute(
        "insert into tokens(name,hash,level,created_at)values('kept','legacy-hash','admin',0)",
        [],
    )?;
    let account_db = initialize(&account)?;
    account_db.execute(
        "insert into drafts(id,title,payload)values('private','kept','{}')",
        [],
    )?;
    for context in [&root, &account] {
        std::fs::create_dir_all(&context.media_dir)?;
        std::fs::write(context.media_dir.join("image.bin"), b"image fixture")?;
        std::fs::write(
            context
                .database_path()
                .parent()
                .ok_or("directory")?
                .join("telegram-import.session.txt"),
            "IMPORT_SESSION_SECRET",
        )?;
    }
    std::fs::write(directory.path().join(".env"), "BOT_SECRET")?;
    let export = backup::export_zip(&settings, &root)?;
    let export_path = export.path().to_path_buf();
    let mut archive = zip::ZipArchive::new(std::fs::File::open(export.path())?)?;
    let names = archive.file_names().collect::<Result<Vec<_>, _>>()?;
    assert_eq!(names.len(), 4);
    assert!(
        names.iter().any(|name| name == "studio.db")
            && names.iter().any(|name| name == "accounts/42/studio.db")
            && names.iter().any(|name| name == "media/image.bin")
            && names
                .iter()
                .any(|name| name == "accounts/42/media/image.bin")
    );
    let restored = directory.path().join("restored.db");
    let mut bytes = Vec::new();
    archive.by_name("studio.db")?.read_to_end(&mut bytes)?;
    std::fs::write(&restored, bytes)?;
    let restored = rusqlite::Connection::open(restored)?;
    assert_eq!(
        restored.query_row("select hash from tokens where name='kept'", [], |row| {
            row.get::<_, String>(0)
        })?,
        "legacy-hash"
    );
    drop(archive);
    drop(export);
    assert!(!export_path.exists());
    let private = backup::export_zip(&settings, &account)?;
    let private = zip::ZipArchive::new(std::fs::File::open(private.path())?)?;
    assert_eq!(private.len(), 2);
    assert!(
        private
            .file_names()
            .all(|name| name.is_ok_and(|name| name.starts_with("accounts/42/")))
    );
    Ok(())
}

#[test]
fn snapshots_and_housekeeping_keep_user_data_without_opt_in() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.backup_keep = 1;
    let context = AccountContext::root(&settings);
    let connection = initialize(&context)?;
    connection.execute("insert into idempotency(key,scope,fingerprint,status,created_at)values('uncertain','send','hash','uncertain',0),('pending','send','hash','pending',0),('done','send','hash','done',0)",[])?;
    connection.execute("insert into post_versions(post_id,payload,replaced_at)values('post','{}',0),('post','{}',1)",[])?;
    std::fs::create_dir_all(&context.media_dir)?;
    std::fs::write(context.media_dir.join("unused.jpg"), b"fixture")?;
    let result = housekeeping::cleanup(&settings, &context, 86401)?;
    assert_eq!(result, json!({"idempotency_keys":1}));
    assert_eq!(
        connection.query_row("select count(*)from idempotency", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        connection.query_row("select count(*)from post_versions", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    assert!(context.media_dir.join("unused.jpg").exists());
    settings.versions_keep = 1;
    housekeeping::cleanup(&settings, &context, 86402)?;
    assert_eq!(
        connection.query_row("select count(*)from post_versions", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    let old = directory.path().join("backups/studio-20000101.db");
    std::fs::create_dir_all(old.parent().ok_or("parent")?)?;
    std::fs::write(&old, b"old")?;
    let snapshot = backup::daily_snapshot(&settings, &context)?.ok_or("snapshot")?;
    assert!(!old.exists());
    assert!(snapshot.exists());
    assert!(backup::daily_snapshot(&settings, &context)?.is_none());
    let copied = rusqlite::Connection::open(snapshot)?;
    assert_eq!(
        copied.query_row("pragma integrity_check", [], |row| row.get::<_, String>(0))?,
        "ok"
    );
    Ok(())
}

#[tokio::test]
async fn export_zip_contains_database_and_every_uploaded_file() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = AppState::new(settings);
    let _database = initialize(&state.account)?;
    let app = build_router(state);
    let uploaded = upload(app.clone(), "cat.png", b"\x89PNG-cat").await?;
    assert_eq!(uploaded["name"], "cat.png");
    let (status, draft) = json_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Кот","post":photo("cat.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let (status, sent) = json_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");

    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .uri("/api/export")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"].to_str()?,
        "application/zip"
    );
    let disposition = response.headers()["content-disposition"].to_str()?;
    assert!(disposition.contains("rich-studio-"), "{disposition}");
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut names = archive
        .file_names()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|name| name.into_owned())
        .collect::<Vec<String>>();
    names.sort();
    assert_eq!(names, ["media/cat.png", "studio.db"]);
    let mut media = Vec::new();
    archive.by_name("media/cat.png")?.read_to_end(&mut media)?;
    assert_eq!(media, b"\x89PNG-cat");
    let mut database = Vec::new();
    archive.by_name("studio.db")?.read_to_end(&mut database)?;
    let restored = directory.path().join("restored.db");
    std::fs::write(&restored, database)?;
    let restored = rusqlite::Connection::open(restored)?;
    assert_eq!(
        restored.query_row("select title from drafts", [], |row| row
            .get::<_, String>(0))?,
        "Кот"
    );
    assert_eq!(
        restored.query_row(
            "select count(*) from posts where status='sent'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn daily_snapshot_is_made_once_and_old_copies_are_pruned() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.backup_keep = 2;
    let context = AccountContext::root(&settings);
    let _database = initialize(&context)?;
    let backups = directory.path().join("backups");
    std::fs::create_dir_all(&backups)?;
    for day in ["20200101", "20200102", "20200103"] {
        std::fs::write(backups.join(format!("studio-{day}.db")), b"old")?;
    }
    let made = backup::daily_snapshot(&settings, &context)?.ok_or("snapshot")?;
    let now = time::OffsetDateTime::now_utc();
    let today = format!(
        "studio-{}{:02}{:02}.db",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    assert_eq!(
        made.file_name().and_then(|name| name.to_str()),
        Some(today.as_str())
    );
    assert!(backup::daily_snapshot(&settings, &context)?.is_none());
    let mut left = std::fs::read_dir(&backups)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    left.retain(|name| name.ends_with(".db"));
    left.sort();
    let mut expected = vec!["studio-20200103.db".to_owned(), today];
    expected.sort();
    assert_eq!(left, expected);
    let partials = std::fs::read_dir(&backups)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "part")
        })
        .count();
    assert_eq!(partials, 0);
    Ok(())
}

#[test]
fn daily_snapshot_can_be_disabled() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.backup_keep = 0;
    let context = AccountContext::root(&settings);
    let _database = initialize(&context)?;
    assert!(backup::daily_snapshot(&settings, &context)?.is_none());
    assert!(!directory.path().join("backups").exists());
    Ok(())
}

#[test]
fn housekeeping_deletes_user_data_only_past_its_retention() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    let context = AccountContext::root(&settings);
    let _database = initialize(&context)?;
    let post = posts_store::add(
        &context,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "sent",
            message_id: Some(7),
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?;
    let post_id = post["id"].as_str().ok_or("post id")?;
    let connection = Store::connect(&context)?;
    for version in 0..4 {
        connection.execute(
            "insert into post_versions(post_id,payload,replaced_at,actor)values(?,?,?,?)",
            rusqlite::params![post_id, json!({"v":version}).to_string(), version, "ui"],
        )?;
    }
    idempotency::claim(&context, "old", "send", "fp")?;
    idempotency::finish(&context, "old", "done", &json!({}))?;
    connection.execute("update idempotency set created_at=0", [])?;
    let media = MediaService::new(
        context.clone(),
        settings.max_upload_mb,
        settings.account_storage_mb,
    );
    for name in ["old.png", "new.png"] {
        media.upload(name, b"\x89PNG")?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(context.media_dir.join("old.png"))?
        .set_modified(SystemTime::now() - Duration::from_secs(40 * 86400))?;
    assert_eq!(
        housekeeping::cleanup(&settings, &context, housekeeping::now())?,
        json!({"idempotency_keys":1})
    );
    assert_eq!(editing_store::versions(&context, post_id)?.len(), 4);
    assert!(context.media_dir.join("old.png").exists());
    settings.versions_keep = 2;
    settings.media_unused_days = 30.0;
    let done = housekeeping::cleanup(&settings, &context, housekeeping::now())?;
    assert_eq!(done["post_versions"], 2);
    assert_eq!(done["media"], json!(["old.png"]));
    let payloads = editing_store::versions(&context, post_id)?
        .iter()
        .map(|version| version["payload"].clone())
        .collect::<Vec<_>>();
    assert_eq!(payloads, [json!({"v":3}), json!({"v":2})]);
    assert!(context.media_dir.join("new.png").exists());
    Ok(())
}
