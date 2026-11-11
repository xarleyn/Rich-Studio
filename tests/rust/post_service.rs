use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    routing::post,
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    services::{
        drafts::{DraftService, SaveDraft},
        kinds, posts,
        publisher::Publisher,
        updates,
    },
    settings::Settings,
    store::{auth, channels, db, meta},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tower::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn settings(dir: &TempDir) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([
        ("DATA_DIR".into(), dir.path().to_string_lossy().into_owned()),
        ("SPEC_PIN".into(), "on".into()),
        ("TG_UPDATES".into(), "off".into()),
        ("TG_DEFAULT_CHAT".into(), "@public_channel".into()),
        ("TG_PREVIEW_CHAT".into(), "777".into()),
        ("TG_BOT_TOKEN".into(), "fixture-token".into()),
    ]))?)
}
fn post_data() -> Value {
    json!({"rich_message":{"blocks":[{"type":"paragraph","text":"Preview"}]}})
}
async fn bot(
    State(sender): State<mpsc::UnboundedSender<(String, String)>>,
    request: Request,
) -> Json<Value> {
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|header| header.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let _ = sender.send((content_type, String::from_utf8_lossy(&bytes).into_owned()));
    Json(
        json!({"ok":true,"result":{"message_id":101,"chat":{"id":777},"rich_message":{"blocks":[{"type":"photo","photo":[{"file_id":"photo-small","width":10,"height":10},{"file_id":"photo-large","width":100,"height":100}],"has_spoiler":true}]}}}),
    )
}
async fn mock() -> Result<
    (
        String,
        mpsc::UnboundedReceiver<(String, String)>,
        tokio::task::JoinHandle<()>,
    ),
    Box<dyn std::error::Error>,
> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/botfixture-token/sendRichMessage", post(bot))
        .with_state(sender);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), receiver, task))
}

