use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    logs::{self, Logger},
    metrics::Metrics,
    services::{
        backup, health, housekeeping, in_flight, loops::LoopStatus, media::MediaService, updates,
    },
    settings::Settings,
    store::{
        db::{self, Store},
        editing as editing_store, idempotency, posts as posts_store,
    },
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
        logger.event("ERROR","rich-studio.test","boom https://telegram.invalid/bot1:ARTIFICIAL_BOT_SECRET/sendMessage https://telegram.invalid/bot1%3AARTIFICIAL_BOT_SECRET/sendMessage Bearer arbitrary-bearer-secret api-key-sentinel import-key-sentinel https://secret.invalid/secret-ping",json!({"payload":{"text":"PRIVATE_POST_SENTINEL"},"session":"SESSION_SENTINEL","event":"fixture"}));
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
    assert!(serialized.contains("<secret>") && serialized.contains("boom"));
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
    let updates = state.updates_status.read().await.clone();
    let report = health::report(
        &settings,
        &context,
        &state.spec.read().await.clone(),
        &state.logs,
        &state.metrics,
        &LoopStatus::default(),
        &updates,
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

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == name))
        .unwrap_or_else(|| panic!("check {name} is missing: {report}"))
}

fn initialized(settings: Settings) -> Result<AppState, TestError> {
    let state = AppState::new(settings);
    initialize(&state.account)?;
    Ok(state)
}

async fn report(state: &AppState) -> Value {
    report_with(state, &state.metrics).await
}

async fn report_with(state: &AppState, metrics: &Metrics) -> Value {
    let spec = state.spec.read().await.clone();
    let loops = state.loop_status.read().await.clone();
    let updates = state.updates_status.read().await.clone();
    health::report(
        &state.settings,
        &state.account,
        &spec,
        &state.logs,
        metrics,
        &loops,
        &updates,
    )
}

async fn request_with_headers(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, Value, HeaderMap), TestError> {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
        None => builder.body(Body::empty())?,
    };
    let response = app.oneshot(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    ))
}

