use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState, build_router,
    services::{kinds, media::public_address},
    settings::Settings,
    store::{drafts as draft_store, posts as post_store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn app(dir: &TempDir, publish: bool) -> Router {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), "http://127.0.0.1:1".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
        (
            "MCP_PUBLISH".to_owned(),
            if publish { "on" } else { "off" }.to_owned(),
        ),
    ]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    build_router(AppState::new(settings))
}
async fn request_with_headers(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("response body: {error}"));
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
async fn request(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    request_with_headers(app, method, path, body, &[]).await
}
async fn request_raw(
    app: Router,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = app
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("response: {error}"));
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_else(|error| panic!("response body: {error}"));
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
async fn rpc(app: Router, method: &str, params: Value) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/mcp",
        Some(json!({"jsonrpc":"2.0","id":9,"method":method,"params":params})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

fn blocks_post() -> Value {
    json!({
        "rich_message": {
            "blocks": [
                {"type": "heading", "size": 1, "text": "Тест"},
                {"type": "paragraph", "text": ["Привет, ", {"type": "bold", "text": "мир"}]}
            ]
        }
    })
}

fn settings_with(dir: &TempDir, values: &[(&str, &str)]) -> Settings {
    let mut map = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]);
    for (name, value) in values {
        map.insert((*name).to_owned(), (*value).to_owned());
    }
    Settings::from_map(&map).unwrap_or_else(|error| panic!("settings: {error}"))
}

fn fake_state(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> AppState {
    let mut values = vec![
        ("TG_API_BASE", base),
        ("TG_BOT_TOKEN", "123:TEST"),
        ("TG_DEFAULT_CHAT", "@test_channel"),
        ("TG_PREVIEW_CHAT", "777"),
        ("TG_UPDATES", "off"),
        ("MCP_PUBLISH", "on"),
    ];
    values.extend_from_slice(extra);
    AppState::new(settings_with(dir, &values))
}

#[derive(Clone)]
struct FakeTelegram {
    calls: Arc<Mutex<Vec<Value>>>,
    queued: Arc<Mutex<BTreeMap<String, VecDeque<Value>>>>,
    next_message_id: Arc<AtomicI64>,
}

impl FakeTelegram {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            queued: Arc::new(Mutex::new(BTreeMap::new())),
            next_message_id: Arc::new(AtomicI64::new(100)),
        }
    }

    fn record(&self, method: &str, body: &Value) {
        if method == "getUpdates" {
            return;
        }
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(json!({"method":method,"body":body}));
    }

    fn all_calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    fn calls_to(&self, method: &str) -> Vec<Value> {
        self.all_calls()
            .into_iter()
            .filter(|call| call["method"].as_str() == Some(method))
            .collect()
    }

    fn last(&self, method: &str) -> Option<Value> {
        self.calls_to(method).pop()
    }

    /// A raw Bot API envelope answered before the mock's own reply, like `tg.queue` in the Python reference.
    fn queue(&self, method: &str, response: Value) {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .entry(method.to_owned())
            .or_default()
            .push_back(response);
    }

    fn take(&self, method: &str) -> Option<Value> {
        self.queued
            .lock()
            .unwrap_or_else(|error| panic!("queue: {error}"))
            .get_mut(method)
            .and_then(VecDeque::pop_front)
    }

    fn answer(&self, method: &str, body: &Value) -> Value {
        match method {
            "getMe" => {
                json!({"id":42,"is_bot":true,"first_name":"Bot","username":"studio_test_bot"})
            }
            "getChat" => json!({"id":-1001,"type":"channel","title":"Тест"}),
            "getChatMember" => {
                json!({"status":"administrator","can_post_messages":true,"can_edit_messages":true})
            }
            "sendRichMessage" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({
                    "message_id":id,
                    "chat":{"id":body["chat_id"].clone(),"type":"channel","title":"Тест"},
                    "rich_message":{"blocks":rich_blocks(&body["rich_message"])},
                })
            }
            "sendPoll" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({
                    "message_id":id,
                    "chat":{"id":body["chat_id"].clone()},
                    "poll":{"id":format!("poll{id}"),"question":body["question"]},
                })
            }
            "sendMessage" => {
                let id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
                json!({"message_id":id,"chat":{"id":body["chat_id"].clone()},"text":body["text"]})
            }
            "editMessageText" | "editMessageReplyMarkup" => {
                json!({"message_id":body["message_id"],"chat":{"id":body["chat_id"].clone()}})
            }
            "stopPoll" => json!({
                "id":"x","total_voter_count":3,"is_closed":true,
                "options":[{"text":"Да","voter_count":2},{"text":"Нет","voter_count":1}],
            }),
            _ => json!(true),
        }
    }
}

