use axum::http::StatusCode;
use rich_studio::build_router;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, call_raw, state, telegram, upload};

#[tokio::test]
async fn raw_method_passthrough_and_invalid_method() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(app.clone(), "POST", "/api/raw/getMe", Some(json!({})), &[])
            .await
            .value["result"]["id"],
        42
    );
    assert_eq!(
        call(app, "POST", "/api/raw/get-me;rm", Some(json!({})), &[],)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn configured_key_protects_media_render_docs_and_static() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("STUDIO_API_KEY", "secret")]));
    for path in [
        "/media/a.png",
        "/render/draft/x",
        "/docs",
        "/openapi.json",
        "/api/spec?key=secret",
    ] {
        assert_eq!(
            call(app.clone(), "GET", path, None, &[]).await.status,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    assert_eq!(
        call(app.clone(), "GET", "/static/js/main.js", None, &[])
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/spec",
            None,
            &[("cookie", "rs_key=secret")],
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let session = call(
        app.clone(),
        "POST",
        "/api/session",
        Some(json!({"key":"secret"})),
        &[],
    )
    .await;
    assert_eq!(session.status, StatusCode::OK, "{}", session.text);
    let cookie = session
        .headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        call(app, "GET", "/api/spec", None, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn configured_key_accepts_the_header_too() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("STUDIO_API_KEY", "secret")]));
    assert_eq!(
        call(app.clone(), "GET", "/api/drafts", None, &[])
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app, "GET", "/api/drafts", None, &[("x-api-key", "secret")],)
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn non_json_and_cross_site_write_bodies_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let text = json!({"post":blocks_post()}).to_string().into_bytes();
    assert_eq!(
        call_raw(
            app.clone(),
            "POST",
            "/api/send",
            Some(text),
            &[("content-type", "text/plain")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        call_raw(
            app.clone(),
            "POST",
            "/api/raw/deleteMessage",
            Some(b"chat_id=@x&message_id=1".to_vec()),
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        call_raw(
            app,
            "POST",
            "/api/raw/logOut",
            Some(Vec::new()),
            &[("content-type", "text/plain")],
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for method in ["sendRichMessage", "deleteMessage", "logOut"] {
        assert_eq!(telegram.count(method), 0, "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn foreign_host_is_refused_but_allowed_hosts_work() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("host", "evil.example")],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            app,
            "GET",
            "/api/drafts",
            None,
            &[("host", "localhost:8080")],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn cross_site_writes_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let cases: &[&[(&str, &str)]] = &[
        &[("origin", "https://evil.example")],
        &[("origin", "null")],
        &[("sec-fetch-site", "cross-site")],
        &[("origin", "http://localhost:8080")],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:8080"),
        ],
        &[("host", "localhost:443"), ("origin", "http://localhost")],
        &[
            ("origin", "https://evil.example"),
            ("sec-fetch-site", "same-origin"),
        ],
    ];
    for headers in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            Some(json!({})),
            headers,
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{headers:?}: {}",
            reply.text
        );
        let media = upload(
            app.clone(),
            "x.html",
            b"<script>1</script>",
            "text/html",
            headers,
        )
        .await;
        assert_eq!(media.status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn same_origin_writes_are_allowed() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let cases: &[&[(&str, &str)]] = &[
        &[
            ("origin", "http://localhost"),
            ("sec-fetch-site", "same-origin"),
        ],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:7424"),
            ("sec-fetch-site", "same-origin"),
        ],
        &[
            ("host", "localhost:7424"),
            ("origin", "http://localhost:7424"),
        ],
        &[("host", "[::1]:7424"), ("origin", "http://[::1]:7424")],
        &[("host", "localhost:443"), ("origin", "https://localhost")],
    ];
    for headers in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/raw/getMe",
            Some(json!({})),
            headers,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{headers:?}: {}", reply.text);
    }
    Ok(())
}

#[tokio::test]
async fn raw_methods_require_confirmation_outside_the_allowlist() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    assert_eq!(
        call(app.clone(), "POST", "/api/raw/logOut", Some(json!({})), &[])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert!(telegram.calls().is_empty());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/raw/logOut",
            Some(json!({})),
            &[("x-confirm", "yes")],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert!(telegram.last("logOut").is_some());
    telegram.clear_calls();
    for method in [
        "deleteMessages",
        "setChatTitle",
        "unpinAllChatMessages",
        "getUpdates",
        "someMethodTelegramAddsLater",
    ] {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                &format!("/api/raw/{method}"),
                Some(json!({})),
                &[],
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "{method}"
        );
    }
    assert!(telegram.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn raw_read_and_send_methods_run_without_confirmation() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for method in [
        "getChat",
        "sendMessage",
        "editMessageText",
        "answerCallbackQuery",
        "copyMessage",
        "stopPoll",
        "pinChatMessage",
    ] {
        let reply = call(
            app.clone(),
            "POST",
            &format!("/api/raw/{method}"),
            Some(json!({})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{method}: {}", reply.text);
        assert!(telegram.last(method).is_some(), "{method}");
    }
    Ok(())
}