async fn telegram_error(status: u16, body: Value) -> Result<String, TestError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback(move |_request: axum::extract::Request| {
        let body = body.clone();
        async move {
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                axum::Json(body),
            )
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

async fn mcp_json(app: axum::Router, tool: &str, arguments: Value) -> Result<Value, TestError> {
    let (status, body, _) = request_with_headers(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        &[
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-06-18"),
        ],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = body["result"]["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block["text"].as_str())
        .ok_or_else(|| format!("tool result text: {body}"))?;
    Ok(serde_json::from_str(text)?)
}

async fn mcp_lines(
    app: axum::Router,
    tool: &str,
    arguments: Value,
) -> Result<Vec<Value>, TestError> {
    let (status, body, _) = request_with_headers(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}})),
        &[
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-06-18"),
        ],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["result"]["content"]
        .as_array()
        .ok_or_else(|| format!("tool result content: {body}"))?
        .iter()
        .map(|block| {
            serde_json::from_str::<Value>(block["text"].as_str().unwrap_or_default())
                .map_err(TestError::from)
        })
        .collect()
}

#[tokio::test]
async fn api_and_telegram_calls_are_logged_without_post_content() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, sent, _) = request_with_headers(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[("x-studio-client", "mcp:test")],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let (status, _, _) = request_with_headers(app, "GET", "/api/health", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    state.logs.flush()?;
    let lines = state.logs.read("INFO", 1000, None, "", "")?;
    let http = lines
        .iter()
        .filter(|line| line["event"] == "http")
        .collect::<Vec<_>>();
    assert_eq!(http.len(), 1, "{lines:?}");
    assert_eq!(http[0]["method"], "POST");
    assert_eq!(http[0]["path"], "/api/send");
    assert_eq!(http[0]["status"], 200);
    assert_eq!(http[0]["client"], "mcp:test");
    let call = lines
        .iter()
        .find(|line| line["event"] == "telegram.call")
        .ok_or("telegram.call line")?;
    assert_eq!(call["method"], "sendRichMessage");
    assert_eq!(call["chat_id"], "@test_channel");
    assert!(call["ms"].as_u64().is_some(), "{call}");
    let text = serde_json::to_string(&lines)?;
    assert!(!text.contains("rich_message"));
    assert!(!text.contains("Привет"));
    assert_eq!(
        state.metrics.snapshot()["telegram_calls"]["sendRichMessage ok"],
        1
    );
    assert!(state.metrics.snapshot()["requests"]["2xx"].as_u64() >= Some(2));
    Ok(())
}

#[test]
fn log_rotation_keeps_the_cap_and_logging_can_be_turned_off() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.log_max_mb = 0.01;
    settings.log_files = 2;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    for index in 0..1000 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let logs = directory.path().join("logs");
    let mut names = std::fs::read_dir(&logs)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    assert_eq!(names, ["studio.log", "studio.log.1", "studio.log.2"]);
    let cap = (3.0 * 0.01 * 1024.0 * 1024.0) as u64;
    assert!(logger.disk_bytes() <= cap + 1024, "{}", logger.disk_bytes());
    let newest = logger.read("INFO", 1, None, "", "")?;
    assert!(
        newest[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 999")
    );
    settings.log_max_mb = 0.0;
    let silent = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(silent.current_file().is_none());
    assert!(silent.read("INFO", 10, None, "", "")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn logs_endpoint_filters_by_level_logger_text_and_time() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    state
        .logs
        .event("INFO", "rich-studio.scheduler", "tick", json!({}));
    state.logs.event(
        "ERROR",
        "rich-studio.scheduler",
        "scheduler broke",
        json!({}),
    );
    state
        .logs
        .event("WARNING", "rich-studio.updates", "late click", json!({}));
    let (status, warning, _) =
        request_with_headers(app.clone(), "GET", "/api/logs?level=WARNING", None, &[]).await?;
    assert_eq!(status, StatusCode::OK, "{warning}");
    assert!(
        warning["file"]
            .as_str()
            .unwrap_or_default()
            .ends_with("studio.log")
    );
    assert_eq!(
        warning["lines"]
            .as_array()
            .ok_or("lines")?
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["scheduler broke", "late click"]
    );
    let (_, by_logger, _) = request_with_headers(
        app.clone(),
        "GET",
        "/api/logs?logger=rich-studio.scheduler",
        None,
        &[],
    )
    .await?;
    assert_eq!(
        by_logger["lines"]
            .as_array()
            .ok_or("lines")?
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["tick", "scheduler broke"]
    );
    let (_, by_text, _) =
        request_with_headers(app.clone(), "GET", "/api/logs?q=LATE", None, &[]).await?;
    assert_eq!(by_text["lines"][0]["msg"], "late click");
    let future = housekeeping::now() + 60;
    let (_, empty, _) = request_with_headers(
        app.clone(),
        "GET",
        &format!("/api/logs?since={future}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(empty["lines"], json!([]));
    let (status, _, _) =
        request_with_headers(app, "GET", "/api/logs?level=LOUD", None, &[]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn request_id_ties_a_request_together_and_every_request_gets_a_new_one() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, sent, headers) = request_with_headers(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let rid = headers["x-request-id"].to_str()?.to_owned();
    assert!(
        rid.len() == 32
            && rid
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{rid}"
    );
    let (_, logged, _) =
        request_with_headers(app.clone(), "GET", &format!("/api/logs?q={rid}"), None, &[]).await?;
    let lines = logged["lines"].as_array().ok_or("lines")?;
    let events = lines
        .iter()
        .map(|line| line["event"].as_str().unwrap_or_default())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(events, ["telegram.call", "http"].into_iter().collect());
    assert!(lines.iter().all(|line| line["request_id"] == rid));
    let (_, _, next) = request_with_headers(app, "GET", "/api/drafts", None, &[]).await?;
    assert_ne!(next["x-request-id"], rid.as_str());
    Ok(())
}

#[test]
fn incoming_request_ids_follow_the_caller_and_w3c_traceparent() {
    assert_eq!(logs::incoming_id(Some("agent-42"), None), "agent-42");
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let parent = "00f067aa0ba902b7";
    assert_eq!(
        logs::incoming_id(None, Some(&format!("00-{trace}-{parent}-01"))),
        trace
    );
    let bad = logs::incoming_id(Some(&format!("{}\n", "x".repeat(65))), None);
    assert!(
        bad.len() == 32 && bad.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{bad}"
    );
    let zero = "0".repeat(32);
    assert_ne!(
        logs::incoming_id(None, Some(&format!("00-{zero}-{parent}-01"))),
        zero
    );
    let no_parent = format!("00-{trace}-{}-01", "0".repeat(16));
    assert_ne!(logs::incoming_id(None, Some(&no_parent)), trace);
    let future = format!("cc-{trace}-{parent}-01-what-the-future-will-be");
    assert_eq!(logs::incoming_id(None, Some(&future)), trace);
    let extra = format!("00-{trace}-{parent}-01-extra");
    assert_ne!(logs::incoming_id(None, Some(&extra)), trace);
    let uppercase = format!("00-{}-{}-01", trace.to_uppercase(), parent.to_uppercase());
    assert_ne!(
        logs::incoming_id(None, Some(&uppercase)),
        trace.to_uppercase()
    );
}

#[tokio::test]
async fn a_scheduler_round_logs_its_calls_under_its_own_request_id() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let mut settings = telegram_settings(&directory, &base)?;
    settings.background = true;
    settings.updates_on = false;
    settings.backup_keep = 0;
    let state = initialized(settings)?;
    let id = posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@test_channel"),
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(housekeeping::now() - 5),
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?["id"]
        .as_str()
        .ok_or("post id")?
        .to_owned();
    let loops = rich_studio::services::loops::Loops::start(state.clone());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if posts_store::get(&state.account, &id)?
                .as_ref()
                .is_some_and(|post| post["status"] == "sent")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, TestError>(())
    })
    .await??;
    loops.request_stop();
    tokio::time::timeout(Duration::from_secs(10), loops.stop()).await??;
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.telegram", "")?;
    let call = lines
        .iter()
        .find(|line| line["event"] == "telegram.call" && line["method"] == "sendRichMessage")
        .ok_or("scheduler telegram call")?;
    assert!(
        call["request_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("scheduler-"),
        "{call}"
    );
    assert_eq!(
        posts_store::get(&state.account, &id)?.ok_or("sent post")?["status"],
        "sent"
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_incoming_update_is_logged_under_its_update_id() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    Store::connect(&state.account)?.execute("drop table suggested_posts", [])?;
    updates::receive_webhook(
        &state,
        &json!({"update_id":7001,"message":{"chat":{"id":-1001},"message_id":1,
            "suggested_post_info":{"state":"pending","send_date":0,"price":{"amount":1,"currency":"XTR"}}}}),
    )
    .await?;
    let lines = state
        .logs
        .read("ERROR", 100, None, "rich-studio.updates", "")?;
    let failure = lines
        .iter()
        .find(|line| line["event"] == "update.failed")
        .ok_or_else(|| format!("update.failed line: {lines:?}"))?;
    assert_eq!(failure["request_id"], "update-7001");
    assert!(
        failure["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("suggested_posts"),
        "{failure}"
    );
    Ok(())
}

#[tokio::test]
async fn problems_in_the_report_point_to_their_request() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram_error(500, json!({"ok":false,"description":"upstream broke"})).await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, failed, headers) = request_with_headers(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
        &[("x-request-id", "trace-me")],
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{failed}");
    assert_eq!(headers["x-request-id"], "trace-me");
    let report = report(&state).await;
    let problem = report["recent_problems"]
        .as_array()
        .ok_or("recent_problems")?
        .iter()
        .find(|problem| {
            problem["message"]
                .as_str()
                .unwrap_or_default()
                .contains("upstream broke")
        })
        .ok_or_else(|| format!("recent problem: {report}"))?;
    assert_eq!(problem["request_id"], "trace-me");
    Ok(())
}

#[tokio::test]
async fn latency_is_kept_per_endpoint_template() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let mut post_id = String::new();
    for _ in 0..3 {
        let (status, sent, _) = request_with_headers(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":blocks_post()})),
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{sent}");
        post_id = sent["post"]["id"].as_str().ok_or("post id")?.to_owned();
    }
    let (status, _, _) = request_with_headers(
        app,
        "GET",
        &format!("/api/posts/{post_id}/versions"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let report = report(&state).await;
    let latency = &report["metrics"]["http_latency"];
    assert_eq!(latency["POST /api/send"]["n"], 3);
    assert_eq!(latency["GET /api/posts/{id}/versions"]["n"], 1, "{latency}");
    assert!(
        latency
            .as_object()
            .ok_or("http_latency")?
            .keys()
            .all(|key| !key.contains(&post_id))
    );
    let telegram = &report["metrics"]["telegram_latency"]["sendRichMessage"];
    assert_eq!(telegram["n"], 3);
    assert!(telegram["p50_ms"].as_u64() <= telegram["p95_ms"].as_u64());
    assert!(telegram["p95_ms"].as_u64() <= telegram["max_ms"].as_u64());
    Ok(())
}

#[tokio::test]
async fn readiness_is_public_and_reports_only_failing_check_names() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.api_key = "secret".to_owned();
    let state = initialized(settings.clone())?;
    let app = build_router(state.clone());
    let (status, body, _) = request_with_headers(app.clone(), "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ready":true,"failing":[]}));
    let (status, _, _) = request_with_headers(app.clone(), "GET", "/api/health", None, &[]).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    std::fs::remove_file(state.account.database_path())?;
    let (status, cached, _) = request_with_headers(app, "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cached, json!({"ready":true,"failing":[]}));
    let missing = build_router(AppState::new(settings));
    let (status, body, _) = request_with_headers(missing, "GET", "/readyz", None, &[]).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ready"], false);
    assert!(
        body["failing"]
            .as_array()
            .ok_or("failing")?
            .contains(&json!("database"))
    );
    Ok(())
}

#[test]
fn timestamps_are_utc_and_since_filters_by_them() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    logger.event("INFO", "rich-studio.test", "now", json!({}));
    let line = logger
        .read("INFO", 100, None, "", "")?
        .pop()
        .ok_or("line")?;
    let timestamp = line["ts"].as_str().ok_or("ts")?;
    assert_eq!(timestamp.len(), 24, "{timestamp}");
    assert!(timestamp.ends_with('Z'));
    let now = time::OffsetDateTime::now_utc();
    let prefix = format!(
        "{}-{:02}-{:02}T{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    );
    assert!(timestamp.starts_with(&prefix), "{timestamp} {prefix}");
    let recent = logger.read(
        "INFO",
        100,
        Some(now.unix_timestamp() as f64 - 60.0),
        "",
        "",
    )?;
    assert_eq!(recent.last().ok_or("line")?["msg"], "now");
    let future = logger.read(
        "INFO",
        100,
        Some(now.unix_timestamp() as f64 + 3600.0),
        "",
        "",
    )?;
    assert!(future.is_empty());
    Ok(())
}

#[tokio::test]
async fn health_of_a_fresh_studio_is_ok_where_it_matters() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let report = report(&state).await;
    assert_eq!(
        report["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .map(|check| check["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        [
            "database",
            "scheduler",
            "posts",
            "sends",
            "spec",
            "editor",
            "telegram",
            "loops",
            "disk",
            "backup",
            "storage",
            "log",
        ]
    );
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        report["bot_api"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Bot API")
    );
    assert!(report["time"].as_str().unwrap_or_default().ends_with('Z'));
    for check in report["checks"].as_array().ok_or("checks")? {
        assert_eq!(check["status"], "ok", "{check}");
    }
    assert_eq!(report["status"], "ok");
    assert_eq!(report["problems"], json!([]));
    assert_eq!(
        report["metrics"]
            .as_object()
            .ok_or("metrics")?
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "http_latency",
            "log_problems",
            "requests",
            "telegram_calls",
            "telegram_latency",
            "uptime_s",
        ]
        .into_iter()
        .collect()
    );
    Ok(())
}

#[tokio::test]
async fn health_names_what_needs_a_person_and_what_to_do() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let now = housekeeping::now();
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "failed",
            message_id: None,
            scheduled_at: None,
            error: Some("Telegram: chat not found"),
            kind: "rich",
            actor: "api",
        },
    )?;
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(now - 600),
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?;
    idempotency::claim(&state.account, "k1", "send", "fp")?;
    idempotency::finish(&state.account, "k1", "uncertain", &json!("пост мог уйти"))?;
    state.logs.event(
        "ERROR",
        "rich-studio.scheduler",
        "scheduler iteration failed",
        json!({"event":"scheduler.error"}),
    );
    state.metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Bad Gateway"),
        1,
        None,
        false,
    );
    let checked = report(&state).await;
    assert_eq!(checked["status"], "warn");
    let posts = check(&checked, "posts");
    assert_eq!(posts["status"], "warn");
    assert_eq!(
        posts["details"]["posts"][0]["error"],
        "Telegram: chat not found"
    );
    let scheduler = check(&checked, "scheduler");
    assert_eq!(scheduler["status"], "warn");
    assert!(
        scheduler["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("выключены"),
        "{scheduler}"
    );
    let sends = check(&checked, "sends");
    assert_eq!(sends["status"], "warn");
    assert!(
        sends["action"]
            .as_str()
            .unwrap_or_default()
            .contains("Idempotency-Key")
    );
    assert!(
        check(&checked, "log")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("scheduler iteration failed")
    );
    assert!(
        check(&checked, "telegram")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("Bad Gateway")
    );
    for check in checked["checks"].as_array().ok_or("checks")? {
        if check["status"] != "ok" {
            assert!(
                check["action"]
                    .as_str()
                    .is_some_and(|action| !action.is_empty()),
                "{check}"
            );
        }
    }
    state
        .metrics
        .telegram_call("getMe", true, None, 1, None, false);
    assert_eq!(check(&report(&state).await, "telegram")["status"], "ok");
    Ok(())
}

