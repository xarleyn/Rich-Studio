use std::collections::BTreeMap;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rich_studio::settings::Settings;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

fn settings(dir: &TempDir, public: bool) -> Settings {
    let mut values = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_API_KEY".to_owned(), "owner-secret".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]);
    if public {
        for (key, value) in [
            ("STUDIO_MODE", "server"),
            ("STUDIO_PUBLIC", "on"),
            ("TG_BOT_TOKEN", "1:fixture"),
            ("TG_BOT_USERNAME", "FixtureBot"),
            ("STUDIO_URL", "https://studio.example"),
        ] {
            values.insert(key.to_owned(), value.to_owned());
        }
    }
    Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}"))
}

async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(
            builder
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body, headers)
}

fn cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_owned()
}

fn forwarded_headers(values: &[&str]) -> Result<axum::http::HeaderMap, Box<dyn std::error::Error>> {
    let mut headers = axum::http::HeaderMap::new();
    for value in values {
        headers.append("x-forwarded-for", value.parse()?);
    }
    Ok(headers)
}

async fn login_from(
    app: axum::Router,
    peer: &str,
    forwarded: &[&str],
) -> Result<StatusCode, Box<dyn std::error::Error>> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/session")
        .header("host", "studio.example")
        .header("content-type", "application/json")
        .body(Body::from(json!({"key":"incorrect-key"}).to_string()))?;
    request.headers_mut().extend(forwarded_headers(forwarded)?);
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>()?,
    ));
    Ok(app.oneshot(request).await?.status())
}

