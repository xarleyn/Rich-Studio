use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    logs::{self, Logger},
    metrics::Metrics,
    services::{backup, health, housekeeping, loops::LoopStatus},
    settings::Settings,
    store::db::{self, Store},
};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
fn settings(directory: &TempDir) -> Result<Settings, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))?)
}
fn initialize(
    context: &AccountContext,
) -> Result<rusqlite::Connection, rich_studio::errors::StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection)
}

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
async fn logs_redact_known_and_structured_secrets_and_preserve_request_ids() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.bot_token = "1:ARTIFICIAL_BOT_SECRET".to_owned();
    settings.api_key = "api-key-sentinel".to_owned();
    settings.import_api_hash = "import-key-sentinel".to_owned();
    settings.deadman_url = "https://secret.invalid/secret-ping".to_owned();
    let metrics = Arc::new(Metrics::default());
    let logger = Logger::new(&settings, metrics.clone());
    logs::scope("request-fixture".to_owned(),async{
        logger.event("ERROR","rich-studio.test","https://telegram.invalid/bot1:ARTIFICIAL_BOT_SECRET/sendMessage https://telegram.invalid/bot1%3AARTIFICIAL_BOT_SECRET/sendMessage Bearer arbitrary-bearer-secret api-key-sentinel import-key-sentinel https://secret.invalid/secret-ping",json!({"payload":{"text":"PRIVATE_POST_SENTINEL"},"session":"SESSION_SENTINEL","event":"fixture"}));
    }).await;
    let lines = logger.read("WARNING", 50, None, "rich-studio.test", "request-fixture")?;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["request_id"], "request-fixture");
    let serialized = serde_json::to_string(&lines)?;
    for secret in [
        "ARTIFICIAL_BOT_SECRET",
        "arbitrary-bearer-secret",
        "api-key-sentinel",
        "import-key-sentinel",
        "secret-ping",
        "PRIVATE_POST_SENTINEL",
        "SESSION_SENTINEL",
    ] {
        assert!(!serialized.contains(secret), "leaked {secret}");
    }
    assert_eq!(metrics.snapshot()["log_problems"]["ERROR"], 1);
    Ok(())
}

#[tokio::test]
async fn observer_never_logs_body_or_query_and_tenant_health_hides_global_state() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = AppState::new(settings.clone());
    initialize(&state.account)?;
    let app = build_router(state.clone());
    let response=app.oneshot(Request::builder().method("POST").uri("/api/drafts?secret=QUERY_SECRET")
        .header("host","localhost").header("content-type","application/json").header("x-request-id","trace-fixture")
        .body(Body::from(json!({"title":"PRIVATE_POST_SENTINEL","post":{"rich_message":{"html":"<p>BODY_SECRET</p>"}}}).to_string()))?).await?;
    assert!(response.status().is_success());
    assert_eq!(response.headers()["x-request-id"], "trace-fixture");
    assert_eq!(response.headers()["x-robots-tag"], "noindex, nofollow");
    let _ = to_bytes(response.into_body(), 1_000_000).await?;
    let lines = state.logs.read("INFO", 100, None, "", "trace-fixture")?;
    assert!(!lines.is_empty());
    let text = serde_json::to_string(&lines)?;
    assert!(
        !text.contains("QUERY_SECRET")
            && !text.contains("BODY_SECRET")
            && !text.contains("PRIVATE_POST_SENTINEL")
    );
    let context = AccountContext::account(&settings, 77, Role::Publish);
    initialize(&context)?;
    let report = health::report(
        &settings,
        &context,
        &*state.spec.read().await,
        &state.logs,
        &state.metrics,
        &LoopStatus::default(),
    );
    assert_eq!(report["metrics"], json!({}));
    assert_eq!(report["recent_problems"], json!([]));
    assert!(
        report["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .all(|check| ["scheduler", "posts", "sends", "spec"]
                .contains(&check["name"].as_str().unwrap_or_default()))
    );
    Ok(())
}