#[tokio::test]
async fn preview_returns_editable_blocks_remembers_callbacks_and_uploads_comment() -> TestResult {
    let dir = TempDir::new()?;
    let mut cfg = settings(&dir)?;
    let (base, mut calls, server) = mock().await?;
    cfg.api_base = base;
    let state = AppState::new(cfg);
    db::migrate(&mut db::Store::connect(&state.account)?)?;
    std::fs::create_dir_all(&state.account.media_dir)?;
    std::fs::write(state.account.media_dir.join("comment.jpg"), b"fixture-jpeg")?;
    let mut post = post_data();
    post["callbacks"] = json!({"confirm":"Thanks"});
    post["comment"] =
        json!({"blocks":[{"type":"photo","photo":{"type":"photo","media":"upload:comment.jpg"}}]});
    let result = posts::from_state(&state)
        .await
        .preview(&post, &Value::Null)
        .await?;
    assert_eq!(result["chat_id"], 777);
    assert_eq!(
        result["parsed"]["input_blocks"][0]["photo"]["media"],
        "photo-large"
    );
    assert_eq!(
        result["parsed"]["input_blocks"][0]["photo"]["has_spoiler"],
        true
    );
    let first = calls.recv().await.ok_or("first request")?;
    let payload: Value = serde_json::from_str(&first.1)?;
    assert_eq!(payload["chat_id"], 777);
    assert!(payload.get("pin").is_none());
    let comment = calls.recv().await.ok_or("comment request")?;
    assert!(comment.0.contains("multipart/form-data"));
    assert!(comment.1.contains("attach://f0"));
    assert!(comment.1.contains("fixture-jpeg"));
    assert!(comment.1.contains("reply_parameters"));
    let raw = meta::get(&state.account, updates::PREVIEW_KEY)?.ok_or("callbacks")?;
    let callbacks: Value = serde_json::from_str(&raw)?;
    assert_eq!(callbacks["777:101"]["confirm"], "Thanks");
    let count: i64 =
        db::Store::connect(&state.account)?
            .query_row("select count(*) from posts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn mcp_personal_preview_strips_duplicate_blocks_and_uses_account_uid() -> TestResult {
    let dir = TempDir::new()?;
    let mut cfg = settings(&dir)?;
    let (base, mut calls, server) = mock().await?;
    cfg.api_base = base;
    let mut state = AppState::new(cfg);
    state.account = AccountContext::account(&state.settings, 555, Role::Drafts);
    let result = rich_studio::mcp_tools::tools::drafts::call(
        state,
        "send_preview_to_me",
        json!({"post":post_data()}),
    )
    .await
    .map_err(std::io::Error::other)?;
    assert_eq!(result["chat_id"], 555);
    assert!(result["parsed"].get("blocks").is_none());
    assert_eq!(
        result["parsed"]["input_blocks"][0]["photo"]["media"],
        "photo-large"
    );
    let (_, body) = calls.recv().await.ok_or("request")?;
    assert_eq!(serde_json::from_str::<Value>(&body)?["chat_id"], 555);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn missing_personal_preview_never_falls_back_to_public_default() -> TestResult {
    let dir = TempDir::new()?;
    let mut cfg = settings(&dir)?;
    cfg.preview_chat.clear();
    cfg.api_base = "http://127.0.0.1:1".into();
    let state = AppState::new(cfg);
    let result = posts::from_state(&state)
        .await
        .preview(&post_data(), &Value::Null)
        .await;
    assert!(matches!(
        result,
        Err(rich_studio::errors::ServiceError::BadRequest(_))
    ));
    Ok(())
}

#[tokio::test]
async fn empty_target_list_inherits_draft_channels_and_exposes_their_titles() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    channels::save(
        &state.account,
        &json!({"chat_id":"@chosen","chat_num":-100123i64,"title":"Chosen title","type":"channel"}),
    )?;
    let drafts = DraftService::new(state.account.clone());
    drafts.migrate()?;
    let draft = drafts.save(SaveDraft {
        id: None,
        title: "Draft".into(),
        post: post_data(),
        version: None,
        post_id: None,
        chat_ids: Some(vec!["@chosen".into()]),
    })?;
    let id = draft["id"].as_str().ok_or("id")?;
    let service = posts::from_state(&state).await;
    let result = service
        .send(
            &post_data(),
            &Value::Null,
            &json!([]),
            Some(id),
            "test",
            true,
            false,
        )
        .await?;
    assert_eq!(result["requests"][0]["params"]["chat_id"], "@chosen");
    assert_eq!(result["requests"][0]["chat_title"], "Chosen title");
    let result =
        rich_studio::mcp_tools::tools::drafts::call(state, "get_draft", json!({"draft_id":id}))
            .await
            .map_err(std::io::Error::other)?;
    assert_eq!(result["goes_to"][0]["title"], "Chosen title");
    Ok(())
}

#[tokio::test]
async fn schedule_audit_actor_comes_from_token_identity_and_sanitized_client() -> TestResult {
    let dir = TempDir::new()?;
    let cfg = settings(&dir)?;
    auth::add_token(&cfg, "laptop", "fixture-secret", "publish")?;
    let state = AppState::new(cfg);
    let app = rich_studio::build_router(state);
    for (client, expected) in [("ui", "ui@laptop"), ("claimed@someone", "laptop")] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/schedule")
                    .header("host", "localhost")
                    .header("authorization", "Bearer fixture-secret")
                    .header("x-studio-client", client)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"post":post_data(),"at":auth::now()+3600}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await?)?;
        assert_eq!(body["actor"], expected);
    }
    Ok(())
}