#[tokio::test]
async fn a_conflicting_bot_is_a_failure() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    settings.background = true;
    settings.updates_on = true;
    let state = initialized(settings)?;
    {
        let mut status = state.updates_status.write().await;
        status.state = "conflict".to_owned();
        status.error = Some("Conflict: terminated by other getUpdates request".to_owned());
    }
    let report = report(&state).await;
    assert_eq!(report["status"], "fail");
    let telegram = check(&report, "telegram");
    assert_eq!(telegram["status"], "fail");
    assert!(
        telegram["action"]
            .as_str()
            .unwrap_or_default()
            .contains("TG_UPDATES=off")
    );
    assert!(
        telegram["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("terminated by other getUpdates")
    );
    Ok(())
}

#[tokio::test]
async fn unused_uploads_are_counted_and_reported() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    upload(app, "big.png", &vec![b'x'; 2048]).await?;
    let big = 201 * 1024 * 1024;
    std::fs::OpenOptions::new()
        .write(true)
        .open(state.account.media_dir.join("big.png"))?
        .set_len(big)?;
    let report = report(&state).await;
    let storage = check(&report, "storage");
    assert_eq!(storage["status"], "warn");
    assert!(
        storage["action"]
            .as_str()
            .unwrap_or_default()
            .contains("MEDIA_UNUSED_DAYS")
    );
    assert_eq!(
        storage["details"]["media"],
        json!({"files":1,"bytes":big,"unused_files":1,"unused_bytes":big})
    );
    Ok(())
}

