use axum::http::StatusCode;
use rich_studio::build_router;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, state, telegram};

#[tokio::test]
async fn bundled_examples_validate() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let blocks: Value = serde_json::from_str(include_str!("../../../examples/post-blocks.json"))?;
    let html = include_str!("../../../examples/post-showcase.html");
    for post in [
        json!({"rich_message":{"blocks":blocks}}),
        json!({"rich_message":{"html":html}}),
    ] {
        let reply = call(app.clone(), "POST", "/api/validate", Some(post), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], true, "{}", reply.text);
    }
    Ok(())
}

#[tokio::test]
async fn validation_errors_name_the_exact_problem() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let long_html = format!("<p>{}</p>", "я".repeat(33000));
    let cases = [
        (
            json!({"blocks": [{"type": "paragraf", "text": "x"}]}),
            "неизвестный type='paragraf'",
        ),
        (
            json!({"blocks": [{"type": "heading", "size": "2", "text": "x"}]}),
            "ожидалось целое число",
        ),
        (
            json!({"blocks": [{"type": "table"}]}),
            "нет обязательного поля cells",
        ),
        (
            json!({"html": "<img src=\"tg://photo?id=cover\">"}),
            "нет элемента с id='cover'",
        ),
        (
            json!({"html": "<p>a</p>", "markdown": "b"}),
            "ровно одно из html / markdown / blocks",
        ),
        (json!({}), "ровно одно из html / markdown / blocks"),
        (json!({"html": long_html}), "лимит 32768"),
    ];
    for (rich_message, fragment) in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(json!({"rich_message":rich_message})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], false, "{}", reply.text);
        assert!(
            reply.value["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|error| error.contains(fragment)),
            "missing {fragment:?}: {}",
            reply.text
        );
    }
    Ok(())
}

#[tokio::test]
async fn button_links_are_checked_like_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let good = json!({"inline_keyboard":[[
        {"text":"Сайт","url":"https://t.me/telegram"},
        {"text":"Бот","url":"tg://resolve?domain=x"}
    ]]});
    let reply = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":blocks_post()["rich_message"],"reply_markup":good})),
        &[],
    )
    .await;
    assert_eq!(reply.value["ok"], true, "{}", reply.text);

    for url in ["https://", "", "http:///path"] {
        let post = json!({
            "rich_message":blocks_post()["rich_message"],
            "reply_markup":{"inline_keyboard":[[{"text":"Сайт","url":url}]]}
        });
        let checked = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(post.clone()),
            &[],
        )
        .await;
        assert_eq!(checked.value["ok"], false, "{url}: {}", checked.text);
        assert!(
            checked.value["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|error| error.contains("кнопка «Сайт» (ряд 1, №1)")),
            "{url}: {}",
            checked.text
        );
        let sent = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":post})),
            &[],
        )
        .await;
        assert_eq!(sent.status, StatusCode::UNPROCESSABLE_ENTITY, "{url}");
    }
    assert_eq!(telegram.count("sendRichMessage"), 0);

    let rich = json!({"rich_message":{"blocks":[
        {"type":"buttons","buttons":[{"text":"Сайт","url":"https://"}]}
    ]}});
    let warning = call(app.clone(), "POST", "/api/validate", Some(rich), &[]).await;
    assert!(
        warning.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("нет адреса сайта")),
        "{}",
        warning.text
    );

    let mut actionless = json!({
        "rich_message":blocks_post()["rich_message"],
        "reply_markup":{"inline_keyboard":[[{"text":"Пусто","style":"primary"}]]}
    });
    let checked = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(actionless.clone()),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], false, "{}", checked.text);
    assert!(
        checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("не задано, что она делает")),
        "{}",
        checked.text
    );
    actionless["reply_markup"]["inline_keyboard"][0][0]["future_action"] = json!({"x":1});
    let checked = call(app, "POST", "/api/validate", Some(actionless), &[]).await;
    assert!(
        !checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("что она делает")),
        "{}",
        checked.text
    );
    Ok(())
}

#[tokio::test]
async fn malformed_and_deep_blocks_are_reported_not_crashing() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    for rich_message in [
        json!({"blocks": [{"type": "list", "items": ["x"]}]}),
        json!({"blocks": [{"type": "table", "cells": [["a"]]}]}),
        json!({"blocks": "not a list"}),
    ] {
        let reply = call(
            app.clone(),
            "POST",
            "/api/validate",
            Some(json!({"rich_message":rich_message})),
            &[],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.value["ok"], false, "{}", reply.text);
    }

    let mut blocks = Vec::new();
    {
        let mut node = &mut blocks;
        for _ in 0..110 {
            let inner: Vec<Value> = Vec::new();
            node.push(json!({"type":"blockquote","blocks":inner}));
            node = node
                .last_mut()
                .and_then(|block| block.get_mut("blocks"))
                .and_then(Value::as_array_mut)
                .ok_or("nested blocks")?;
        }
    }
    let deep = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":{"blocks":blocks}})),
        &[],
    )
    .await;
    assert_eq!(deep.status, StatusCode::OK, "{}", deep.text);
    assert_eq!(deep.value["ok"], false, "{}", deep.text);
    assert!(
        deep.value["errors"][0]
            .as_str()
            .unwrap_or_default()
            .contains("вложенность"),
        "{}",
        deep.text
    );

    let wrong =
        json!({"blocks":[{"type":"photo","photo":{"type":"video","media":"https://x/y.mp4"}}]});
    let checked = call(
        app.clone(),
        "POST",
        "/api/validate",
        Some(json!({"rich_message":wrong})),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], false);
    assert!(
        checked.value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|error| error.contains("должно быть 'photo'")),
        "{}",
        checked.text
    );
    let valid = json!({
        "html":"<video src='tg://video?id=v'></video>",
        "media":[{"id":"v","media":{"type":"video","media":"https://x/y.mp4","supports_streaming":true}}]
    });
    let checked = call(
        app,
        "POST",
        "/api/validate",
        Some(json!({"rich_message":valid})),
        &[],
    )
    .await;
    assert_eq!(checked.value["ok"], true, "{}", checked.text);
    assert_eq!(checked.value["warnings"], json!([]), "{}", checked.text);
    Ok(())
}