fn rich_blocks(rich: &Value) -> Vec<Value> {
    let parsed = match rich {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    if let Some(blocks) = parsed["blocks"].as_array() {
        return blocks.clone();
    }
    if let Some(text) = parsed["markdown"]
        .as_str()
        .or_else(|| parsed["html"].as_str())
    {
        return vec![json!({"type":"paragraph","text":text})];
    }
    Vec::new()
}

async fn fake_telegram() -> (String, FakeTelegram) {
    let fake = FakeTelegram::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let server = fake.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            let request_line = request.lines().next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let http_method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or_default().to_owned();
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let response_body = if http_method == "GET" && target == "/log" {
                Value::Array(server.all_calls()).to_string()
            } else {
                let payload = serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({}));
                let method = target.rsplit('/').next().unwrap_or_default().to_owned();
                server.record(&method, &payload);
                server
                    .take(&method)
                    .unwrap_or_else(|| json!({"ok":true,"result":server.answer(&method,&payload)}))
                    .to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    (format!("http://{address}"), fake)
}

async fn studio_with_fake(
    dir: &TempDir,
    extra: &[(&str, &str)],
) -> (String, FakeTelegram, AppState, Router) {
    let (base, fake) = fake_telegram().await;
    let state = fake_state(dir, &base, extra);
    let app = build_router(state.clone());
    (base, fake, state, app)
}

async fn call(app: Router, tool: &str, arguments: Value) -> (Value, Value, String) {
    let result = rpc(
        app,
        "tools/call",
        json!({"name":tool,"arguments":arguments}),
    )
    .await["result"]
        .clone();
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if result["isError"].as_bool() == Some(true) {
        return (result, Value::Null, text);
    }
    let data = serde_json::from_str(&text).unwrap_or(Value::Null);
    (result, data, text)
}

async fn call_ok(app: Router, tool: &str, arguments: Value) -> Value {
    let (result, data, text) = call(app, tool, arguments).await;
    assert_eq!(result["isError"], false, "{tool}: {text}");
    data
}

async fn call_fail(app: Router, tool: &str, arguments: Value) -> String {
    let (result, _, text) = call(app, tool, arguments).await;
    assert_eq!(result["isError"], true, "{tool}: {text}");
    text
}

async fn call_list(app: Router, tool: &str, arguments: Value) -> Vec<Value> {
    let result = rpc(
        app,
        "tools/call",
        json!({"name":tool,"arguments":arguments}),
    )
    .await["result"]
        .clone();
    assert_eq!(result["isError"], false, "{tool}: {result}");
    result["content"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|block| {
            serde_json::from_str(block["text"].as_str().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

fn media_names(state: &AppState) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&state.account.media_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

async fn upload_file(
    app: Router,
    name: &str,
    data: &[u8],
    content_type: &str,
) -> (StatusCode, Value) {
    let boundary = "studio-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    request_raw(
        app,
        "POST",
        "/api/media",
        &format!("multipart/form-data; boundary={boundary}"),
        body,
    )
    .await
}
#[tokio::test]
async fn all_legacy_http_operations_and_mcp_schemas_survive_the_sdk_adapter() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status, schema) = request(app.clone(), "GET", "/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    let original: Value = serde_json::from_str(include_str!("../../docs/openapi.json"))?;
    assert_eq!(schema, original);
    assert_eq!(
        schema["paths"]
            .as_object()
            .ok_or("paths")?
            .values()
            .map(|methods| methods.as_object().map_or(0, serde_json::Map::len))
            .sum::<usize>(),
        71
    );
    let init=rpc(app.clone(),"initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"contract","version":"1"}})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "rich-studio");
    assert!(
        init["result"]["instructions"]
            .as_str()
            .ok_or("instructions")?
            .contains("explicitly confirmed")
    );
    let listed = rpc(app, "tools/list", json!({})).await;
    let original: Value = serde_json::from_str(include_str!("../../src/mcp_tools/contract.json"))?;
    assert_eq!(listed["result"]["tools"], original["tools"]);
    assert_eq!(
        listed["result"]["tools"].as_array().ok_or("tools")?.len(),
        39
    );
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap_or_default(), tool))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        tools["publish_post"]["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(tools["validate_post"]["annotations"]["readOnlyHint"], true);
    Ok(())
}
#[tokio::test]
async fn malformed_http_models_fail_before_side_effects_and_unknown_post_fields_remain_open()
-> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for (path, body) in [
        ("/api/drafts", json!([])),
        ("/api/send", json!({})),
        (
            "/api/send",
            json!({"post":{"rich_message":{"markdown":"test"},"pin":"bad"}}),
        ),
        ("/api/posts/missing/buttons", json!({"callbacks":1})),
        ("/api/posts/missing/buttons", json!({"callbacks":[]})),
        (
            "/api/templates/missing/drafts",
            json!({"version":1,"values":{"name":3}}),
        ),
    ] {
        let (status, result) = request(app.clone(), "POST", path, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path} {result}");
    }
    let post = json!({"rich_message":{"markdown":"future"},"future_option":{"value":123}});
    let (status, created) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"contract","post":post})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (_, draft) = request(
        app,
        "GET",
        &format!("/api/drafts/{}", created["id"].as_str().ok_or("draft id")?),
        None,
    )
    .await;
    assert_eq!(draft["payload"]["future_option"], post["future_option"]);
    Ok(())
}
#[tokio::test]
async fn mcp_enforces_required_arguments_and_preserves_list_content_shape() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let missing = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"save_draft","arguments":{}}),
    )
    .await;
    assert_eq!(missing["result"]["isError"], true);
    let invalid = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"studio_logs","arguments":{"limit":99999}}),
    )
    .await;
    assert_eq!(invalid["result"]["isError"], true);
    for title in ["one", "two"] {
        let saved=rpc(app.clone(),"tools/call",json!({"name":"save_draft","arguments":{"title":title,"post":{"rich_message":{"markdown":title}}}})).await;
        assert_eq!(saved["result"]["isError"], false, "{saved}");
    }
    let listed = rpc(
        app,
        "tools/call",
        json!({"name":"list_drafts","arguments":{}}),
    )
    .await;
    let blocks = listed["result"]["content"].as_array().ok_or("content")?;
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|block| {
        serde_json::from_str::<Value>(block["text"].as_str().unwrap_or(""))
            .is_ok_and(|value| value.is_object())
    }));
    Ok(())
}
#[tokio::test]
async fn mcp_publish_off_hides_and_rejects_publishing_tools() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, false);
    let listed = rpc(app.clone(), "tools/list", json!({})).await;
    let contract: Value = serde_json::from_str(include_str!("../../src/mcp_tools/contract.json"))?;
    let publishing = BTreeSet::from([
        "publish_post",
        "schedule_post",
        "edit_post",
        "reschedule_post",
        "cancel_scheduled_post",
        "send_poll",
        "create_invite_link",
        "edit_invite_link",
        "revoke_invite_link",
        "send_media_post",
        "approve_suggested_post",
        "decline_suggested_post",
    ]);
    let expected = contract["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| !publishing.contains(name))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    let called=rpc(app,"tools/call",json!({"name":"publish_post","arguments":{"post":{"rich_message":{"markdown":"must not send"}}}})).await;
    assert_eq!(called["result"]["isError"], true);
    Ok(())
}

