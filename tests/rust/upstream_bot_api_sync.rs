use std::collections::BTreeMap;

use rich_studio::{
    AppState,
    services::templates::{FillInput, TemplateInput, TemplateService},
    settings::Settings,
    spec::{source::Spec, validator::Validator},
};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn state(dir: &TempDir) -> Result<AppState, Box<dyn std::error::Error>> {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))?;
    Ok(AppState::new(settings))
}
fn spec(dir: &TempDir) -> Result<Spec, Box<dyn std::error::Error>> {
    Ok(Spec::load_initial(&state(dir)?.settings))
}

#[test]
fn rich_limits_count_list_items_table_rows_and_each_text_string() -> TestResult {
    let dir = TempDir::new()?;
    let spec = spec(&dir)?;
    let validator = Validator::new(&spec);
    let list = json!({"rich_message":{"blocks":[{"type":"list","items":[{"blocks":[{"type":"paragraph","text":"a"}]}]}]}});
    let report = validator.check_post(&list);
    assert_eq!(report["stats"]["blocks"], 3, "{report}");
    assert_eq!(report["stats"]["chars"], 1);
    let table =
        json!({"rich_message":{"blocks":[{"type":"table","cells":[[{"text":"a","colspan":21}]]}]}});
    let report = validator.check_post(&table);
    assert_eq!(report["ok"], false);
    assert!(report["errors"].to_string().contains("колонок"));
    assert!(!report["errors"].to_string().contains("align"));
    let source = json!({"rich_message":{"html":"ж".repeat(18000)}});
    let report = validator.check_post(&source);
    assert_eq!(report["ok"], false);
    assert!(report["errors"].to_string().contains("байт"));
    let split = json!({"rich_message":{"blocks":[{"type":"paragraph","text":"ж".repeat(16000)},{"type":"paragraph","text":"ж".repeat(16000)}]}});
    let report = validator.check_post(&split);
    assert_eq!(report["ok"], true, "{report}");
    Ok(())
}

#[test]
fn block_and_html_nesting_limits_include_formatting_and_implicit_paragraphs() -> TestResult {
    let dir = TempDir::new()?;
    let spec = spec(&dir)?;
    let validator = Validator::new(&spec);
    for wrappers in [15, 16] {
        let mut text = json!("x");
        for _ in 0..wrappers {
            text = json!({"type":"bold","text":text});
        }
        let report = validator
            .check_post(&json!({"rich_message":{"blocks":[{"type":"paragraph","text":text}]}}));
        assert_eq!(report["ok"], wrappers == 15, "{report}");
        assert_eq!(report["stats"]["depth"], wrappers + 1);
        let html = format!(
            "<blockquote><p>{}x{}</p></blockquote>",
            "<b>".repeat(wrappers),
            "</b>".repeat(wrappers)
        );
        let report = validator.check_post(&json!({"rich_message":{"html":html}}));
        assert_eq!(report["ok"], wrappers == 15, "{report}");
    }
    let report=validator.check_post(&json!({"rich_message":{"html":format!("{}x{}{}","<b>".repeat(1000),"</i>".repeat(1000),"<p>x".repeat(1000))}}));
    assert_eq!(report["stats"]["depth"], 1001, "{report}");
    for count in [500, 501] {
        let report = validator.check_post(
            &json!({"rich_message":{"blocks":vec![json!({"type":"paragraph","text":"x"});count]}}),
        );
        assert_eq!(report["ok"], count == 500, "{report}");
    }
    Ok(())
}

#[test]
fn entities_and_addresses_match_measured_telegram_behavior() -> TestResult {
    let dir = TempDir::new()?;
    let spec = spec(&dir)?;
    let validator = Validator::new(&spec);
    let report = validator
        .check_post(&json!({"rich_message":{"html":"<p>&#0000000065;&copy;&laquo;&#128;</p>"}}));
    assert_eq!(report["stats"]["chars"], 10, "{report}");
    for html in [
        "<h1>lost &#xD800; text</h1>",
        "<p>lost &#1114112;</p>",
        "<a href=\"https://example.com/&#x2028;\">link</a>",
        "<tg-button url=\"https://example.com/&#160;\">button</tg-button>",
    ] {
        let report = validator.check_post(&json!({"rich_message":{"html":html}}));
        assert_eq!(report["ok"], false, "{html}: {report}");
    }
    let report = validator.check_post(
        &json!({"rich_message":{"html":"<a href=\"https://example.com/\u{2028}\">link</a>"}}),
    );
    assert_eq!(report["ok"], true, "{report}");
    assert!(report["warnings"].to_string().contains("U+2028"));
    let report = validator.check_post(&json!({"rich_message":{"markdown":"```\n&#xD800;\n```"}}));
    assert_eq!(report["ok"], true, "{report}");
    Ok(())
}

