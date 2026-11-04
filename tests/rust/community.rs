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
        updates,
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
        atomic::{AtomicU8, AtomicU64, Ordering},
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
    links: Arc<Mutex<BTreeMap<String, Value>>>,
    link_ids: Arc<AtomicU64>,
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
    fn last(&self, name: &str) -> Value {
        self.calls()
            .into_iter()
            .rev()
            .find(|(method, _)| method == name)
            .map(|(_, body)| body)
            .unwrap_or_else(|| panic!("no {name} call"))
    }
    fn invite_call(&self, method: &str, body: Value) -> Value {
        let mut links = self
            .links
            .lock()
            .unwrap_or_else(|error| panic!("lock {error}"));
        let link = if method.starts_with("create") {
            let id = self.link_ids.fetch_add(1, Ordering::SeqCst) + 1;
            let link = format!("https://t.me/+fixture{id}");
            links.insert(link.clone(), json!({}));
            link
        } else {
            body["invite_link"].as_str().unwrap_or_default().to_owned()
        };
        let stored = links.get(&link).cloned().unwrap_or_else(|| json!({}));
        if method == "revokeChatInviteLink" {
            let mut record = stored;
            record["invite_link"] = json!(link.clone());
            record["is_revoked"] = json!(true);
            links.insert(link, record.clone());
            return record;
        }
        let mut record = json!({
            "invite_link": link.clone(),
            "creator": {"id": 42, "is_bot": true, "first_name": "Bot"},
            "creates_join_request": false,
            "is_primary": false,
            "is_revoked": false
        });
        for key in ["subscription_period", "subscription_price"] {
            if let Some(value) = stored.get(key) {
                record[key] = value.clone();
            }
        }
        for key in [
            "name",
            "expire_date",
            "member_limit",
            "creates_join_request",
            "subscription_period",
            "subscription_price",
        ] {
            if let Some(value) = body.get(key)
                && !value.is_null()
                && value.as_str() != Some("")
            {
                record[key] = value.clone();
            }
        }
        links.insert(link, record.clone());
        record
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
            mock.invite_call(&method,body)
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
        ("TG_DEFAULT_CHAT".to_owned(), "@test_channel".to_owned()),
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

fn member(link: &str, old: &str, new: &str, extra: Value) -> Value {
    let mut new_member = json!({"status": new, "user": {"id": 7}});
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            new_member[key.as_str()] = value.clone();
        }
    }
    json!({
        "chat_member": {
            "chat": {"id": -1001, "type": "channel"},
            "from": {"id": 7},
            "date": auth::now(),
            "old_chat_member": {"status": old, "user": {"id": 7}},
            "new_chat_member": new_member,
            "invite_link": {"invite_link": link, "creator": {"id": 42}, "is_primary": false, "is_revoked": false}
        }
    })
}

async fn mcp_call(app: axum::Router, tool: &str, arguments: Value, credential: &str) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
    let (status, response) = request(app, "POST", "/mcp", body, credential, false, None).await;
    assert_eq!(status, StatusCode::OK, "mcp {tool}: {response}");
    response["result"].clone()
}