#[tokio::test]
async fn health_changes_are_logged_once() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let mut previous = BTreeMap::new();
    housekeeping::watch(&report(&state).await, &mut previous, &state.logs);
    posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "failed",
            message_id: None,
            scheduled_at: None,
            error: Some("boom"),
            kind: "rich",
            actor: "api",
        },
    )?;
    let changed = report(&state).await;
    housekeeping::watch(&changed, &mut previous, &state.logs);
    housekeeping::watch(&changed, &mut previous, &state.logs);
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.health", "")?;
    let changes = lines
        .iter()
        .filter(|line| line["event"] == "health.change" && line["check"] == "posts")
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["status"], "warn");
    assert!(
        changes[0]["action"]
            .as_str()
            .is_some_and(|action| !action.is_empty())
    );
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

#[tokio::test]
async fn mcp_health_and_logs_match_http() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    state
        .logs
        .event("WARNING", "rich-studio.updates", "late click", json!({}));
    let app = build_router(state.clone());
    let health = mcp_json(app.clone(), "studio_health", json!({})).await?;
    assert!(
        health["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .any(|check| check["name"] == "database")
    );
    let lines = mcp_lines(app, "studio_logs", json!({"level":"WARNING","limit":50})).await?;
    assert_eq!(
        lines
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["late click"]
    );
    Ok(())
}