#[test]
fn source_lower_bound_never_exceeds_telegram_captured_text() -> TestResult {
    let dir = TempDir::new()?;
    let spec = spec(&dir)?;
    let validator = Validator::new(&spec);
    let cases: Value = serde_json::from_str(include_str!("../telegram_parse/cases.json"))?;
    let captured: Value = serde_json::from_str(include_str!("../telegram_parse/captured.json"))?;
    for case in cases.as_array().ok_or("cases")? {
        let Some(name) = case["name"].as_str() else {
            continue;
        };
        let Some(source) = case["source"].as_str() else {
            continue;
        };
        let Some(format) = case["format"].as_str() else {
            continue;
        };
        let Some(blocks) = captured[name]["blocks"].as_array() else {
            continue;
        };
        let source_report = validator.check_post(&json!({"rich_message":{format:source}}));
        let blocks_report = validator.check_post(&json!({"rich_message":{"blocks":blocks}}));
        let minimum = source_report["stats"]["at_least_chars"]
            .as_u64()
            .ok_or("minimum")?;
        let actual = blocks_report["stats"]["chars"].as_u64().ok_or("actual")?;
        assert!(
            minimum <= actual,
            "{name}: minimum {minimum} > captured {actual}: {source_report}"
        );
    }
    Ok(())
}

#[test]
fn callback_reply_is_validated_without_fixed_destination_or_recipient() -> TestResult {
    let dir = TempDir::new()?;
    let spec = spec(&dir)?;
    let validator = Validator::new(&spec);
    let mut post = json!({"rich_message":{"html":"<p>post</p>"},"reply_markup":{"inline_keyboard":[[{"text":"Answer","callback_data":"answer"}]]},"callbacks":{"answer":{"text":"done","reply":{"rich_message":{"html":"<p>personal</p>"},"ephemeral_message_parameters":{"replace_callback_query_message":true}}}}});
    let report = validator.check_post(&post);
    assert_eq!(report["ok"], true, "{report}");
    for (key, value) in [
        ("chat_id", json!(123)),
        ("allow_paid_broadcast", json!(false)),
        ("business_connection_id", json!("business")),
    ] {
        post["callbacks"]["answer"]["reply"][key] = value;
        let report = validator.check_post(&post);
        assert_eq!(report["ok"], false, "{report}");
        post["callbacks"]["answer"]["reply"]
            .as_object_mut()
            .ok_or("reply")?
            .remove(key);
    }
    post["callbacks"]["answer"]["reply"]["ephemeral_message_parameters"]["receiver_user_id"] =
        json!(42);
    assert_eq!(validator.check_post(&post)["ok"], false);
    Ok(())
}

#[test]
fn chat_restrictions_are_dynamic_and_include_measured_refusals() -> TestResult {
    let dir = TempDir::new()?;
    let mut spec = spec(&dir)?;
    spec.data["types"]["InlineKeyboardButton"]["fields"].as_array_mut().ok_or("fields")?.push(json!({"name":"future_private","types":["String"],"required":false,"description":"Available only in private chats."}));
    let post = json!({"rich_message":{"html":"<p>post</p>"},"reply_markup":{"inline_keyboard":[[{"text":"mini","web_app":{"url":"https://example.com"},"future_private":"x"}]]}});
    let report = Validator::new(&spec).check_post_in(
        &post,
        &[("channel".to_owned(), "в канале «Новости»".to_owned())],
    );
    assert_eq!(report["ok"], true, "{report}");
    let warnings = report["warnings"].to_string();
    assert!(
        warnings.contains("future_private") && warnings.contains("BUTTON_TYPE_INVALID"),
        "{report}"
    );
    let summary = spec.summary();
    assert!(
        summary["chat_rules"]["rules"]
            .get("InlineKeyboardButton.future_private")
            .is_some()
    );
    assert_eq!(
        summary["types"]["InputRichBlockMap"]["fields"]
            .as_array()
            .ok_or("map fields")?
            .iter()
            .find(|field| field["name"] == "zoom")
            .ok_or("zoom")?["range"],
        json!([0, 24])
    );
    Ok(())
}