#[test]
fn python_model_coercions_defaults_and_open_telegram_payloads_match() -> TestResult {
    use rich_studio::services::contracts;
    let document = contracts::http_document()?;
    let schema = &document["components"]["schemas"]["PublishReq"];
    let rich = json!({"markdown":"hello","future_field":{"pin":"true","values":[null,"1",1],"large_integer":u64::MAX}});
    let options = json!({"disable_notification":"false","future_option":["0",false]});
    let normalized = contracts::normalize(
        document,
        schema,
        &json!({"post":{"rich_message":rich,"options":options,"pin":"TRUE","ignored_outer":1},"chat_id":"-1001","dry_run":"yes","ignored_request":1}),
    )?;
    assert_eq!(
        normalized,
        json!({"post":{"rich_message":rich,"options":options,"pin":true,"reply_markup":null,"callbacks":{},"comment":null,"poll_after":null},"chat_id":"-1001","chat_ids":null,"draft_id":null,"dry_run":true,"as_new":false})
    );
    assert_eq!(
        contracts::parse_post(&json!({"rich_message":{},"pin":"off"}))?["pin"],
        false
    );
    for bad in [
        json!({}),
        json!({"rich_message":{},"options":[]}),
        json!({"rich_message":{},"callbacks":1}),
        json!({"rich_message":{},"pin":"bad"}),
    ] {
        assert!(contracts::parse_post(&bad).is_err(), "{bad}");
    }
    let draft = &document["components"]["schemas"]["DraftReq"];
    for version in [json!("1"), json!(1.0), json!(true), json!("1.0")] {
        assert_eq!(
            contracts::normalize(
                document,
                draft,
                &json!({"post":{"arbitrary":[1,"2"]},"version":version})
            )?["version"],
            1
        );
    }
    let token = &document["components"]["schemas"]["TokenReq"];
    for expiry in [json!("1"), json!(1.0), json!(true)] {
        assert!(
            contracts::normalize(
                document,
                token,
                &json!({"name":"client","expires_in_days":expiry})
            )
            .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn http_normalizes_model_fields_before_dry_run_and_optimistic_update() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status,dry)=request(app.clone(),"POST","/api/send",Some(json!({"post":{"rich_message":{"markdown":"hello"},"pin":"false","options":{"disable_notification":"false"}},"chat_id":"@test","dry_run":"true"}))).await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(
        dry["requests"][0]["params"]["disable_notification"],
        "false"
    );
    let (_, draft) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"post":{"raw_data":"unchanged"}})),
    )
    .await;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let version = draft["version"].as_i64().ok_or("version")?;
    let (status, rejected) = request(
        app.clone(),
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"post":{"raw_data":"must not overwrite"},"version":9223372036854775808u64})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    let (_, unchanged) = request(app.clone(), "GET", &format!("/api/drafts/{id}"), None).await;
    assert_eq!(unchanged["version"], version);
    assert_eq!(unchanged["payload"], draft["payload"]);
    let (status, updated) = request(
        app,
        "PUT",
        &format!("/api/drafts/{id}"),
        Some(json!({"post":{"raw_data":"updated"},"version":version.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["version"], version + 1);
    Ok(())
}

#[tokio::test]
async fn mcp_post_model_is_checked_before_save_publish_and_preview() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for name in [
        "validate_post",
        "save_draft",
        "preview_post",
        "send_preview_to_me",
        "publish_post",
    ] {
        let result=rpc(app.clone(),"tools/call",json!({"name":name,"arguments":{"post":{"rich_message":{"markdown":"hello"},"options":[]},"dry_run":true}})).await;
        assert_eq!(result["result"]["isError"], true, "{name}: {result}");
    }
    let listed = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"list_drafts","arguments":{}}),
    )
    .await;
    assert_eq!(listed["result"]["content"], json!([]));
    let saved=rpc(app.clone(),"tools/call",json!({"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"hello"},"pin":"true"}}})).await;
    let id = saved["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("saved id")?;
    let both=rpc(app.clone(),"tools/call",json!({"name":"preview_post","arguments":{"draft_id":id,"post":{"rich_message":{"markdown":"different"}}}})).await;
    assert_eq!(both["result"]["isError"], true);
    let shown = rpc(
        app,
        "tools/call",
        json!({"name":"preview_post","arguments":{"draft_id":id}}),
    )
    .await;
    assert_eq!(shown["result"]["structuredContent"]["post"]["pin"], true);
    assert_eq!(
        shown["result"]["structuredContent"]["post"]["callbacks"],
        json!({})
    );
    Ok(())
}

#[tokio::test]
async fn optional_json_bodies_accept_empty_and_null_before_service_lookup() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for path in [
        "/api/posts/missing/pin",
        "/api/suggested-posts/missing/approve",
        "/api/suggested-posts/missing/decline",
    ] {
        for body in [None, Some(Value::Null)] {
            let (status, result) = request(app.clone(), "POST", path, body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {result}");
        }
    }
    Ok(())
}

#[test]
fn normalized_http_request_preserves_python_idempotency_fingerprint() -> TestResult {
    use rich_studio::services::{contracts, idempotency};
    let document = contracts::http_document()?;
    let input = json!({"post":{"rich_message":{"markdown":"hello"},"pin":"false","_studio":{"ignored":true}},"chat_id":"@channel","dry_run":0,"ignored_request":"discarded"});
    let normalized = contracts::normalize(
        document,
        &document["components"]["schemas"]["PublishReq"],
        &input,
    )?;
    // Exact Post.model_dump + PublishReq.model_dump from Python, including defaults.
    let python = json!({"post":{"rich_message":{"markdown":"hello"},"reply_markup":null,"options":{},"pin":false,"comment":null,"callbacks":{},"poll_after":null},"chat_id":"@channel","chat_ids":null,"draft_id":null,"dry_run":false,"as_new":false});
    assert_eq!(normalized, python);
    assert_eq!(
        idempotency::fingerprint("send", &normalized),
        idempotency::fingerprint("send", &python)
    );
    Ok(())
}

#[tokio::test]
async fn http_patch_preserves_omitted_fields_and_explicit_clear_values() -> TestResult {
    async fn echo(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
        axum::Json(body)
    }
    let app = Router::new()
        .route("/api/invite-links", axum::routing::patch(echo))
        .layer(axum::middleware::from_fn(
            rich_studio::api::validation::validate,
        ));
    let (status, omitted) = request(
        app.clone(),
        "PATCH",
        "/api/invite-links",
        Some(json!({"invite_link":"https://t.me/+fixture"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(omitted, json!({"invite_link":"https://t.me/+fixture"}));
    let (status,cleared)=request(app,"PATCH","/api/invite-links",Some(json!({"invite_link":"https://t.me/+fixture","name":"","member_limit":"0","creates_join_request":"false"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cleared,
        json!({"invite_link":"https://t.me/+fixture","name":"","member_limit":0,"creates_join_request":false})
    );
    Ok(())
}

#[tokio::test]
async fn config_and_mcp_media_schemas_match_the_kind_registry() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let (status, config) = request(app, "GET", "/api/config", None).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let described = config["kinds"].as_object().ok_or("missing kinds")?;
    assert_eq!(described.len(), kinds::KINDS.len());
    for kind in kinds::KINDS {
        let entry = &described[kind.name];
        assert_eq!(entry["label"], kind.label, "{}", kind.name);
        assert_eq!(entry["badge"], json!(kind.badge), "{}", kind.name);
        assert_eq!(entry["editable"], kind.editable, "{}", kind.name);
        assert_eq!(entry["stoppable"], kind.stoppable, "{}", kind.name);
        assert_eq!(entry["media"], kind.media, "{}", kind.name);
    }
    let contract: Value = serde_json::from_str(include_str!("../../src/mcp_tools/contract.json"))?;
    let tool = contract["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .find(|tool| tool["name"] == "send_media_post")
        .ok_or("send_media_post")?;
    let media = kinds::KINDS
        .iter()
        .filter(|kind| kind.media)
        .map(|kind| kind.name)
        .collect::<Vec<_>>();
    assert_eq!(
        tool["inputSchema"]["properties"]["kind"]["enum"],
        json!(media)
    );
    Ok(())
}

#[tokio::test]
async fn mcp_validate_and_describe_never_touch_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (result, data, text) =
        call(app.clone(), "validate_post", json!({"post":blocks_post()})).await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(data["ok"], true);
    let (_, described, _) = call(
        app.clone(),
        "describe_type",
        json!({"name":"InputRichBlockTable"}),
    )
    .await;
    assert_eq!(described["name"], "InputRichBlockTable");
    let (result, _, text) = call(app, "describe_type", json!({"name":"NoSuchType"})).await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("NoSuchType"), "{text}");
    assert!(tg.all_calls().is_empty(), "{:?}", tg.all_calls());
    Ok(())
}

#[tokio::test]
async fn mcp_publish_is_idempotent_and_marked_as_mcp() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    for _ in 0..2 {
        let (result, _, text) = call(
            app.clone(),
            "publish_post",
            json!({"post":blocks_post(),"idempotency_key":"mcp-1"}),
        )
        .await;
        assert_eq!(result["isError"], false, "{text}");
    }
    assert_eq!(tg.calls_to("sendRichMessage").len(), 1);
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["actor"], "mcp");
    assert_eq!(posts[0]["status"], "sent");
    Ok(())
}

#[tokio::test]
async fn mcp_media_upload_accepts_base64_and_fetched_urls() -> TestResult {
    use base64::Engine as _;

    let dir = TempDir::new()?;
    let (base, _tg, state, app) = studio_with_fake(&dir, &[("MEDIA_FETCH_PRIVATE", "on")]).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG fake");
    let file = call_ok(
        app.clone(),
        "upload_media",
        json!({"data_base64":encoded,"filename":"обложка.png"}),
    )
    .await;
    assert!(
        file["ref"]
            .as_str()
            .unwrap_or_default()
            .starts_with("upload:")
    );
    assert_eq!(file["kind"], "photo");
    let calls = call_ok(
        app.clone(),
        "upload_media",
        json!({"url":format!("{base}/log"),"filename":"calls.json"}),
    )
    .await;
    assert_eq!(calls["ref"], "upload:calls.json");
    let saved = std::fs::read(state.account.media_dir.join("calls.json"))?;
    assert!(saved.starts_with(b"["), "{saved:?}");
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":"file:///etc/passwd"}),
    )
    .await;
    assert!(text.contains("http"), "{text}");
    let text = call_fail(app.clone(), "upload_media", json!({"data_base64":"AAAA"})).await;
    assert!(text.contains("filename"), "{text}");
    let names = call_list(app, "list_media", json!({}))
        .await
        .into_iter()
        .filter_map(|file| file["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert!(names.contains("calls.json"));
    assert_eq!(names.len(), 2, "{names:?}");
    Ok(())
}

#[tokio::test]
async fn media_fetch_refuses_private_addresses_everywhere() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    for host in [
        "127.0.0.1",
        "10.1.2.3",
        "192.168.0.10",
        "169.254.169.254",
        "::1",
        "::ffff:127.0.0.1",
        "0.0.0.0",
        "100.64.0.1",
    ] {
        let address: IpAddr = host
            .parse()
            .unwrap_or_else(|error| panic!("{host}: {error}"));
        assert!(public_address(&[address], host).is_err(), "accepted {host}");
    }
    let public: IpAddr = "1.1.1.1".parse()?;
    assert!(public_address(&[public], "1.1.1.1").is_ok());
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":"http://127.0.0.1:8080/api/config"}),
    )
    .await;
    assert!(text.contains("внутренний"), "{text}");
    let (status, result) = request(
        app,
        "POST",
        "/api/media/fetch",
        Some(json!({"url":"http://localhost:8080/"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    assert!(media_names(&state).is_empty(), "{:?}", media_names(&state));
    assert!(tg.all_calls().is_empty());
    Ok(())
}

async fn file_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                if data.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&data);
            let path = request
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let response = match path.as_str() {
                "/to-inside" => {
                    b"HTTP/1.1 302 Found\r\nLocation: http://intranet/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                }
                "/hop" => {
                    b"HTTP/1.1 302 Found\r\nLocation: /hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                }
                _ => {
                    let body = vec![b'x'; 3 * 1024 * 1024];
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&body);
                    response
                }
            };
            if socket.write_all(&response).await.is_err() {
                break;
            }
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn media_fetch_checks_redirects_and_leaves_no_partial_file() -> TestResult {
    let dir = TempDir::new()?;
    let files = file_server().await;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[("MEDIA_FETCH_PRIVATE", "on")]).await;
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":format!("{files}/to-inside")}),
    )
    .await;
    assert!(text.contains("внутренний"), "{text}");
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":format!("{files}/hop")}),
    )
    .await;
    assert!(text.contains("перенаправлений"), "{text}");
    let (_base2, _tg2, state2, app2) = studio_with_fake(
        &dir,
        &[("MEDIA_FETCH_PRIVATE", "on"), ("MAX_UPLOAD_MB", "1")],
    )
    .await;
    let text = call_fail(
        app2,
        "upload_media",
        json!({"url":format!("{files}/big.jpg")}),
    )
    .await;
    assert!(text.contains("больше"), "{text}");
    assert!(media_names(&state).is_empty(), "{:?}", media_names(&state));
    assert!(
        media_names(&state2).is_empty(),
        "{:?}",
        media_names(&state2)
    );
    Ok(())
}

#[tokio::test]
async fn mcp_send_poll_is_idempotent() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let poll = json!({"question":"Когда эфир?","options":["Утром","Вечером"]});
    let result = call_ok(
        app.clone(),
        "send_poll",
        json!({"poll":poll,"chat_id":"@c","idempotency_key":"poll-1"}),
    )
    .await;
    assert_eq!(result["results"][0]["ok"], true);
    assert_eq!(
        tg.last("sendPoll").ok_or("sendPoll")?["body"]["question"],
        "Когда эфир?"
    );
    call_ok(
        app,
        "send_poll",
        json!({"poll":poll,"chat_id":"@c","idempotency_key":"poll-1"}),
    )
    .await;
    assert_eq!(tg.calls_to("sendPoll").len(), 1);
    Ok(())
}

#[tokio::test]
async fn mcp_links_point_at_the_editor_and_preview() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Ссылки"}),
    )
    .await;
    let expected_editor = format!(
        "http://localhost:7424/?draft={}",
        draft["id"].as_str().ok_or("draft id")?
    );
    assert_eq!(draft["editor_url"], expected_editor);
    assert_eq!(
        draft["preview_url"],
        format!(
            "http://localhost:7424/render/draft/{}",
            draft["id"].as_str().ok_or("draft id")?
        )
    );
    let shown = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(shown["editor_url"], draft["editor_url"]);
    let got = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(got["editor_url"], draft["editor_url"]);
    let sent = call_ok(app.clone(), "publish_post", json!({"post":blocks_post()})).await;
    let post = call_ok(app, "get_post", json!({"post_id":sent["post"]["id"]})).await;
    assert_eq!(
        post["editor_url"],
        format!(
            "http://localhost:7424/?post={}",
            sent["post"]["id"].as_str().ok_or("post id")?
        )
    );
    Ok(())
}

#[tokio::test]
async fn draft_channel_from_save_draft_is_used_by_publish() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс","chat_ids":["@agent_chan"]}),
    )
    .await;
    assert_eq!(draft["chat_ids"], json!(["@agent_chan"]));
    let again = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert_eq!(again["chat_ids"], json!(["@agent_chan"]));
    for (tool, extra) in [
        ("publish_post", json!({})),
        ("schedule_post", json!({"at":"2099-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({
            "draft_id":draft["id"],"version":again["version"],"dry_run":true
        });
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let dry = call_ok(app.clone(), tool, arguments).await;
        assert_eq!(
            dry["requests"][0]["params"]["chat_id"], "@agent_chan",
            "{tool}"
        );
    }
    assert!(tg.all_calls().is_empty());
    let (result, _, text) = call(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":again["version"]}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["chat_id"],
        "@agent_chan"
    );
    call_ok(
        app.clone(),
        "check_channel",
        json!({"chat_id":"@agent_chan"}),
    )
    .await;
    let channels = call_list(app, "list_channels", json!({})).await;
    let titles = channels
        .iter()
        .filter(|channel| channel["chat_id"] == "@agent_chan")
        .map(|channel| channel["title"].clone())
        .collect::<Vec<_>>();
    assert_eq!(titles, vec![json!("Тест")]);
    Ok(())
}

#[tokio::test]
async fn list_posts_reports_status_title_and_failures() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@ok"}),
    )
    .await;
    tg.queue(
        "sendRichMessage",
        json!({"ok":false,"error_code":400,"description":"Bad Request: inline keyboard button URL 'https://x' is invalid"}),
    );
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@bad"}),
    )
    .await;
    assert!(text.contains("invalid"), "{text}");
    let rows = call_list(app.clone(), "list_posts", json!({})).await;
    let summary = rows
        .iter()
        .map(|post| {
            (
                post["status"].clone(),
                post["title"].clone(),
                post["pinned"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            (json!("failed"), json!("Тест"), json!(false)),
            (json!("sent"), json!("Тест"), json!(false)),
        ]
    );
    assert_eq!(
        rows[1]["link"],
        format!("https://t.me/ok/{}", rows[1]["message_id"])
    );
    let failed = call_list(app, "list_posts", json!({"status":"failed"})).await;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["chat_id"], "@bad");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid"),
        "{}",
        failed[0]["error"]
    );
    Ok(())
}