#[tokio::test]
async fn token_echoed_by_a_proxy_page_never_reaches_health_or_log() -> TestResult {
    const TOKEN: &str = "1234567890:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = axum::Router::new().fallback({
        let counter = counter.clone();
        move |request: axum::extract::Request| {
            let counter = counter.clone();
            async move {
                let pad = if counter.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
                    0
                } else {
                    248
                };
                let path = request.uri().path().to_owned();
                (
                    StatusCode::NOT_FOUND,
                    axum::response::Html(format!(
                        "<pre>Cannot POST {}{path}</pre>",
                        "/".repeat(pad)
                    )),
                )
            }
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let directory = TempDir::new()?;
    let mut settings = telegram_settings(&directory, &format!("http://{address}"))?;
    settings.bot_token = TOKEN.to_owned();
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    for _ in 0..2 {
        let (status, _, _) =
            request_with_headers(app.clone(), "POST", "/api/raw/getMe", Some(json!({})), &[])
                .await?;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let (_, report, _) =
            request_with_headers(app.clone(), "GET", "/api/health", None, &[]).await?;
        assert!(!report.to_string().contains("AAHxxxx"));
    }
    state.logs.flush()?;
    let log = std::fs::read_to_string(state.logs.current_file().ok_or("log file")?)?;
    assert!(!log.contains("AAHxxxx"), "{log}");
    Ok(())
}

