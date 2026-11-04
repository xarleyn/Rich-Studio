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
    services::{
        accounts,
        invite_links::{self, InviteLinks},
        posts::PostService,
        suggested_posts::{self, SuggestedPosts},
    },
    settings::Settings,
    spec::source::Spec,
    store::{auth, channels, invite_links as link_store, suggested_posts as suggestions},
    telegram::client::TelegramTransport,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};
use tempfile::TempDir;
use tower::ServiceExt as _;

#[derive(Clone, Default)]
struct Mock {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    mode: Arc<AtomicU8>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Mock {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(|e| panic!("lock {e}"))
            .clone()
    }
    fn count(&self, name: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(method, _)| method == name)
            .count()
    }
}
async fn mock() -> Result<(String, Mock, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let mock = Mock::default();
    let app=axum::Router::new().route("/bot1:fixture/{method}",axum::routing::post(async|State(mock):State<Mock>,Path(method):Path<String>,Json(body):Json<Value>|{
        mock.calls.lock().unwrap_or_else(|e|panic!("lock {e}")).push((method.clone(),body.clone()));
        if ["approveSuggestedPost","declineSuggestedPost"].contains(&method.as_str()){
            let mode=mock.mode.load(Ordering::SeqCst);
            if [3,4].contains(&mode){mock.entered.notify_one();mock.release.notified().await;}
            if [1,4].contains(&mode){return(StatusCode::BAD_REQUEST,Json(json!({"ok":false,"error_code":400,"description":"not enough rights"})))}
            if mode==2{return(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"error_code":502,"description":"lost acknowledgement"})))}
        }
        let result=if method=="getChatMember"{json!({"status":if body["user_id"]==22{"member"}else{"creator"}})}else if method.contains("InviteLink"){
            let mut result=body;result["invite_link"]=json!("https://t.me/+fixture");if method=="revokeChatInviteLink"{result["is_revoked"]=json!(true)}result
        }else{json!(true)};
        (StatusCode::OK,Json(json!({"ok":true,"result":result})))
    })).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), mock, server))
}
fn settings(dir: &TempDir, base: &str) -> Settings {
    let mut settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_BOT_TOKEN".to_owned(), "1:fixture".to_owned()),
        ("STUDIO_API_KEY".to_owned(), "owner-secret".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]))
    .unwrap_or_else(|e| panic!("settings {e}"));
    settings.api_base = base.to_owned();
    settings
}
fn services(settings: &Settings, context: AccountContext) -> (InviteLinks, SuggestedPosts) {
    let transport = TelegramTransport::new(settings.api_base.clone(), settings.bot_token.clone());
    let posts = PostService::new(
        context.clone(),
        transport.clone(),
        settings,
        Spec::load_initial(settings),
    );
    (
        InviteLinks::new(context.clone(), posts, transport.clone()),
        SuggestedPosts::new(context, transport),
    )
}
fn suggestion(context: &AccountContext, mid: i64) -> Result<String, Box<dyn std::error::Error>> {
    suggestions::save(
        context,
        -100500,
        mid,
        &json!({"chat":{"id":-100500,"title":"News Direct Messages"},"message_id":mid,"text":"Proposal","from":{"first_name":"First","last_name":"Last","username":"author"},"direct_messages_topic":{"topic_id":2}}),
        &json!({"state":"pending","price_amount":100,"price_currency":"XTR"}),
    )?;
    Ok(suggestions::key(-100500, mid))
}
async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    credential: &str,
    session: bool,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .header("content-type", "application/json");
    builder = if session {
        builder.header("cookie", format!("rs_session={credential}"))
    } else {
        builder.header("authorization", format!("Bearer {credential}"))
    };
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key)
    }
    let response = app
        .oneshot(
            builder
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
async fn invites_preserve_omitted_limits_explicit_clear_counters_and_revocation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (links, _) = services(&settings, context.clone());
    let expires = auth::now() + 86400;
    let created = links
        .create(
            &json!({"chat_id":-100500,"name":" Campaign ","member_limit":5,"expire_date":expires}),
            "api@test",
        )
        .await?;
    assert_eq!(created["name"], "Campaign");
    assert_eq!(created["actor"], "api@test");
    let link = created["invite_link"].as_str().ok_or("link")?;
    assert!(invite_links::on_member(&context,&json!({"invite_link":{"invite_link":link},"old_chat_member":{"status":"left"},"new_chat_member":{"status":"restricted","is_member":true}})).await?);
    assert!(!invite_links::on_member(&context,&json!({"invite_link":{"invite_link":link},"old_chat_member":{"status":"member"},"new_chat_member":{"status":"administrator"}})).await?);
    invite_links::on_join_request(&context, &json!({"invite_link":{"invite_link":link}})).await?;
    links
        .edit(&json!({"invite_link":link,"name":"Renamed"}))
        .await?;
    let edit = mock
        .calls()
        .into_iter()
        .find(|(method, _)| method == "editChatInviteLink")
        .ok_or("edit")?
        .1;
    assert_eq!(edit["member_limit"], 5);
    assert_eq!(edit["expire_date"], expires);
    links.edit(&json!({"invite_link":link,"member_limit":0,"expire_date":0,"creates_join_request":false})).await?;
    let edit = mock
        .calls()
        .into_iter()
        .rev()
        .find(|(method, _)| method == "editChatInviteLink")
        .ok_or("edit")?
        .1;
    assert!(edit.get("member_limit").is_none());
    assert!(edit.get("expire_date").is_none());
    links.revoke(link).await?;
    let again = links.revoke(link).await?;
    assert_eq!(again["joined"], 1);
    assert_eq!(again["requests"], 1);
    assert_eq!(again["actor"], "api@test");
    assert_eq!(mock.count("revokeChatInviteLink"), 1);
    assert!(
        links
            .edit(&json!({"invite_link":link,"name":"bad"}))
            .await
            .is_err()
    );
    assert!(links.revoke("https://t.me/+unknown").await.is_err());
    let subscription = links
        .create(
            &json!({"chat_id":-100500,"subscription_price":500,"name":"Paid"}),
            "api",
        )
        .await?;
    assert_eq!(subscription["kind"], "subscription");
    assert_eq!(subscription["subscription_period"], 2592000);
    assert!(
        links
            .create(
                &json!({"chat_id":-100500,"subscription_price":500,"member_limit":2}),
                "api"
            )
            .await
            .is_err()
    );
    assert_eq!(mock.count("createChatSubscriptionInviteLink"), 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn suggestion_claim_serializes_concurrent_decisions_and_cancellation_keeps_claim()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (_, service) = services(&settings, context.clone());
    let id = suggestion(&context, 1)?;
    mock.mode.store(3, Ordering::SeqCst);
    let first_service = service.clone();
    let first_id = id.clone();
    let first = tokio::spawn(async move {
        first_service
            .approve(&first_id, &Value::Null, "first")
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    assert!(
        service
            .decline(&id, &json!("second"), "second")
            .await
            .is_err()
    );
    assert_eq!(mock.count("declineSuggestedPost"), 0);
    mock.release.notify_one();
    assert_eq!(first.await??["state"], "approved");
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    let cancel_id = suggestion(&context, 2)?;
    let copy = cancel_id.clone();
    let service_copy = service.clone();
    let canceled =
        tokio::spawn(async move { service_copy.approve(&copy, &Value::Null, "canceled").await });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    canceled.abort();
    let _ = canceled.await;
    mock.release.notify_one();
    assert_eq!(
        suggestions::get(&context, &cancel_id)?.ok_or("suggestion")?["state"],
        "approving"
    );
    assert!(
        service
            .approve(&cancel_id, &Value::Null, "retry")
            .await
            .is_err()
    );
    assert_eq!(mock.count("approveSuggestedPost"), 2);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn uncertain_decision_waits_for_monotonic_outcome_refusal_rolls_back_safely()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let settings = settings(&dir, &base);
    let context = AccountContext::root(&settings);
    let (_, service) = services(&settings, context.clone());
    let id = suggestion(&context, 1)?;
    mock.mode.store(2, Ordering::SeqCst);
    assert!(matches!(
        service.approve(&id, &Value::Null, "api").await,
        Err(rich_studio::errors::ServiceError::MayHaveGone(_))
    ));
    assert!(service.approve(&id, &Value::Null, "retry").await.is_err());
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    assert!(suggested_posts::on_service(&context,&json!({"chat":{"id":-100500},"suggested_post_paid":{"suggested_post_message":{"message_id":1}}})).await?);
    suggested_posts::on_message(&context,&json!({"chat":{"id":-100500},"message_id":1,"text":"edited","suggested_post_info":{"state":"pending"}})).await?;
    assert_eq!(
        suggestions::get(&context, &id)?.ok_or("suggestion")?["state"],
        "paid"
    );
    assert!(service.reopen(&id, "admin").await.is_err());
    let refused = suggestion(&context, 2)?;
    mock.mode.store(1, Ordering::SeqCst);
    assert!(
        service
            .decline(&refused, &json!("No"), "api")
            .await
            .is_err()
    );
    assert_eq!(
        suggestions::get(&context, &refused)?.ok_or("suggestion")?["state"],
        "pending"
    );
    mock.mode.store(4, Ordering::SeqCst);
    let copy = refused.clone();
    let service_copy = service.clone();
    let pending =
        tokio::spawn(async move { service_copy.approve(&copy, &Value::Null, "api").await });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified()).await?;
    suggested_posts::on_service(&context,&json!({"chat":{"id":-100500},"suggested_post_refunded":{"suggested_post_message":{"message_id":2}}})).await?;
    mock.release.notify_one();
    assert!(pending.await?.is_err());
    assert_eq!(
        suggestions::get(&context, &refused)?.ok_or("suggestion")?["state"],
        "refunded"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn guarded_http_and_mcp_enforce_roles_and_idempotency_for_suggestion_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "reader", "reader-secret", "read")?;
    auth::add_token(&config, "publisher", "publish-secret", "publish")?;
    let state = AppState::new(config.clone());
    let id = suggestion(&state.account, 5)?;
    let path = format!("/api/suggested-posts/{id}/approve");
    let app = build_router(state.clone());
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "reader-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"approve_suggested_post","arguments":{"suggestion_id":id}}});
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/mcp",
            body.clone(),
            "reader-secret",
            false,
            None
        )
        .await
        .1["result"]["isError"],
        true
    );
    assert_eq!(mock.count("approveSuggestedPost"), 0);
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "publish-secret",
            false,
            Some("decision-one")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &path,
            json!({}),
            "publish-secret",
            false,
            Some("decision-one")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("approveSuggestedPost"), 1);
    assert_eq!(
        suggestions::get(&state.account, &id)?.ok_or("suggestion")?["actor"],
        "api@publisher"
    );
    let uncertain = suggestion(&state.account, 6)?;
    suggestions::claim(&state.account, &uncertain, &["pending"], "approving")?;
    let reopen = format!("/api/suggested-posts/{uncertain}/reopen");
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &reopen,
            json!({}),
            "publish-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(app, "POST", &reopen, json!({}), "owner-secret", false, None)
            .await
            .0,
        StatusCode::OK
    );
    let mut off = config;
    off.mcp_publish = false;
    assert_eq!(
        request(
            build_router(AppState::new(off)),
            "POST",
            "/mcp",
            body,
            "owner-secret",
            false,
            None
        )
        .await
        .1["result"]["isError"],
        true
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn public_invites_and_suggestions_cannot_read_or_mutate_other_accounts()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let mut config = settings(&dir, &base);
    config.public = true;
    for (uid, secret) in [(11, "account-a"), (22, "account-b")] {
        accounts::initialize(&config, uid)?;
        auth::save_account(&config, &json!({"id":uid,"first_name":"test"}))?;
        auth::add_account_session(&config, secret, uid, 86400)?;
    }
    let a = AccountContext::account(&config, 11, Role::Publish);
    let b = AccountContext::account(&config, 22, Role::Publish);
    channels::save(
        &a,
        &json!({"chat_id":"-100500","chat_num":-100500,"type":"channel","can_post":1}),
    )?;
    link_store::save(
        &a,
        "https://t.me/+only-a",
        "-100500",
        "a",
        &json!({"name":"Private"}),
    )?;
    let sid = suggestion(&a, 7)?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/invite-links",
            Value::Null,
            "account-b",
            true,
            None
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/suggested-posts",
            Value::Null,
            "account-b",
            true,
            None
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link":"https://t.me/+only-a"}),
            "account-b",
            true,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            &format!("/api/suggested-posts/{sid}/approve"),
            json!({}),
            "account-b",
            true,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/invite-links",
            json!({"chat_id":-100500,"name":"Owned"}),
            "account-a",
            true,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(mock.count("createChatInviteLink"), 1);
    assert_eq!(mock.count("getChatMember"), 1);
    assert!(link_store::listing(&b, None)?.is_empty());
    server.abort();
    Ok(())
}