#[tokio::test]
async fn mcp_publish_dry_run_and_validation_errors() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x","dry_run":true}),
    )
    .await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["requests"][0]["params"]["chat_id"], "@x");
    let bad = json!({"rich_message":{"blocks":[{"type":"heading","text":"x","size":"big"}]}});
    let text = call_fail(app, "publish_post", json!({"post":bad})).await;
    assert!(text.contains("проверку"), "{text}");
    assert!(tg.all_calls().is_empty());
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn mcp_save_draft_preview_path_and_preview_to_me() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    assert_eq!(
        draft["preview_path"],
        format!("/render/draft/{}", draft["id"].as_str().ok_or("draft id")?)
    );
    let (status, page) = request(
        app.clone(),
        "GET",
        draft["preview_path"].as_str().ok_or("preview path")?,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let sent = call_ok(app, "send_preview_to_me", json!({"post":blocks_post()})).await;
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["chat_id"],
        777
    );
    assert!(
        sent["parsed"]["input_blocks"]
            .as_array()
            .is_some_and(|blocks| !blocks.is_empty())
    );
    assert!(sent["parsed"].get("blocks").is_none(), "{sent}");
    Ok(())
}

#[tokio::test]
async fn mcp_requires_the_api_key() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[("STUDIO_API_KEY", "secret")]).await;
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let (status, _) = request(app.clone(), "POST", "/mcp", Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, listed) =
        request_with_headers(app, "POST", "/mcp", Some(body), &[("x-api-key", "secret")]).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    Ok(())
}