#[test]
fn template_guards_ambiguous_html_dynamic_urls_and_bound_callbacks() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    for html in [
        "<!-->{{name}}",
        "<p title=\"x\"bad=\"{{name}}\">text</p>",
        "<p title=\"{{name}}",
        "<textarea>{{name}}</textarea>",
        "<a href=\"https://example.com/{{name}}\">go</a>",
    ] {
        let input: TemplateInput = serde_json::from_value(
            json!({"title":"test","post":{"rich_message":{"html":html}},"fields":[{"key":"name","label":"Name","default":"a"}]}),
        )?;
        assert!(service.save(None, input).is_err(), "{html}");
    }
    let input: TemplateInput = serde_json::from_value(
        json!({"title":"test","post":{"rich_message":{"html":"<tg-button data=\"{{name}}\">go</tg-button>"},"callbacks":{"a":"answer"}},"fields":[{"key":"name","label":"Name","default":"a"}]}),
    )?;
    assert!(service.save(None, input).is_err());
    let input: TemplateInput = serde_json::from_value(
        json!({"title":"safe","post":{"rich_message":{"html":"<a href=\"{{link}}\">go</a>"}},"fields":[{"key":"link","label":"Link","type":"url","default":"tg://resolve?domain=example"}]}),
    )?;
    let saved = service.save(None, input)?;
    let id = saved["id"].as_str().ok_or("id")?;
    let fill: FillInput =
        serde_json::from_value(json!({"version":1,"values":{"link":"upload:file.jpg"}}))?;
    assert!(service.create_draft(id, fill).is_err());
    Ok(())
}

#[test]
fn markdown_template_url_destinations_preserve_literal_urls_without_html_entities() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    let source = "[site]({{url}})\n\n![image]({{url}})\n\n[reference]: {{url}}\n\n[title]({{url}} \"Title\")\n\n{{url}}\n\n<a href=\"{{url}}\">HTML</a>";
    let input: TemplateInput = serde_json::from_value(
        json!({"title":"Markdown links","post":{"rich_message":{"markdown":source}},"fields":[{"key":"url","label":"URL","type":"url"}]}),
    )?;
    let template = service.save(None, input)?;
    let url = "https://example.com/`**changed**`(x)?a=1&b=2#part%20one";
    let fill: FillInput = serde_json::from_value(json!({"version":1,"values":{"url":url}}))?;
    let draft = service.create_draft(template["id"].as_str().ok_or("template id")?, fill)?;
    let filled = draft["payload"]["rich_message"]["markdown"]
        .as_str()
        .ok_or("Markdown")?;
    let escaped = "https://example.com/%60%2A%2Achanged%2A%2A%60%28x%29?a=1&b=2#part%20one";
    assert!(
        filled.starts_with(&format!(
            "[site]({escaped})\n\n![image]({escaped})\n\n[reference]: {escaped}"
        )),
        "{filled}"
    );
    // The same value in visible text and a raw HTML attribute retains entity escaping.
    assert!(
        filled.contains(&format!("[title]({escaped} \"Title\")")),
        "{filled}"
    );
    assert!(filled.contains("https://example&#46;com/&#96;&#42;&#42;changed&#42;&#42;&#96;&#40;x&#41;?a&#61;1&#38;b&#61;2&#35;part%20one"),"{filled}");
    assert!(
        filled.contains("<a href=\"https://example&#46;com/"),
        "{filled}"
    );
    for invalid in [
        "[site](https://example.com/{{url}})",
        "[site]({{url}}/tail)",
    ] {
        let input: TemplateInput = serde_json::from_value(
            json!({"title":"Partial destination","post":{"rich_message":{"markdown":invalid}},"fields":[{"key":"url","label":"URL","type":"url"}]}),
        )?;
        assert!(service.save(None, input).is_err(), "{invalid}");
    }
    Ok(())
}

async fn rpc(
    app: axum::Router,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-06-18")
                .body(axum::body::Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
                ))?,
        )
        .await?;
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn mcp_skill_resource_and_prompt_return_the_same_bundled_instruction() -> TestResult {
    let dir = TempDir::new()?;
    let app = rich_studio::build_router(state(&dir)?);
    let listed = rpc(app.clone(), "resources/list", json!({})).await?;
    assert!(
        listed["result"]["resources"]
            .as_array()
            .ok_or("resources")?
            .iter()
            .any(|resource| resource["uri"] == "skill://telegram-rich-posts/SKILL.md")
    );
    let resource = rpc(
        app.clone(),
        "resources/read",
        json!({"uri":"skill://telegram-rich-posts/SKILL.md"}),
    )
    .await?;
    assert_eq!(
        resource["result"]["contents"][0]["mimeType"],
        "text/markdown"
    );
    let prompt = rpc(
        app.clone(),
        "prompts/get",
        json!({"name":"telegram-rich-posts"}),
    )
    .await?;
    assert_eq!(
        resource["result"]["contents"][0]["text"],
        prompt["result"]["messages"][0]["content"]["text"]
    );
    assert_eq!(
        resource["result"]["contents"][0]["text"].as_str(),
        Some(include_str!(
            "../../.claude/skills/telegram-rich-posts/SKILL.md"
        ))
    );
    let listed = rpc(app, "prompts/list", json!({})).await?;
    assert_eq!(
        listed["result"]["prompts"][0]["name"],
        "telegram-rich-posts"
    );
    Ok(())
}

#[derive(Clone, Default)]
struct CallbackMock {
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    mode: u8,
}