#[test]
fn a_long_run_of_digits_does_not_stall_the_scrubber() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.bot_token = "1234567890:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_owned();
    let started = Instant::now();
    let digits = logs::scrub(&settings, &"1".repeat(60000));
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(digits.len(), 60000);
    assert_eq!(
        logs::scrub(&settings, &format!("/bot{}/getMe", settings.bot_token)),
        "/bot<secret>/getMe"
    );
    Ok(())
}

#[tokio::test]
async fn access_log_path_is_short_and_printable() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let path = format!("/x%1B[31mred{}", "y".repeat(500));
    let _ = request_with_headers(app, "GET", &path, None, &[]).await?;
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.http", "")?;
    let http = lines
        .iter()
        .find(|line| line["event"] == "http")
        .ok_or("access line")?;
    let logged = http["path"].as_str().ok_or("path")?;
    assert!(!logged.contains('\u{1b}'), "{logged}");
    assert!(logged.len() <= 210, "{}", logged.len());
    assert!(logged.starts_with("/x%1B"), "{logged}");
    Ok(())
}

#[tokio::test]
async fn unknown_methods_and_raw_telegram_methods_do_not_grow_metrics() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    for index in 0..5 {
        let _ = request_with_headers(app.clone(), &format!("M{index}"), "/api/drafts", None, &[])
            .await?;
    }
    let latency = state.metrics.snapshot()["http_latency"].clone();
    assert!(
        latency
            .as_object()
            .ok_or("http_latency")?
            .keys()
            .all(|key| !key.starts_with('M')),
        "{latency}"
    );
    for index in 0..120 {
        state
            .metrics
            .telegram_call(&format!("method{index}"), true, None, 1, None, false);
    }
    let telegram = state.metrics.snapshot()["telegram_latency"].clone();
    assert!(
        telegram.as_object().ok_or("telegram_latency")?.len() <= 101,
        "{telegram}"
    );
    assert!(telegram.get("other").is_some(), "{telegram}");
    Ok(())
}

#[test]
fn zero_log_files_still_rotate_and_stray_copies_are_ignored() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    std::fs::create_dir_all(settings.data_dir.join("logs"))?;
    std::fs::write(settings.data_dir.join("logs/studio.log.old"), "not ours")?;
    std::fs::write(settings.data_dir.join("logs/studio.log.7"), "{}\n")?;
    settings.log_max_mb = 0.01;
    settings.log_files = 0;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(!settings.data_dir.join("logs/studio.log.7").exists());
    assert!(settings.data_dir.join("logs/studio.log.old").exists());
    for index in 0..1000 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let cap = (2.0 * 0.01 * 1024.0 * 1024.0) as u64;
    assert!(logger.disk_bytes() <= cap + 1024, "{}", logger.disk_bytes());
    assert!(
        logger.read("INFO", 1, None, "", "")?[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 999")
    );
    Ok(())
}