#[tokio::test]
async fn mcp_get_is_405_not_an_endless_stream() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let response = app
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .uri("/mcp")
                .header("accept", "text/event-stream")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    Ok(())
}

#[tokio::test]
async fn refresh_bot_api_reports_failure_and_keeps_the_spec_through_mcp() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(
        &dir,
        &[
            ("SPEC_PIN", "off"),
            ("SPEC_URL", "http://127.0.0.1:9/unavailable"),
        ],
    )
    .await;
    let before = call_ok(app.clone(), "bot_api_status", json!({})).await;
    let report = call_ok(app.clone(), "refresh_bot_api", json!({})).await;
    assert_eq!(report["updated"], false);
    assert_eq!(report["version"], before["version"]);
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{report}"
    );
    let after = call_ok(app, "bot_api_status", json!({})).await;
    assert_eq!(after["gaps"], json!([]));
    assert_eq!(after["last_check"]["error"], report["error"]);
    Ok(())
}

#[tokio::test]
async fn tools_that_reach_telegram_say_so() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    let listed = rpc(app, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("tools")?
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap_or_default().to_owned(),
                tool["annotations"].clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for name in [
        "send_preview_to_me",
        "check_channel",
        "publish_post",
        "schedule_post",
        "edit_post",
    ] {
        assert_eq!(
            tools.get(name).ok_or(name)?["openWorldHint"],
            true,
            "{name}"
        );
    }
    assert_eq!(tools["send_preview_to_me"]["destructiveHint"], false);
    Ok(())
}

