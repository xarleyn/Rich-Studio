use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
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
use serde_json::{Value, json};
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

type TestError = Box<dyn std::error::Error + Send + Sync>;

fn photo(name: &str) -> Value {
    json!({"rich_message":{"blocks":[{"type":"photo","photo":{"type":"photo","media":format!("upload:{name}")}}]}})
}

fn blocks_post() -> Value {
    json!({"rich_message":{"blocks":[
        {"type":"heading","size":1,"text":"Тест"},
        {"type":"paragraph","text":["Привет, ",{"type":"bold","text":"мир"}]}
    ]}})
}

fn telegram_settings(directory: &TempDir, base: &str) -> Result<Settings, TestError> {
    let mut settings = settings(directory)?;
    settings.api_base = base.to_owned();
    settings.bot_token = "1:x".to_owned();
    settings.default_chat = "@test_channel".to_owned();
    settings.preview_chat = "777".to_owned();
    Ok(settings)
}

/// A Bot API stand-in: any method succeeds, rich messages get a message id.
async fn telegram() -> Result<String, TestError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback(|request: axum::extract::Request| async move {
        let method = request
            .uri()
            .path()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let _ = to_bytes(request.into_body(), 1 << 20).await;
        let result = if method == "sendRichMessage" {
            json!({"message_id":101,"chat":{"id":1},"rich_message":{"blocks":[]}})
        } else {
            json!(true)
        };
        axum::Json(json!({"ok":true,"result":result}))
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

async fn json_request(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), TestError> {
    let builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
        None => builder.body(Body::empty())?,
    };
    let response = app.oneshot(request).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

async fn upload(app: axum::Router, name: &str, data: &[u8]) -> Result<Value, TestError> {
    let boundary = "rich-studio-operations-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .method("POST")
                .uri("/api/media")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{value}");
    Ok(value)
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

#[tokio::test]
async fn unused_media_is_reported_and_cleaned_up() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = AppState::new(settings);
    let _database = initialize(&state.account)?;
    let app = build_router(state);
    for name in ["draft.png", "sent.png", "old.png", "lonely.png"] {
        upload(app.clone(), name, b"\x89PNG").await?;
    }
    let (status, draft) = json_request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"d","post":photo("draft.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let (status, sent) = json_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":photo("sent.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let (status, old) = json_request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":photo("old.png")})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{old}");
    let id = old["post"]["id"].as_str().ok_or("post id")?;
    let (status, deleted) =
        json_request(app.clone(), "DELETE", &format!("/api/posts/{id}"), None).await?;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, listed) = json_request(app.clone(), "GET", "/api/media", None).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let used = listed
        .as_array()
        .ok_or("media list")?
        .iter()
        .map(|file| {
            (
                file["name"].as_str().unwrap_or_default().to_owned(),
                file["used"].as_bool().unwrap_or(true),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        used,
        BTreeMap::from([
            ("draft.png".to_owned(), true),
            ("sent.png".to_owned(), true),
            ("old.png".to_owned(), false),
            ("lonely.png".to_owned(), false),
        ])
    );
    let (status, cleaned) = json_request(app.clone(), "POST", "/api/media/cleanup", None).await?;
    assert_eq!(status, StatusCode::OK, "{cleaned}");
    let mut deleted = cleaned["deleted"]
        .as_array()
        .ok_or("deleted")?
        .iter()
        .map(|name| name.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    deleted.sort();
    assert_eq!(deleted, ["lonely.png", "old.png"]);
    let (_, listed) = json_request(app, "GET", "/api/media", None).await?;
    let mut names = listed
        .as_array()
        .ok_or("media list")?
        .iter()
        .map(|file| file["name"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["draft.png", "sent.png"]);
    Ok(())
}

#[tokio::test]
async fn media_of_a_missed_post_is_protected_without_force() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = AppState::new(settings);
    let _database = initialize(&state.account)?;
    let app = build_router(state.clone());
    upload(app.clone(), "later.png", b"\x89PNG").await?;
    let at = housekeeping::now() + 3600;
    let (status, scheduled) = json_request(
        app.clone(),
        "POST",
        "/api/schedule",
        Some(json!({"post":photo("later.png"),"chat_id":"@c","at":at})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{scheduled}");
    let id = scheduled["id"].as_str().ok_or("scheduled id")?;
    assert!(rich_studio::store::scheduler::claim(
        &state.account,
        id,
        i64::MAX
    )?);
    assert!(rich_studio::store::scheduler::missed(
        &state.account,
        id,
        "студия не работала"
    )?);
    let (status, conflict) =
        json_request(app.clone(), "DELETE", "/api/media/later.png", None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["detail"]["used_by"],
        json!(["пропущенный пост в @c"])
    );
    let (status, forced) =
        json_request(app, "DELETE", "/api/media/later.png?force=true", None).await?;
    assert_eq!(status, StatusCode::OK, "{forced}");
    Ok(())
}