#[test]
fn a_rotated_away_copy_is_skipped_while_reading_and_counting() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.log_max_mb = 0.01;
    settings.log_files = 1;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    for index in 0..500 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let current = logger.current_file().ok_or("log file")?.to_path_buf();
    let rotated = current.with_file_name("studio.log.1");
    assert!(rotated.exists());
    std::fs::remove_file(&rotated)?;
    assert_eq!(logger.disk_bytes(), std::fs::metadata(&current)?.len());
    assert!(
        logger.read("INFO", 1, None, "", "")?[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 499")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn log_file_is_readable_only_by_its_owner() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    logger.event("INFO", "rich-studio.test", "x", json!({}));
    logger.flush()?;
    let mode = std::fs::metadata(logger.current_file().ok_or("log file")?)?
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    Ok(())
}

#[tokio::test]
async fn since_out_of_range_is_refused_not_a_server_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings.clone())?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(
        logger
            .read("INFO", 10, Some(f64::INFINITY), "", "")?
            .is_empty()
    );
    let app = build_router(state);
    let (status, _, _) =
        request_with_headers(app, "GET", "/api/logs?since=1e300", None, &[]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn raw_chat_id_that_is_not_an_id_stays_out_of_the_log() -> TestResult {
    let directory = TempDir::new()?;
    let base = telegram().await?;
    let settings = telegram_settings(&directory, &base)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let (status, _, _) = request_with_headers(
        app,
        "POST",
        "/api/raw/sendMessage",
        Some(json!({"chat_id":{"x":"MARKERPOSTTEXT"},"text":"t"})),
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    state.logs.flush()?;
    let text = std::fs::read_to_string(state.logs.current_file().ok_or("log file")?)?;
    assert!(!text.contains("MARKERPOSTTEXT"), "{text}");
    let call = state
        .logs
        .read("INFO", 100, None, "rich-studio.telegram", "")?
        .into_iter()
        .find(|line| line["event"] == "telegram.call")
        .ok_or("telegram.call")?;
    assert_eq!(call["chat_id"], "?");
    Ok(())
}

#[tokio::test]
async fn unhandled_error_keeps_its_request_id() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    Store::connect(&state.account)?.execute("drop table drafts", [])?;
    let app = build_router(state.clone());
    let (status, _, headers) = request_with_headers(
        app,
        "GET",
        "/api/drafts",
        None,
        &[("x-request-id", "boom-1")],
    )
    .await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers["x-request-id"], "boom-1");
    let lines = state
        .logs
        .read("WARNING", 100, None, "rich-studio.http", "")?;
    let access = lines
        .iter()
        .find(|line| line["event"] == "http" && line["status"] == 500)
        .ok_or_else(|| format!("access line: {lines:?}"))?;
    assert_eq!(access["request_id"], "boom-1");
    Ok(())
}

#[tokio::test]
async fn watchers_own_error_line_does_not_count_as_a_log_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    state.logs.event(
        "ERROR",
        "rich-studio.health",
        "x: ok → fail",
        json!({"event":"health.change"}),
    );
    assert_eq!(check(&report(&state).await, "log")["status"], "ok");
    state
        .logs
        .event("ERROR", "rich-studio.test", "real", json!({}));
    for _ in 0..40 {
        state
            .logs
            .event("WARNING", "rich-studio.test", "noise", json!({}));
    }
    let report = report(&state).await;
    assert_eq!(check(&report, "log")["status"], "warn");
    assert!(
        check(&report, "log")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("real")
    );
    Ok(())
}

#[tokio::test]
async fn a_post_being_sent_right_now_is_not_a_problem() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let id = posts_store::add(
        &state.account,
        posts_store::NewPost {
            chat_id: &json!("@c"),
            payload: &blocks_post(),
            draft_id: None,
            status: "scheduled",
            message_id: None,
            scheduled_at: Some(housekeeping::now() - 5),
            error: None,
            kind: "rich",
            actor: "api",
        },
    )?["id"]
        .as_str()
        .ok_or("post id")?
        .to_owned();
    let guard =
        in_flight::claim(&state.account, &id, housekeeping::now())?.ok_or("in-flight claim")?;
    assert_eq!(check(&report(&state).await, "posts")["status"], "ok");
    drop(guard);
    let flagged = report(&state).await;
    let posts = check(&flagged, "posts");
    assert_eq!(posts["status"], "warn");
    assert_eq!(posts["details"]["posts"][0]["id"], id.as_str());
    posts_store::mark_failed(&state.account, &id, "test")?;
    Ok(())
}