#[test]
fn readme_points_at_the_generated_tool_reference() {
    let readme = include_str!("../../README.md");
    assert!(readme.contains("docs/reference/mcp-tools.md"));
    assert!(readme.contains("docs/reference/http-api.md"));
}

#[tokio::test]
async fn agent_continues_a_draft_without_making_copies() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Анонс"}),
    )
    .await;
    let full = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(
        full["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    let new = json!({"rich_message":{"markdown":"# Правка агента"}});
    let updated = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":new,"draft_id":draft["id"],"version":full["version"]}),
    )
    .await;
    assert_eq!(updated["id"], draft["id"]);
    assert_eq!(updated["title"], "Анонс");
    assert_eq!(
        updated["version"].as_i64(),
        full["version"].as_i64().map(|version| version + 1)
    );
    assert_eq!(draft_store::list(&state.account)?.len(), 1);
    let text = call_fail(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":full["version"]}),
    )
    .await;
    assert!(text.contains("изменили"), "{text}");
    let (result, _, _) = call(
        app,
        "save_draft",
        json!({"post":blocks_post(),"draft_id":"nope"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    Ok(())
}

#[tokio::test]
async fn agent_save_keeps_the_link_to_the_edited_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let (status, draft) = request(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Правка","post":blocks_post(),"post_id":pid})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let id = draft["id"].as_str().ok_or("draft id")?;
    call_ok(
        app,
        "save_draft",
        json!({"post":{"rich_message":{"markdown":"x"}},"draft_id":id}),
    )
    .await;
    let saved = draft_store::get(&state.account, id)?.ok_or("draft")?;
    assert_eq!(saved["post_id"], pid);
    Ok(())
}

#[tokio::test]
async fn agent_reads_a_post_before_editing_and_sees_history() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let post = call_ok(app.clone(), "get_post", json!({"post_id":pid})).await;
    assert_eq!(
        post["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    assert_eq!(post["status"], "sent");
    call_ok(
        app.clone(),
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Новое"}}}),
    )
    .await;
    let history = call_list(app, "post_history", json!({"post_id":pid})).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["actor"], "mcp");
    assert_eq!(
        history[0]["payload"]["rich_message"],
        blocks_post()["rich_message"]
    );
    Ok(())
}

#[tokio::test]
async fn media_list_and_studio_status() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, uploaded) = upload_file(app.clone(), "cat.png", b"\x89PNG", "image/png").await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let files = call_list(app.clone(), "list_media", json!({})).await;
    assert_eq!(files[0]["ref"], "upload:cat.png");
    assert_eq!(files[0]["used"], false);
    let studio = call_ok(app, "studio_status", json!({})).await;
    assert_eq!(studio["token_set"], true);
    assert_eq!(studio["updates"]["state"], "off");
    assert_eq!(studio["schedule_late_min"].as_f64(), Some(60.0));
    Ok(())
}

#[tokio::test]
async fn agent_moves_and_cancels_scheduled_posts_without_deleting_from_channel() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let record = call_ok(
        app.clone(),
        "schedule_post",
        json!({"post":blocks_post(),"at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    let moved = call_ok(
        app.clone(),
        "reschedule_post",
        json!({"post_id":record["id"],"at":"2099-01-02T09:00:00+03:00"}),
    )
    .await;
    assert!(moved["scheduled_at"].as_i64() > record["scheduled_at"].as_i64());
    let cancelled = call_ok(
        app.clone(),
        "cancel_scheduled_post",
        json!({"post_id":record["id"]}),
    )
    .await;
    assert_eq!(cancelled["status"], "cancelled");
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let text = call_fail(
        app,
        "cancel_scheduled_post",
        json!({"post_id":sent["post"]["id"]}),
    )
    .await;
    assert!(text.contains("sent"), "{text}");
    assert!(tg.calls_to("deleteMessage").is_empty());
    Ok(())
}

#[tokio::test]
async fn schedule_dry_run_creates_nothing() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[]).await;
    let report = call_ok(
        app,
        "schedule_post",
        json!({"post":blocks_post(),"at":"2099-01-01T09:00:00+03:00","dry_run":true}),
    )
    .await;
    assert_eq!(report["dry_run"], true);
    assert!(report["at"].as_i64().is_some(), "{report}");
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn publish_without_key_does_not_post_twice() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    for chat in ["@a", "@a", "@b"] {
        call_ok(
            app.clone(),
            "publish_post",
            json!({"post":blocks_post(),"chat_id":chat}),
        )
        .await;
    }
    let sent = tg
        .calls_to("sendRichMessage")
        .into_iter()
        .map(|call| call["body"]["chat_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(sent, vec![json!("@a"), json!("@b")]);
    Ok(())
}

#[tokio::test]
async fn flood_wait_is_reported_with_retry_after() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    tg.queue(
        "sendRichMessage",
        json!({"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":300}}),
    );
    let (status, error) = request(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
    assert_eq!(error["retry_after"], 300);
    Ok(())
}

#[tokio::test]
async fn agent_sends_the_post_once_and_then_works_by_draft_id() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"Один раз"}),
    )
    .await;
    assert_eq!(draft["validation"]["ok"], true);
    let preview = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(
        preview["post"]["rich_message"],
        blocks_post()["rich_message"]
    );
    assert_eq!(preview["validation"]["ok"], true);
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"chat_id":"@x","dry_run":true}),
    )
    .await;
    assert_eq!(dry["dry_run"], true);
    assert!(tg.all_calls().is_empty());
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"chat_id":"@x"}),
    )
    .await;
    assert!(text.contains("version"), "{text}");
    assert!(tg.all_calls().is_empty());
    let (result, _, text) = call(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"],"chat_id":"@x"}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["draft_id"], draft["id"]);
    assert_eq!(posts[0]["actor"], "mcp");
    assert_eq!(
        tg.last("sendRichMessage").ok_or("sendRichMessage")?["body"]["rich_message"],
        blocks_post()["rich_message"]
    );
    Ok(())
}