#[tokio::test]
async fn trusted_proxy_keeps_visitor_login_budgets_separate_and_direct_spoofing_cannot_reset_them()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    for _ in 0..30 {
        assert_eq!(
            login_from(app.clone(), "127.0.0.1:5600", &["203.0.113.10"]).await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5601", &["203.0.113.10"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5602", &["203.0.113.11"]).await?,
        StatusCode::UNAUTHORIZED
    );
    // An untrusted direct client cannot get fresh buckets by rotating XFF.
    for host in 1..=30 {
        assert_eq!(
            login_from(
                app.clone(),
                "198.51.100.20:5603",
                &[&format!("192.0.2.{host}")]
            )
            .await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "198.51.100.20:5604", &["192.0.2.31"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app, "198.51.100.21:5605", &["192.0.2.31"]).await?,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn proxy_chain_budget_stops_at_first_untrusted_hop_and_invalid_headers_use_peer()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8".parse()?;
    for key in ["login:203.0.113.9", "login:10.1.2.3"] {
        for _ in 0..30 {
            assert!(auth::take_budget(&config, key, 30, 60)?);
        }
    }
    let app = build_router(AppState::new(config));
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.1, 203.0.113.9", "10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.2, 203.0.113.9, 10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "10.1.2.3:5000", &["203.0.113.10, 10.30.40.50"]).await?,
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        "",
        "unknown",
        "192.0.2.1,",
        "203.0.113.10, bad-proxy",
        "999.1.1.1",
        "192.0.2.1:99999",
    ] {
        assert_eq!(
            login_from(app.clone(), "10.1.2.3:5001", &[invalid]).await?,
            StatusCode::TOO_MANY_REQUESTS,
            "invalid XFF: {invalid:?}"
        );
    }
    assert_eq!(
        login_from(app, "10.1.2.3:5002", &[]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    Ok(())
}

#[test]
fn forwarded_address_policy_covers_ipv6_cidrs_wildcard_and_invalid_settings()
-> Result<(), Box<dyn std::error::Error>> {
    use rich_studio::security::client_ip;
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    assert!(config.forwarded_allow_ips.contains("127.0.0.1".parse()?));
    assert!(!config.forwarded_allow_ips.contains("::1".parse()?));
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8,::1".parse()?;
    for (peer, forwarded, expected) in [
        (
            "10.1.2.3:5000",
            "192.0.2.1, 203.0.113.2, 10.20.30.40",
            "203.0.113.2",
        ),
        ("10.1.2.3:5000", "10.2.3.4, 10.3.4.5", "10.2.3.4"),
        ("[fd00::2]:5000", "2001:db8::4, fd00::3", "2001:db8::4"),
        (
            "[::1]:5000",
            "[2001:db8::5]:9876, [fd00::3]:4321",
            "2001:db8::5",
        ),
        ("[::1]:5000", "[2001:db8::5]", "2001:db8::5"),
        ("10.1.2.3:5000", "203.0.113.6:9876, 10.2.3.4", "203.0.113.6"),
        ("[fe00::1]:5000", "2001:db8::5", "fe00::1"),
    ] {
        assert_eq!(
            client_ip(
                &config,
                &forwarded_headers(&[forwarded])?,
                Some(peer.parse()?)
            ),
            Some(expected.parse()?),
            "{peer}: {forwarded}"
        );
    }
    let mut invalid = axum::http::HeaderMap::new();
    invalid.insert(
        "x-forwarded-for",
        axum::http::HeaderValue::from_bytes(&[0xff])?,
    );
    assert_eq!(
        client_ip(&config, &invalid, Some("10.1.2.3:5000".parse()?)),
        Some("10.1.2.3".parse()?)
    );
    config.forwarded_allow_ips = "*".parse()?;
    let headers = forwarded_headers(&["192.0.2.1, 203.0.113.2"])?;
    assert_eq!(
        client_ip(&config, &headers, Some("198.51.100.1:50".parse()?)),
        Some("192.0.2.1".parse()?)
    );
    assert_eq!(client_ip(&config, &headers, None), None);
    config.forwarded_allow_ips = "".parse()?;
    assert_eq!(
        client_ip(&config, &headers, Some("127.0.0.1:50".parse()?)),
        Some("127.0.0.1".parse()?)
    );
    for raw in [
        "127.0.0.1/33",
        "fd00::/129",
        "localhost",
        "10.0.0.0/bad",
        "*,127.0.0.1",
    ] {
        assert!(
            Settings::from_map(&BTreeMap::from([(
                "FORWARDED_ALLOW_IPS".to_owned(),
                raw.to_owned()
            )]))
            .is_err(),
            "accepted malformed proxy setting: {raw}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_key_is_enforced_and_legacy_key_session_dies_on_rotation()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    let app = build_router(AppState::new(config.clone()));
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/session",
        json!({"key":"owner-secret"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"name":null,"level":"admin"}));
    let session = cookie(&headers);
    assert!(!session.contains("owner-secret"));
    assert!(headers["set-cookie"].to_str()?.contains("HttpOnly"));
    assert_eq!(
        request(
            app,
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::OK
    );
    config.api_key = "rotated".to_owned();
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("cookie", &session)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_sessions_tokens_and_drafts_are_isolated_over_http()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    for uid in [101, 202] {
        auth::save_account(
            &config,
            &json!({"id":uid,"first_name":format!("User {uid}")}),
        )?;
        auth::add_account_session(&config, &format!("session-{uid}"), uid, 3600)?;
    }
    let app = build_router(AppState::new(config.clone()));
    let one = [("cookie", "rs_session=session-101")];
    let two = [("cookie", "rs_session=session-202")];
    let (status, draft, _) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        json!({"title":"Private","post":{"rich_message":{"markdown":"secret"}}}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = format!("/api/drafts/{}", draft["id"].as_str().ok_or("draft id")?);
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "GET", "/api/drafts", Value::Null, &two)
            .await
            .1,
        json!([])
    );
    let (status, token, _) = request(
        app.clone(),
        "POST",
        "/api/account/tokens",
        json!({"name":"Agent","level":"read"}),
        &one,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let bearer = format!("Bearer {}", token["secret"].as_str().ok_or("secret")?);
    let credential = [("authorization", bearer.as_str())];
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({"title":"Denied","post":{}}),
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/account/tokens",
            Value::Null,
            &credential
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/session",
            json!({"key":token["secret"]}),
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let token_path = format!(
        "/api/account/tokens/{}",
        token["token"]["id"].as_str().ok_or("token id")?
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &two)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app.clone(), "DELETE", &token_path, Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null, &credential)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/api/session", Value::Null, &one)
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}

fn signed_callback(state: &str, uid: i64, token: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    let mut fields = BTreeMap::from([
        ("id", uid.to_string()),
        ("first_name", "User".to_owned()),
        ("auth_date", auth::now().to_string()),
    ]);
    let check = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&Sha256::digest(token.as_bytes()))
        .unwrap_or_else(|error| panic!("hmac: {error}"));
    mac.update(check.as_bytes());
    let hash = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fields.insert("hash", hash);
    fields.insert("state", state.to_owned());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    format!("/auth/telegram/callback?{query}")
}

#[tokio::test]
async fn telegram_login_is_signed_browser_bound_one_use_and_owner_stays_root()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.owner_telegram_id = 101;
    let app = build_router(AppState::new(config.clone()));
    let (status, body, headers) = request(
        app.clone(),
        "POST",
        "/api/auth/telegram/start",
        json!({}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = body["auth_url"]
        .as_str()
        .and_then(|url| url.split("state=").nth(1))
        .ok_or("state")?;
    let login_cookie = cookie(&headers);
    let callback = signed_callback(state, 101, "1:fixture");
    assert_eq!(
        request(app.clone(), "GET", &callback, Value::Null, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let tampered = signed_callback(state, 101, "wrong-token");
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &tampered,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _, headers) = request(
        app.clone(),
        "GET",
        &callback,
        Value::Null,
        &[("cookie", &login_cookie)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let owner_cookie = cookie(&headers);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .1["level"],
        "admin"
    );
    assert!(auth::account(&config, 101)?.is_none());
    assert!(!config.account_dir(101).exists());
    assert_eq!(
        request(
            app,
            "GET",
            &callback,
            Value::Null,
            &[("cookie", &login_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    config.owner_telegram_id = 202;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/session",
            Value::Null,
            &[("cookie", &owner_cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn account_token_limits_expiry_and_management_validation_are_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    auth::save_account(&config, &json!({"id":101,"first_name":"User"}))?;
    auth::add_account_session(&config, "browser", 101, 3600)?;
    let app = build_router(AppState::new(config.clone()));
    let owner = [("cookie", "rs_session=browser")];
    for body in [
        json!({"name":"x","level":"admin"}),
        json!({"name":"x","telegram_id":202}),
        json!({"name":"x","expires_in_days":true}),
        json!({"name":"x","expires_in_days":0}),
    ] {
        assert_eq!(
            request(app.clone(), "POST", "/api/account/tokens", body, &owner)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut first = Value::Null;
    for index in 0..20 {
        let (status, token, _) = request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":format!("Agent{index}")}),
            &owner,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        if index == 0 {
            first = token;
        }
    }
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/account/tokens",
            json!({"name":"overflow"}),
            &owner
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    auth::database(&config)?.execute(
        "update account_tokens set expires_at=? where id=?",
        rusqlite::params![auth::now(), first["token"]["id"].as_str()],
    )?;
    let bearer = format!("Bearer {}", first["secret"].as_str().ok_or("secret")?);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/api/drafts",
            Value::Null,
            &[("authorization", &bearer)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/account/tokens",
            json!({"name":"replacement"}),
            &owner
        )
        .await
        .0,
        StatusCode::CREATED
    );
    Ok(())
}

#[tokio::test]
async fn revoked_last_token_does_not_open_service_and_mcp_enforces_roles()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.api_key.clear();
    auth::add_token(&config, "reader", "read-secret", "read")?;
    let app = build_router(AppState::new(config.clone()));
    let headers = [("authorization", "Bearer read-secret")];
    assert_eq!(
        request(app.clone(), "POST", "/api/send", json!({}), &headers)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"x"}}}}}), &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["result"]["isError"], true);
    auth::revoke_token(&config, "reader")?;
    assert_eq!(
        request(
            build_router(AppState::new(config)),
            "GET",
            "/api/drafts",
            Value::Null,
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn unsafe_origins_unknown_hosts_and_mcp_publish_off_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    config.mcp_publish = false;
    let app = build_router(AppState::new(config));
    let owner = [("x-api-key", "owner-secret")];
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/api/drafts",
            json!({}),
            &[
                ("x-api-key", "owner-secret"),
                ("origin", "http://localhost:9999")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let request_bad_host = Request::builder()
        .uri("/healthz")
        .header("host", "attacker.example")
        .body(Body::empty())?;
    assert_eq!(
        app.clone().oneshot(request_bad_host).await?.status(),
        StatusCode::BAD_REQUEST
    );
    let (_, tools, _) = request(
        app.clone(),
        "POST",
        "/mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &owner,
    )
    .await;
    assert!(
        !tools["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "publish_post")
    );
    let (_, result, _) = request(app, "POST", "/mcp", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish_post","arguments":{}}}), &owner).await;
    assert_eq!(result["result"]["isError"], true);
    Ok(())
}