#[tokio::test]
async fn telegram_check_ignores_a_rejected_post_but_not_a_setup_problem() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    for (refusal, expected) in [
        (
            "Bad Request: poll members_only is available only in channels",
            "ok",
        ),
        (
            "Bad Request: query is too old and response timeout expired",
            "ok",
        ),
        (
            "Bad Request: can't parse entities: unsupported start tag",
            "ok",
        ),
        ("Bad Request: chat not found", "warn"),
        ("Bad Request: not enough rights to send messages", "warn"),
        (
            "Bad Request: group chat was upgraded to a supergroup chat",
            "warn",
        ),
        ("Bad Request: message thread not found", "warn"),
        ("Bad Request: TOPIC_CLOSED", "warn"),
        ("Bad Request: PEER_ID_INVALID", "warn"),
        ("Bad Request: chat_id is empty", "warn"),
        ("Bad Request: CHAT_WRITE_FORBIDDEN", "warn"),
    ] {
        let metrics = Metrics::default();
        metrics.telegram_call("sendRichMessage", false, Some(refusal), 1, Some(400), false);
        let report = report_with(&state, &metrics).await;
        assert_eq!(check(&report, "telegram")["status"], expected, "{refusal}");
    }
    Ok(())
}

#[tokio::test]
async fn a_refused_post_or_long_poll_does_not_hide_a_setup_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = telegram_settings(&directory, "http://127.0.0.1:9")?;
    let state = initialized(settings)?;
    let metrics = Metrics::default();
    metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Forbidden: bot was kicked from the channel chat"),
        1,
        Some(403),
        false,
    );
    metrics.telegram_call(
        "sendRichMessage",
        false,
        Some("Bad Request: members_only is available only in channels"),
        1,
        Some(400),
        false,
    );
    let report = report_with(&state, &metrics).await;
    assert!(
        check(&report, "telegram")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("kicked")
    );
    metrics.telegram_call("getUpdates", true, None, 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "warn"
    );
    metrics.telegram_call("sendRichMessage", true, None, 5, None, false);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "ok"
    );
    metrics.telegram_call("getUpdates", false, Some("ConnectError"), 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "warn"
    );
    metrics.telegram_call("getUpdates", true, None, 1, None, true);
    assert_eq!(
        check(&report_with(&state, &metrics).await, "telegram")["status"],
        "ok"
    );
    Ok(())
}

#[test]
fn monitor_agent_is_read_only() -> TestResult {
    let agent = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/agents/studio-monitor.md"),
    )?;
    let front = agent.split("---").nth(1).ok_or("front matter")?;
    assert!(front.lines().any(|line| line == "name: studio-monitor"));
    let tools = front
        .lines()
        .find_map(|line| line.strip_prefix("tools: "))
        .ok_or("tools")?;
    let tools = tools
        .split(',')
        .map(str::trim)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        !tools.contains("Edit") && !tools.contains("Write"),
        "{tools:?}"
    );
    Ok(())
}

#[test]
fn session_health_hook_is_registered_next_to_the_format_hook() -> TestResult {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(
        root.join(".claude/settings.json"),
    )?)?;
    let start = settings["hooks"]["SessionStart"]
        .as_array()
        .ok_or("SessionStart")?
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(start.contains("studio_health.py"), "{start}");
    assert!(start.contains("session-start.sh"), "{start}");
    assert!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_default()
            .contains("format.py")
    );
    Ok(())
}

#[test]
fn monitor_skill_explains_every_health_check() {
    let skill = include_str!("../../.claude/skills/studio-health/SKILL.md");
    let explained = skill
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|line| line.split_once("` |").map(|(name, _)| name.to_owned()))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        "database",
        "editor",
        "telegram",
        "loops",
        "scheduler",
        "posts",
        "sends",
        "spec",
        "backup",
        "storage",
        "log",
        "disk",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(explained, expected);
}