#[tokio::test]
async fn publishing_a_draft_changed_after_the_preview_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    let draft = call_ok(app.clone(), "save_draft", json!({"post":blocks_post()})).await;
    let id = draft["id"].as_str().ok_or("draft id")?;
    let version = draft["version"].as_i64().ok_or("version")?;
    draft_store::save(
        &state.account,
        Some(id),
        "",
        &json!({"rich_message":{"markdown":"правка в редакторе"}}),
        Some(version),
        None,
        None,
    )?
    .ok_or("draft was not saved")?;
    for (tool, extra) in [
        ("publish_post", json!({})),
        ("schedule_post", json!({"at":"2030-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({"draft_id":draft["id"],"version":draft["version"]});
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let text = call_fail(app.clone(), tool, arguments).await;
        assert!(text.contains("изменили"), "{tool}: {text}");
    }
    let text = call_fail(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert!(text.contains("одно из двух"), "{text}");
    assert!(tg.all_calls().is_empty());
    assert!(post_store::list(&state.account)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn save_draft_reports_validation_errors() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let bad = json!({"rich_message":{"blocks":[{"type":"table","rows":[]}]}});
    let draft = call_ok(app, "save_draft", json!({"post":bad})).await;
    assert_eq!(draft["validation"]["ok"], false);
    assert!(
        draft["validation"]["errors"][0]
            .as_str()
            .unwrap_or_default()
            .contains("cells"),
        "{}",
        draft["validation"]["errors"]
    );
    Ok(())
}

#[tokio::test]
async fn draft_editing_a_published_post_never_goes_out_as_a_new_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x"}),
    )
    .await;
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 1);
    let original = posts[0]["id"].as_str().ok_or("post id")?.to_owned();
    let draft = draft_store::save(
        &state.account,
        None,
        "правка",
        &json!({"rich_message":{"markdown":"новый текст"}}),
        None,
        Some(&original),
        None,
    )?
    .ok_or("draft")?;
    for (tool, extra) in [
        ("publish_post", json!({"chat_id":"@x"})),
        ("schedule_post", json!({"at":"2030-01-01T09:00:00+03:00"})),
    ] {
        let mut arguments = json!({"draft_id":draft["id"],"version":draft["version"]});
        if let Some(object) = extra.as_object() {
            for (name, value) in object {
                arguments[name] = value.clone();
            }
        }
        let text = call_fail(app.clone(), tool, arguments).await;
        assert!(text.contains("edit_post"), "{tool}: {text}");
        assert!(text.contains(&original), "{tool}: {text}");
    }
    let after = post_store::list(&state.account)?;
    assert_eq!(after.len(), 1);
    assert_eq!(tg.calls_to("sendRichMessage").len(), 1);
    let preview = call_ok(app.clone(), "preview_post", json!({"draft_id":draft["id"]})).await;
    assert_eq!(preview["version"], draft["version"]);
    let (result, _, text) = call(
        app.clone(),
        "edit_post",
        json!({"post_id":original,"draft_id":draft["id"],"version":preview["version"]}),
    )
    .await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(
        tg.last("editMessageText").ok_or("editMessageText")?["body"]["rich_message"],
        json!({"markdown":"новый текст"})
    );
    let after = post_store::list(&state.account)?;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["id"], original);
    let stored = post_store::get(&state.account, &original)?.ok_or("post")?;
    assert_eq!(
        stored["payload"]["rich_message"],
        json!({"markdown":"новый текст"})
    );
    Ok(())
}