#[test]
fn account_draft_byte_and_count_quotas_return_413_and_preserve_saved_rows() -> TestResult {
    use rich_studio::{errors::StoreError, services::drafts::DraftError, store::drafts};
    let dir = TempDir::new()?;
    let cfg = settings(&dir)?;
    let context = AccountContext::account(&cfg, 123, Role::Drafts);
    db::migrate(&mut db::Store::connect(&context)?)?;
    let empty = json!({"rich_message":{"markdown":""}});
    let overhead =
        rich_studio::store::templates::encode(&empty)?.len() + "boundary".len() + "t".len();
    let room = 1024 * 1024 - overhead;
    let accepted = json!({"rich_message":{"markdown":"x".repeat(room)}});
    let record = drafts::save(&context, Some("boundary"), "t", &accepted, None, None, None)?
        .ok_or("record")?;
    let too_large = json!({"rich_message":{"markdown":"x".repeat(room+1)}});
    let error = drafts::save(
        &context,
        Some("boundary"),
        "t",
        &too_large,
        None,
        None,
        None,
    )
    .err()
    .ok_or("oversize accepted")?;
    assert!(matches!(&error, StoreError::Quota(_)));
    assert!(error.to_string().contains("1 МБ"));
    assert_eq!(
        rich_studio::api::drafts::error_response(DraftError::Store(error)).status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(drafts::get(&context, "boundary")?, Some(record));
    let connection = db::Store::connect(&context)?;
    connection.execute("with recursive seq(n) as (select 1 union all select n+1 from seq where n<999) insert into drafts(id,title,payload,created_at,updated_at) select 'seed-'||n,'','{}',0,0 from seq",[])?;
    let error = drafts::save(&context, None, "extra", &json!({}), None, None, None)
        .err()
        .ok_or("1001st accepted")?;
    assert!(matches!(&error, StoreError::Quota(_)));
    assert!(error.to_string().contains("Лимит черновиков"));
    assert_eq!(
        rich_studio::api::drafts::error_response(DraftError::Store(error)).status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(drafts::list(&context)?.len(), 1000);
    assert!(
        drafts::save(
            &context,
            Some("boundary"),
            "t",
            &json!({}),
            None,
            None,
            None
        )?
        .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn streamed_media_failure_and_cancellation_remove_partial_downloads() -> TestResult {
    use rich_studio::services::media::{MediaError, MediaService};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = TempDir::new()?;
    let cfg = settings(&dir)?;
    let context = AccountContext::root(&cfg);
    let service = MediaService::new(context.clone(), 0.00001, 1.0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut bytes = [0; 4096];
        let _ = socket.read(&mut bytes).await?;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n")
            .await?;
        socket.write_all(&[1; 100]).await?;
        Ok::<_, std::io::Error>(())
    });
    let error = service
        .fetch(
            &format!("http://{address}/file"),
            Some("large.jpg"),
            false,
            true,
        )
        .await
        .err()
        .ok_or("large media accepted")?;
    assert!(matches!(error, MediaError::TooLarge));
    server.await??;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let signal = std::sync::Arc::new(tokio::sync::Notify::new());
    let seen = signal.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut bytes = [0; 4096];
        let _ = socket.read(&mut bytes).await?;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc")
            .await?;
        seen.notify_one();
        std::future::pending::<()>().await;
        Ok::<_, std::io::Error>(())
    });
    let task = tokio::spawn(async move {
        service
            .fetch(
                &format!("http://{address}/file"),
                Some("partial.jpg"),
                false,
                true,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), signal.notified()).await?;
    task.abort();
    let _ = task.await;
    server.abort();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while std::fs::read_dir(&context.media_dir).is_ok_and(|entries| entries.count() > 0) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

#[test]
fn every_kind_sends_with_its_own_method() {
    let rich = kinds::get(None).unwrap_or_else(|error| panic!("rich kind: {error}"));
    let (method, _) =
        Publisher::request(&json!(-1), &json!({"rich_message":{"markdown":"x"}}), rich)
            .unwrap_or_else(|error| panic!("rich request: {error}"));
    assert_eq!(method, "sendRichMessage");
    for kind in kinds::KINDS.iter().filter(|kind| kind.key.is_some()) {
        let key = kind.key.unwrap_or_default();
        let (method, params) =
            Publisher::request(&json!(-1), &json!({key:{"a":1},"pin":true}), kind)
                .unwrap_or_else(|error| panic!("{} request: {error}", kind.name));
        assert_eq!(method, kind.method);
        assert_eq!(params, json!({"a":1,"chat_id":-1}));
    }
}