fn mcp_list(result: &Value) -> Vec<Value> {
    result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    serde_json::from_str::<Value>(item["text"].as_str().unwrap_or_default()).ok()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn named_link_counts_joins_and_filters_listing_by_chat()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"chat_id": "@chan", "name": "Хабр", "member_limit": 100}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["name"], "Хабр");
    assert_eq!(link["member_limit"], 100);
    assert_eq!(link["joined"], 0);
    assert_eq!(
        mock.last("createChatInviteLink"),
        json!({"chat_id": "@chan", "name": "Хабр", "member_limit": 100})
    );
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        updates::handle_update(&state, &member(&url, "left", "member", json!({}))).await?,
        Some("join")
    );
    assert_eq!(
        updates::handle_update(
            &state,
            &member(&url, "kicked", "restricted", json!({"is_member": true}))
        )
        .await?,
        Some("join")
    );
    // an admin promoted, a link the studio did not make: nothing to count
    assert_eq!(
        updates::handle_update(&state, &member(&url, "member", "administrator", json!({}))).await?,
        None
    );
    assert_eq!(
        updates::handle_update(
            &state,
            &member("https://t.me/+other", "left", "member", json!({}))
        )
        .await?,
        None
    );
    let listed = request(
        app.clone(),
        "GET",
        "/api/invite-links?chat_id=%40chan",
        Value::Null,
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    let listed = listed.as_array().ok_or("listing")?;
    let counts = listed
        .iter()
        .map(|link| {
            (
                link["name"].as_str().unwrap_or_default(),
                link["joined"].as_i64().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, [("Хабр", 2)]);
    assert_eq!(
        request(
            app,
            "GET",
            "/api/invite-links?chat_id=%40other",
            Value::Null,
            "owner-secret",
            false,
            None
        )
        .await
        .1,
        json!([])
    );
    assert!(updates::ALLOWED_UPDATES.contains(&"chat_member"));
    assert!(updates::ALLOWED_UPDATES.contains(&"chat_join_request"));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn join_request_link_keeps_expiry_and_requests_across_edits_and_revoke()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let state = AppState::new(settings(&dir, &base));
    let app = build_router(state.clone());
    let expires = auth::now() + 86400;
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"name": "Заявки", "creates_join_request": true, "expire_date": expires}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["creates_join_request"], 1);
    assert_eq!(link["expire_date"], expires);
    let sent = mock.last("createChatInviteLink");
    assert_eq!(sent["chat_id"], "@test_channel");
    assert_eq!(sent["creates_join_request"], true);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        updates::handle_update(
            &state,
            &json!({"chat_join_request": {"chat": {"id": -1001}, "from": {"id": 8}, "invite_link": {"invite_link": url.clone()}}})
        )
        .await?,
        Some("join_request")
    );
    let conflicting = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"creates_join_request": true, "member_limit": 5}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(conflicting.0, StatusCode::BAD_REQUEST);
    assert!(
        conflicting.1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("заяв")
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({"name": "x".repeat(33)}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // Telegram drops what an edit leaves out, so a rename repeats the expiry and the join requests
    let edited = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "name": "Заявки 2"}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{}", edited.1);
    assert_eq!(edited.1["name"], "Заявки 2");
    assert_eq!(edited.1["requests"], 1);
    assert_eq!(edited.1["expire_date"], expires);
    assert_eq!(edited.1["creates_join_request"], 1);
    let sent = mock.last("editChatInviteLink");
    assert_eq!(sent["expire_date"], expires);
    assert_eq!(sent["creates_join_request"], true);
    let later = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "expire_date": expires + 60}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(later["name"], "Заявки 2");
    assert_eq!(later["expire_date"], expires + 60);
    assert_eq!(later["creates_join_request"], 1);
    let off = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url.clone(), "expire_date": 0, "creates_join_request": false}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(off["name"], "Заявки 2");
    assert!(off["expire_date"].is_null());
    assert_eq!(off["creates_join_request"], 0);
    assert!(mock.last("editChatInviteLink").get("expire_date").is_none());
    let revoked = request(
        app.clone(),
        "POST",
        "/api/invite-links/revoke",
        json!({"invite_link": url.clone()}),
        "owner-secret",
        false,
        None,
    )
    .await
    .1;
    assert_eq!(revoked["is_revoked"], 1);
    assert_eq!(revoked["name"], "Заявки 2");
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": url}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link": "https://t.me/+nope"}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn subscription_link_rejects_limits_and_changes_only_its_name()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let created = request(
        app.clone(),
        "POST",
        "/api/invite-links",
        json!({"name": "Клуб", "subscription_price": 50}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    let link = created.1;
    assert_eq!(link["kind"], "subscription");
    assert_eq!(link["subscription_price"], 50);
    let body = mock.last("createChatSubscriptionInviteLink");
    assert_eq!(body["subscription_period"], 2592000);
    assert_eq!(body["subscription_price"], 50);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({"subscription_price": 50, "member_limit": 3}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": url.clone(), "name": "Клуб+", "member_limit": 3}),
            "owner-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let edited = request(
        app,
        "PATCH",
        "/api/invite-links",
        json!({"invite_link": url, "name": "Клуб+"}),
        "owner-secret",
        false,
        None,
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{}", edited.1);
    assert_eq!(edited.1["name"], "Клуб+");
    assert_eq!(mock.last("editChatSubscriptionInviteLink")["name"], "Клуб+");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn invite_link_mutations_require_publish_while_lists_stay_readable()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let config = settings(&dir, &base);
    auth::add_token(&config, "d", "draft-secret", "draft")?;
    let app = build_router(AppState::new(config));
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links",
            json!({}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "PATCH",
            "/api/invite-links",
            json!({"invite_link": "x"}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/invite-links/revoke",
            json!({"invite_link": "x"}),
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app,
            "GET",
            "/api/invite-links",
            Value::Null,
            "draft-secret",
            false,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(mock.calls().is_empty());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn invite_links_work_through_mcp_create_edit_list_and_revoke()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let (base, mock, server) = mock().await?;
    let app = build_router(AppState::new(settings(&dir, &base)));
    let created = mcp_call(
        app.clone(),
        "create_invite_link",
        json!({"chat_id": "@chan", "name": "Telegram-чат"}),
        "owner-secret",
    )
    .await;
    assert_eq!(created["isError"], false);
    let link = created["structuredContent"].clone();
    assert!(
        link["invite_link"]
            .as_str()
            .ok_or("link")?
            .starts_with("https://t.me/+")
    );
    let listed =
        mcp_list(&mcp_call(app.clone(), "list_invite_links", json!({}), "owner-secret").await);
    let names = listed
        .iter()
        .map(|item| item["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Telegram-чат"]);
    let url = link["invite_link"].as_str().ok_or("link")?.to_owned();
    let limited = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "member_limit": 20}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(limited["name"], "Telegram-чат");
    assert_eq!(limited["member_limit"], 20);
    let renamed = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "name": "Чат"}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(renamed["name"], "Чат");
    assert_eq!(renamed["member_limit"], 20);
    let unnamed = mcp_call(
        app.clone(),
        "edit_invite_link",
        json!({"invite_link": url.clone(), "name": "", "member_limit": 0}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert!(unnamed["name"].is_null() || unnamed["name"] == json!(""));
    assert!(unnamed["member_limit"].is_null() || unnamed["member_limit"] == json!(0));
    let revoked = mcp_call(
        app,
        "revoke_invite_link",
        json!({"invite_link": url}),
        "owner-secret",
    )
    .await["structuredContent"]
        .clone();
    assert_eq!(revoked["is_revoked"], 1);
    assert_eq!(mock.count("revokeChatInviteLink"), 1);
    server.abort();
    Ok(())
}
