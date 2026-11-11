//! HTML-виджеты (sendWidget, Bot API 10.4): лимиты, отправка, данные, запрет каналов.

use axum::http::{StatusCode, header};
use rich_studio::build_router;
use rich_studio::store::channels;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, call, call_raw, context, state, telegram};

const DEMO: &str = "<div style=\"background:#123\">виджет<button>ОК</button></div>";

#[tokio::test]
async fn widget_mcp_partial_edit_preserves_omitted_data() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("MCP_PUBLISH", "on")]));
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({
            "widget":{"html":DEMO,"chat_data":"old","public_data":"keep"},
            "chat_id":777,"at":4_500_000_000i64
        })),
        &[],
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    let id = scheduled.value["scheduled"][0]["id"].as_str().ok_or("id")?;
    let edit = call(
        app.clone(),
        "POST",
        "/mcp",
        Some(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"edit_widget_data","arguments":{"post_id":id,"chat_data":"new"}}
        })),
        &[],
    )
    .await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.text);
    assert_ne!(edit.value["result"]["isError"], true, "{}", edit.text);
    let row = call(app, "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(row.value["payload"]["widget"]["chat_data"], "new");
    assert_eq!(telegram.count("editMessageWidgetData"), 0);
    assert_eq!(
        row.value["payload"]["widget"]["public_data"], "keep",
        "omitted public_data must survive: {}",
        row.text
    );
    Ok(())
}

#[tokio::test]
async fn widget_rejects_nul_html() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let report = call(
        app,
        "POST",
        "/api/widget/validate",
        Some(json!({
            "widget":{"html":"<div>bad\0html</div>"}
        })),
        &[],
    )
    .await;
    assert_eq!(
        report.value["ok"], false,
        "NUL source must fail validation: {}",
        report.text
    );
    Ok(())
}

#[tokio::test]
async fn widget_rejects_non_string_data() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let report = call(
        app,
        "POST",
        "/api/widget/validate",
        Some(json!({
            "widget":{"html":DEMO,"chat_data":{"n":1}}
        })),
        &[],
    )
    .await;
    assert_eq!(
        report.value["ok"], false,
        "chat_data must be UTF-8 text: {}",
        report.text
    );
    Ok(())
}

#[tokio::test]
async fn widget_invalid_payloads_never_reach_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("MCP_PUBLISH", "on")]));
    for widget in [
        json!({"html":"<div>bad\0html</div>"}),
        json!({"html":DEMO,"chat_data":null}),
        json!({"html":DEMO,"public_data":{"n":1}}),
        json!({"html":DEMO,"chat_data":"я".repeat(513)}),
        json!({"html":DEMO,"public_data":"bad\0data"}),
        json!({"html":DEMO,"allow_queries":true}),
        json!({"html":DEMO,"allow_queries":"true"}),
    ] {
        let reply = call(
            app.clone(),
            "POST",
            "/api/widget",
            Some(json!({
                "widget":widget,"chat_id":777
            })),
            &[],
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text
        );
    }
    let mcp = call(app.clone(), "POST", "/mcp", Some(json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"send_widget","arguments":{"widget":{"html":DEMO,"allow_queries":true},"chat_id":777}}
    })), &[]).await;
    assert_eq!(mcp.value["result"]["isError"], true, "{}", mcp.text);
    assert_eq!(telegram.count("sendWidget"), 0);
    let rows = call(app, "GET", "/api/posts", None, &[]).await;
    assert_eq!(rows.value, json!([]));
    Ok(())
}

#[tokio::test]
async fn widget_unsupported_endpoint_is_explained_without_retry() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.queue(
        "sendWidget",
        vec![json!({"ok":false,"error_code":404,"description":"Not Found"})],
    );
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({"widget":{"html":DEMO},"chat_id":777})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text);
    assert!(reply.text.contains("Bot API 10.4"), "{}", reply.text);
    assert!(reply.text.contains("TG_API_BASE"), "{}", reply.text);
    assert_eq!(telegram.count("sendWidget"), 1);
    let rows = call(app, "GET", "/api/posts", None, &[]).await;
    assert_eq!(rows.value[0]["status"], "failed");
    Ok(())
}