#[tokio::test]
async fn edit_post_by_draft_checks_the_version_and_the_post() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@x"}),
    )
    .await;
    call_ok(
        app.clone(),
        "publish_post",
        json!({"post":blocks_post(),"chat_id":"@y"}),
    )
    .await;
    let posts = post_store::list(&state.account)?;
    assert_eq!(posts.len(), 2);
    let one = posts[0]["id"].as_str().ok_or("first post")?.to_owned();
    let two = posts[1]["id"].as_str().ok_or("second post")?.to_owned();
    let draft = draft_store::save(
        &state.account,
        None,
        "правка",
        &blocks_post(),
        None,
        Some(&one),
        None,
    )?
    .ok_or("draft")?;
    let text = call_fail(
        app.clone(),
        "edit_post",
        json!({"post_id":two,"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert!(text.contains(&one), "{text}");
    let text = call_fail(
        app.clone(),
        "edit_post",
        json!({"post_id":one,"draft_id":draft["id"]}),
    )
    .await;
    assert!(text.contains("version"), "{text}");
    assert!(tg.calls_to("editMessageText").is_empty());
    Ok(())
}

#[tokio::test]
async fn channels_are_named_wherever_a_post_goes() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[("TG_DEFAULT_CHAT", "@main")]).await;
    call_ok(app.clone(), "check_channel", json!({"chat_id":"@named"})).await;
    let draft = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"A","chat_ids":["@named"]}),
    )
    .await;
    assert_eq!(
        draft["goes_to"],
        json!([{"chat_id":"@named","title":"Тест","default":false}])
    );
    let plain = call_ok(
        app.clone(),
        "save_draft",
        json!({"post":blocks_post(),"title":"B"}),
    )
    .await;
    assert_eq!(
        plain["goes_to"],
        json!([{"chat_id":"@main","title":null,"default":true}])
    );
    let drafts = call_list(app.clone(), "list_drafts", json!({})).await;
    let by_title = drafts
        .iter()
        .filter_map(|draft| {
            draft["title"]
                .as_str()
                .map(|title| (title.to_owned(), draft))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(by_title["A"]["goes_to"][0]["title"], "Тест");
    assert_eq!(by_title["B"]["goes_to"][0]["default"], true);
    let got = call_ok(app.clone(), "get_draft", json!({"draft_id":draft["id"]})).await;
    assert_eq!(got["goes_to"][0]["title"], "Тест");
    let dry = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"],"dry_run":true}),
    )
    .await;
    assert_eq!(dry["requests"][0]["chat_title"], "Тест");
    let sent = call_ok(
        app.clone(),
        "publish_post",
        json!({"draft_id":draft["id"],"version":draft["version"]}),
    )
    .await;
    assert_eq!(sent["results"][0]["chat_title"], "Тест");
    let later = call_ok(
        app.clone(),
        "schedule_post",
        json!({"post":blocks_post(),"chat_id":"@named","at":"2099-01-01T09:00:00+03:00"}),
    )
    .await;
    assert_eq!(later["chat_title"], "Тест");
    let rows = call_list(app.clone(), "list_posts", json!({})).await;
    let titles = rows
        .iter()
        .filter_map(|post| post["chat_title"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert_eq!(titles, BTreeSet::from(["Тест".to_owned()]));
    let post = call_ok(app, "get_post", json!({"post_id":rows[0]["id"]})).await;
    assert_eq!(post["chat_title"], "Тест");
    Ok(())
}

#[tokio::test]
async fn edit_post_with_a_stale_post_version_is_a_tool_error() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, sent) = request(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let pid = sent["post"]["id"].as_str().ok_or("post id")?;
    let post = call_ok(app.clone(), "get_post", json!({"post_id":pid})).await;
    let done = call_ok(
        app.clone(),
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Моё"}},"post_version":post["version"]}),
    )
    .await;
    assert_eq!(
        done["version"].as_i64(),
        post["version"].as_i64().map(|version| version + 1)
    );
    let text = call_fail(
        app,
        "edit_post",
        json!({"post_id":pid,"post":{"rich_message":{"markdown":"# Чужое"}},"post_version":post["version"]}),
    )
    .await;
    assert!(text.contains("get_post"), "{text}");
    Ok(())
}
