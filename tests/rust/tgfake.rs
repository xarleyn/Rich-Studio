//! Optional integration checks against a real fake Bot API process.
//!
//! The default suite uses in-process mocks and stays fast. This test is
//! ignored by default and starts the external `tgfake` binary (a separate,
//! resource-heavy process), then drives the Rust server through its HTTP API.
//!
//! Run with:
//!   TGFAKE_BIN=/path/to/tgfake cargo test --locked --test tgfake -- \
//!       --ignored --test-threads=1

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use reqwest::Client;
use rich_studio::{AppState, build_router, settings::Settings};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct TgFake {
    child: Child,
    origin: String,
}

impl Drop for TgFake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_tgfake() -> Result<TgFake, Box<dyn std::error::Error>> {
    let binary =
        std::env::var("TGFAKE_BIN").map_err(|_| "TGFAKE_BIN is not set; see docs/TGFAKE.md")?;
    let mut child = Command::new(binary)
        .args(["--addr", "127.0.0.1:0", "--poll-max", "1s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("tgfake stdout is missing")?;
    let mut banner = String::new();
    BufReader::new(stdout).read_line(&mut banner)?;
    let origin = banner
        .split(" at ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .ok_or_else(|| format!("unexpected tgfake banner: {banner:?}"))?
        .to_owned();
    Ok(TgFake { child, origin })
}

fn settings(dir: &TempDir, origin: &str) -> Settings {
    Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), origin.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("TG_DEFAULT_CHAT".to_owned(), "4242".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"))
}

async fn request(
    app: Router,
    method: &str,
    uri: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .header("host", "localhost")
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
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
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"));
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

async fn calls(
    client: &Client,
    origin: &str,
    method: &str,
) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let response: Value = client
        .get(format!("{origin}/sim/outbox?method={method}"))
        .send()
        .await?
        .json()
        .await?;
    Ok(response["calls"].as_array().cloned().unwrap_or_default())
}

#[tokio::test]
#[ignore = "requires TGFAKE_BIN and a running external tgfake process"]
async fn tgfake_accepts_messages_and_preserves_idempotency_and_errors() -> TestResult {
    let fake = start_tgfake()?;
    let client = Client::new();
    let mut ready = false;
    for _ in 0..100 {
        if client
            .get(format!("{}/sim/state", fake.origin))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready, "tgfake did not become ready");

    let dir = TempDir::new()?;
    let app = build_router(AppState::new(settings(&dir, &fake.origin)));
    let post = json!({"post":{"rich_message":{"markdown":"Hello from tgfake"}}});
    let mut created = Value::Null;
    for _ in 0..2 {
        let (status, response) = request(
            app.clone(),
            "POST",
            "/api/send",
            post.clone(),
            &[("idempotency-key", "tgfake-duplicate")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        created = response;
    }
    let sent = calls(&client, &fake.origin, "sendRichMessage").await?;
    assert_eq!(sent.len(), 1, "idempotency must not send twice: {sent:?}");
    assert_eq!(sent[0]["status"], 200);
    let chat_id = sent[0]["params"]["chat_id"]
        .as_str()
        .ok_or("chat_id parameter")?;
    let chat: Value = client
        .get(format!("{}/sim/chat/{chat_id}", fake.origin))
        .send()
        .await?
        .json()
        .await?;
    assert!(
        chat["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["rich"] == true)),
        "tgfake transcript has no rich message: {chat}"
    );

    let id = created["post"]["id"].as_str().ok_or("post id")?;
    let keyboard = json!({"inline_keyboard":[[{"text":"Да","callback_data":"yes"}]]});
    let (status, response) = request(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/buttons"),
        json!({"reply_markup":keyboard}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let edits = calls(&client, &fake.origin, "editMessageReplyMarkup").await?;
    assert_eq!(edits.len(), 1, "edit call: {edits:?}");
    assert_eq!(edits[0]["status"], 200);

    client
        .post(format!("{}/sim/fault", fake.origin))
        .json(&json!({"method":"sendRichMessage","code":400,"description":"Bad Request: chat not found","times":1}))
        .send()
        .await?
        .error_for_status()?;
    let (status, response) = request(
        app,
        "POST",
        "/api/send",
        json!({"post":{"rich_message":{"markdown":"must fail"}}}),
        &[("idempotency-key", "tgfake-refusal")],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
    let refused = calls(&client, &fake.origin, "sendRichMessage").await?;
    assert_eq!(refused.len(), 2, "refused call: {refused:?}");
    assert_eq!(refused[1]["status"], 400);
    Ok(())
}