#[tokio::test]
async fn widget_scheduler_sends_updated_data_and_refuses_legacy_queries() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app_state = state(&dir, &base, &[]);
    let app = build_router(app_state.clone());
    let at = 4_500_000_000i64;
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({
            "widget":{"html":DEMO,"public_data":"keep"},"chat_id":777,"at":at
        })),
        &[],
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    let id = scheduled.value["scheduled"][0]["id"].as_str().ok_or("id")?;
    let edit = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/widget-data?post_version=0"),
        Some(json!({"chat_data":"latest"})),
        &[],
    )
    .await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.text);
    let legacy = rich_studio::store::posts::add_scheduled(
        &context(&app_state),
        &[json!(777)],
        &json!({
            "widget":{"html":DEMO,"allow_queries":true}
        }),
        at,
        None,
        "widget",
        "legacy",
    )?;
    telegram.queue(
        "sendWidget",
        vec![json!({"ok":true,"result":{"message_id":780,"chat":{"id":777}}})],
    );
    assert_eq!(crate::support::run_due(&app_state, at).await, 2);
    assert_eq!(telegram.count("sendWidget"), 1);
    let params = telegram.last("sendWidget").ok_or("send")?.body;
    assert_eq!(params["chat_data"], "latest");
    assert_eq!(params["public_data"], "keep");
    let sent = call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(sent.value["status"], "sent");
    let legacy_id = legacy[0]["id"].as_str().ok_or("legacy id")?;
    let refused = call(app, "GET", &format!("/api/posts/{legacy_id}"), None, &[]).await;
    assert_eq!(refused.value["status"], "failed");
    assert!(
        refused.value["error"]
            .as_str()
            .is_some_and(|text| text.contains("allow_queries=true"))
    );
    Ok(())
}

#[tokio::test]
async fn widget_sent_mcp_patch_clear_noop_and_versions_are_safe() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[("MCP_PUBLISH", "on")]));
    telegram.queue(
        "sendWidget",
        vec![json!({"ok":true,"result":{"message_id":779,"chat":{"id":777}}})],
    );
    let sent = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({
            "widget":{"html":DEMO,"chat_data":"old","public_data":"keep"},"chat_id":777
        })),
        &[],
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
    let id = sent.value["results"][0]["post"]["id"]
        .as_str()
        .ok_or("id")?;
    for arguments in [
        json!({"post_id":id}),
        json!({"post_id":id,"chat_data":null}),
    ] {
        let reply = call(
            app.clone(),
            "POST",
            "/mcp",
            Some(json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"edit_widget_data","arguments":arguments}
            })),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(telegram.count("editMessageWidgetData"), 0);
    let edit = call(app.clone(), "POST", "/mcp", Some(json!({
        "jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"edit_widget_data","arguments":{"post_id":id,"post_version":0,"chat_data":""}}
    })), &[]).await;
    assert_ne!(edit.value["result"]["isError"], true, "{}", edit.text);
    let params = telegram.last("editMessageWidgetData").ok_or("edit")?.body;
    assert_eq!(params["chat_data"], "");
    assert!(params.get("public_data").is_none(), "{params}");
    let row = call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert!(row.value["payload"]["widget"].get("chat_data").is_none());
    assert_eq!(row.value["payload"]["widget"]["public_data"], "keep");
    assert_eq!(row.value["version"], 1);
    let stale = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/widget-data?post_version=0"),
        Some(json!({"public_data":"lost"})),
        &[],
    )
    .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text);
    let stale_mcp = call(app.clone(), "POST", "/mcp", Some(json!({
        "jsonrpc":"2.0","id":5,"method":"tools/call",
        "params":{"name":"edit_widget_data","arguments":{"post_id":id,"post_version":0,"public_data":"lost"}}
    })), &[]).await;
    assert_eq!(
        stale_mcp.value["result"]["isError"], true,
        "{}",
        stale_mcp.text
    );
    for value in [
        json!(null),
        json!(23),
        json!({"bad":true}),
        json!("bad\0data"),
    ] {
        let invalid = call(
            app.clone(),
            "POST",
            &format!("/api/posts/{id}/widget-data"),
            Some(json!({"public_data":value})),
            &[],
        )
        .await;
        assert_eq!(
            invalid.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            invalid.text
        );
    }
    assert_eq!(telegram.count("editMessageWidgetData"), 1);
    let row = call(app, "GET", &format!("/api/posts/{id}"), None, &[]).await;
    assert_eq!(row.value["payload"]["widget"]["public_data"], "keep");
    assert_eq!(row.value["version"], 1);
    Ok(())
}