#[tokio::test]
async fn rich_callback_reply_precedes_popup_and_refused_reply_is_not_retried() -> TestResult {
    use axum::{
        Json,
        extract::{Path, State},
        routing::post,
    };
    use rich_studio::{services::updates, store::posts};
    for mode in 0..=4 {
        let mock = CallbackMock {
            mode,
            ..Default::default()
        };
        let copy = mock.clone();
        let app=axum::Router::new().route("/bot1:fixture/{method}",post(async |State(mock):State<CallbackMock>,Path(method):Path<String>,Json(body):Json<Value>|{
            mock.calls.lock().unwrap_or_else(|error|panic!("calls: {error}")).push((method.clone(),body));
            if mock.mode==4&&method=="sendRichMessage"{tokio::time::sleep(std::time::Duration::from_secs(5)).await;}
            if mock.mode==1&&method=="sendRichMessage"{Json(json!({"ok":false,"error_code":429,"description":"slow down callback-api-key-fixture callback-import-secret-fixture","parameters":{"retry_after":1}}))}else{Json(json!({"ok":true,"result":{"message_id":99}}))}
        })).with_state(copy);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let dir = TempDir::new()?;
        let mut settings = (*state(&dir)?.settings).clone();
        settings.api_base = format!("http://{address}");
        settings.bot_token = "1:fixture".to_owned();
        settings.preview_chat = "11".to_owned();
        settings.api_key = "callback-api-key-fixture".to_owned();
        settings.import_api_hash = "callback-import-secret-fixture".to_owned();
        let state = AppState::new(settings);
        let mut payload = json!({"rich_message":{"html":"post"},"callbacks":{"answer":{"text":"popup","reply":{"rich_message":{"html":"personal"},"future_parameter":true,"ephemeral_message_parameters":{"replace_callback_query_message":true}}}}});
        if mode == 2 {
            payload["callbacks"]["answer"]["reply"]["rich_message"] = json!({"blocks":[{"type":"callback-api-key-fixture callback-import-secret-fixture"}]});
        }
        let saved = posts::add(
            &state.account,
            posts::NewPost {
                chat_id: &json!(11),
                payload: &payload,
                draft_id: None,
                status: "sent",
                message_id: Some(7),
                scheduled_at: None,
                error: None,
                kind: "rich",
                actor: "test",
            },
        )?;
        let id = saved["id"].as_str().ok_or("id")?;
        posts::update_delivery(&state.account, id, Some(11), None, None)?;
        if mode == 3 {
            rich_studio::store::db::Store::connect(&state.account)?
                .execute("update posts set clicks='broken' where id=?", [id])?;
        }
        let result=updates::handle_update(&state,&json!({"update_id":1,"callback_query":{"id":"cq1","from":{"id":11},"message":{"chat":{"id":11,"type":"private"},"message_id":7},"data":"answer"}})).await?;
        assert_eq!(result, Some("callback"));
        let calls = mock
            .calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone();
        if mode == 2 || mode == 3 {
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0].0, "answerCallbackQuery");
            if mode == 2 {
                let saved = posts::get(&state.account, id)?.ok_or("saved post")?;
                assert_eq!(saved["reply_errors"]["answer"]["n"], 1);
                let error = saved["reply_errors"]["answer"]["error"]
                    .as_str()
                    .ok_or("reply error")?;
                assert!(
                    !error.contains("callback-api-key-fixture"),
                    "local validation error exposed API key"
                );
                assert!(
                    !error.contains("callback-import-secret-fixture"),
                    "local validation error exposed import hash"
                );
            }
            server.abort();
            continue;
        }
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].0, "sendRichMessage");
        assert_eq!(calls[1].0, "answerCallbackQuery");
        assert_eq!(
            calls[0].1["ephemeral_message_parameters"]["callback_query_id"],
            "cq1"
        );
        assert_eq!(
            calls[0].1["ephemeral_message_parameters"]["replace_callback_query_message"],
            true
        );
        assert_eq!(calls[0].1["future_parameter"], true);
        assert!(calls[0].1.get("chat_id").is_none());
        assert_eq!(calls[1].1["text"], "popup");
        assert!(calls[1].1.get("reply").is_none());
        let saved = posts::get(&state.account, id)?.ok_or("saved post")?;
        assert_eq!(saved["clicks"]["answer"], 1);
        if mode == 1 || mode == 4 {
            assert_eq!(saved["reply_errors"]["answer"]["n"], 1);
            let error = saved["reply_errors"]["answer"]["error"]
                .as_str()
                .ok_or("reply error")?;
            assert!(
                !error.contains("callback-api-key-fixture"),
                "stored reply error exposed API key"
            );
            assert!(
                !error.contains("callback-import-secret-fixture"),
                "stored reply error exposed import hash"
            );
        }
        server.abort();
    }
    Ok(())
}
