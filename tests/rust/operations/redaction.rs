use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use rich_studio::{
    build_router,
    logs::{self, Logger},
    metrics::Metrics,
};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, initialized, request_with_headers, settings, telegram,
    telegram_settings,
};

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
                let pad = if counter.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
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