/// Высокая энтропия: 6000 символов из широкого алфавита сжимаются хуже лимита 4096 байт.
fn incompressible() -> String {
    const ALPHABET: &[char] = &[
        'а', 'б', 'в', 'г', 'д', 'е', 'ж', 'з', 'и', 'к', 'л', 'м', 'н', 'о', 'п', 'р', 'с', 'т',
        'у', 'ф', 'х', 'ц', 'ч', 'ш', 'щ', 'э', 'ю', 'я', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H',
        'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '日', '本', '語', '汉', '字', 'テ', 'ス',
        'ト', 'Φ', 'Ψ', 'Ω', 'α', 'β', 'γ', 'δ', 'ε', 'λ', 'μ', 'π', 'σ', 'φ', 'ψ', 'ω',
    ];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut text = String::with_capacity(6000 * 3);
    for _ in 0..6000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        text.push(ALPHABET[((state >> 33) as usize) % ALPHABET.len()]);
    }
    format!("<div>{text}</div>")
}

#[tokio::test]
async fn widget_limits_are_enforced_before_sending() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));

    // Исходник больше 32 768 байт — ошибка до отправки, Telegram не вызывается.
    let big = format!("<div>{}</div>", "x".repeat(33_000));
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget/validate",
        Some(json!({"widget":{"html":big}})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["ok"], json!(false));
    assert!(reply.value["size_raw"].as_i64() >= Some(33_000));
    assert!(
        reply.value["errors"]
            .as_array()
            .ok_or("errors")?
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("32768"))
    );

    // Несжимаемый исходник: после zlib level 9 больше 4096 — сервер отклонит.
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget/validate",
        Some(json!({"widget":{"html":incompressible()}})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["ok"], json!(false), "{}", reply.text);
    assert!(reply.value["size_compressed"].as_i64() > Some(4096));
    assert!(
        reply.value["errors"]
            .as_array()
            .ok_or("errors")?
            .iter()
            .any(|error| error
                .as_str()
                .unwrap_or_default()
                .contains("INPUT_MEDIA_WIDGET_TOO_LARGE"))
    );

    // chat_data больше 1024 байт.
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget/validate",
        Some(json!({"widget":{"html":DEMO,"chat_data":"x".repeat(1025)}})),
        &[],
    )
    .await;
    assert_eq!(reply.value["ok"], json!(false));
    assert!(
        reply.value["errors"]
            .as_array()
            .ok_or("errors")?
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("chat_data"))
    );

    // Отправка с превышением лимита — 422, Telegram не вызван.
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({"widget":{"html":big},"chat_id":"@test_channel"})),
        &[],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.text
    );
    assert_eq!(telegram.count("sendWidget"), 0);

    // Запрещённые возможности — предупреждения, отправка проходит.
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget/validate",
        Some(json!({"widget":{"html":"<div onclick=\"fetch('/x')\">п</div><audio src=\"x\"></audio>"}})),
        &[],
    )
    .await;
    assert_eq!(reply.value["ok"], json!(true), "{}", reply.text);
    let warnings = reply.value["warnings"].as_array().ok_or("warnings")?;
    assert!(warnings.len() >= 2, "{}", reply.text);
    Ok(())
}

#[tokio::test]
async fn widget_sends_and_is_idempotent() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.queue(
        "sendWidget",
        vec![json!({"ok":true,"result":{"message_id":777,"chat":{"id":-1001},"widget":{"is_shared":false}}})],
    );
    let body = json!({
        "widget":{"html":DEMO,"caption":"счёт","chat_data":"{}","allow_queries":false,"future_field":{"keep":true}},
        "chat_id":"@test_channel",
    });
    let headers = [
        ("idempotency-key", "widget-key-1"),
        ("x-studio-client", "test"),
    ];
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(body.clone()),
        &headers,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = telegram.last("sendWidget").ok_or("sendWidget")?.body;
    assert_eq!(sent["html"], DEMO);
    assert_eq!(sent["caption"], "счёт");
    assert_eq!(sent["chat_data"], "{}");
    assert_eq!(sent["allow_queries"], false);
    assert_eq!(sent["future_field"], json!({"keep":true}));
    assert_eq!(sent["chat_id"], json!("@test_channel"));
    assert!(sent.get("pin").is_none());

    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    let row = rows
        .as_array()
        .ok_or("posts")?
        .first()
        .ok_or("row")?
        .clone();
    assert_eq!(row["kind"], json!("widget"));
    assert_eq!(row["status"], json!("sent"));
    let id = row["id"].as_str().ok_or("id")?.to_owned();
    let full = call(
        app.clone(),
        "GET",
        format!("/api/posts/{id}").as_str(),
        None,
        &[],
    )
    .await
    .value;
    assert_eq!(full["payload"]["widget"]["html"], DEMO);
    assert_eq!(full["payload"]["widget"]["allow_queries"], json!(false));

    // Повтор с тем же ключом не отправляет второй виджет.
    let replay = call(app.clone(), "POST", "/api/widget", Some(body), &headers).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.text);
    assert_eq!(telegram.count("sendWidget"), 1);
    Ok(())
}

#[tokio::test]
async fn widget_refuses_channels() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app_state = state(&dir, &base, &[]);
    let app = build_router(app_state.clone());
    channels::save(
        &context(&app_state),
        &json!({"chat_id":"@test_channel","title":"Тестовый канал","type":"channel"}),
    )?;
    let reply = call(
        app,
        "POST",
        "/api/widget",
        Some(json!({"widget":{"html":DEMO},"chat_id":"@test_channel"})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.value["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("в канал напрямую не отправляются")
    );
    assert_eq!(telegram.count("sendWidget"), 0);
    Ok(())
}

#[tokio::test]
async fn widget_data_edits_the_state_only() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    telegram.queue(
        "sendWidget",
        vec![json!({"ok":true,"result":{"message_id":778,"chat":{"id":-1001},"widget":{"is_shared":false}}})],
    );
    let reply = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({"widget":{"html":DEMO,"chat_data":"{\"n\":1}"},"chat_id":"@test_channel"})),
        &[("x-studio-client", "test")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let id = reply.value["results"][0]["post"]["id"]
        .as_str()
        .ok_or("id")?
        .to_owned();
    telegram.clear_calls();

    // Отправленный: данные уходят в editMessageWidgetData и попадают в payload.
    let reply = call(
        app.clone(),
        "POST",
        format!("/api/posts/{id}/widget-data").as_str(),
        Some(json!({"chat_data":"{\"n\":2}"})),
        &[("x-studio-client", "test")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let call_ = telegram.last("editMessageWidgetData").ok_or("edit")?.body;
    assert_eq!(call_["chat_data"], json!("{\"n\":2}"));
    assert_eq!(call_["message_id"], 778);
    let row = call(
        app.clone(),
        "GET",
        format!("/api/posts/{id}").as_str(),
        None,
        &[],
    )
    .await
    .value;
    assert_eq!(row["payload"]["widget"]["chat_data"], json!("{\"n\":2}"));
    assert_eq!(row["payload"]["widget"]["html"], DEMO, "HTML не меняется");

    // Отложенному виджету данные пишутся в payload без вызова Telegram.
    telegram.clear_calls();
    let scheduled = call(
        app.clone(),
        "POST",
        "/api/widget",
        Some(json!({"widget":{"html":DEMO},"chat_id":"@test_channel","at":4_500_000_000i64})),
        &[("x-studio-client", "test")],
    )
    .await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    let id2 = scheduled.value["scheduled"][0]["id"]
        .as_str()
        .ok_or("id2")?
        .to_owned();
    telegram.clear_calls();
    let reply = call(
        app.clone(),
        "POST",
        format!("/api/posts/{id2}/widget-data").as_str(),
        Some(json!({"public_data":"общее"})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(telegram.count("editMessageWidgetData"), 0);
    let row = call(
        app.clone(),
        "GET",
        format!("/api/posts/{id2}").as_str(),
        None,
        &[],
    )
    .await
    .value;
    assert_eq!(row["payload"]["widget"]["public_data"], json!("общее"));

    // Не-виджет отказывается.
    let rich = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":{"rich_message":{"html":"т"}}})),
        &[],
    )
    .await;
    let rich_id = rich.value["post"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let reply = call(
        app,
        "POST",
        format!("/api/posts/{rich_id}/widget-data").as_str(),
        Some(json!({"chat_data":"x"})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    Ok(())
}

#[tokio::test]
async fn widget_preview_is_an_isolated_page() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let body = "html=%3Cdiv%3E%D0%BF%D1%80%D0%B8%D0%B2%D0%B5%D1%82%3C%2Fdiv%3E&chat_data=%7B%7D"
        .to_owned();
    let reply = call_raw(
        app,
        "POST",
        "/render/widget",
        Some(body.into_bytes()),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let csp = reply
        .headers
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    assert!(csp.contains("'wasm-unsafe-eval'"), "{csp}");
    assert!(csp.contains("worker-src 'self' blob:"), "{csp}");
    assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
    for name in [
        "cross-origin-opener-policy",
        "cross-origin-embedder-policy",
        "cross-origin-resource-policy",
    ] {
        assert!(reply.headers.contains_key(name), "нет {name}");
    }
    assert!(reply.text.contains("widget-source"), "{}", reply.text);
    assert!(reply.text.contains("/static/vendor/widget/host.js"));
    assert!(reply.text.contains("привет"), "{}", reply.text);
    Ok(())
}
