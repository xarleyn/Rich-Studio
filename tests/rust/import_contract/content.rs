use grammers_session::storages::MemorySession;
use grammers_tl_types as tl;
use rich_studio::{
    account_context::AccountContext,
    services::{
        import_web,
        imports::{self, SavedMedia},
    },
    spec::{
        import_content::{Converter, entities_text, plain, representations, text_html},
        source::Spec,
        validator::Validator,
    },
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tempfile::TempDir;

use crate::support::*;

#[test]
fn full_rich_requires_matching_complete_message_in_rpc_response() -> TestResult {
    let response = |message: tl::types::Message| {
        tl::types::messages::Messages {
            messages: vec![message.into()],
            topics: vec![],
            chats: vec![],
            users: vec![],
        }
        .into()
    };
    let full = imports::full_rich_message(response(mt_message(203, false)), 203)?;
    assert_eq!(full.id, 203);
    for message in [mt_message(203, true), mt_message(999, false)] {
        assert_eq!(
            imports::full_rich_message(response(message), 203)
                .err()
                .ok_or("accepted incomplete")?
                .status,
            502
        );
    }
    let mut missing = mt_message(203, false);
    missing.rich_message = None;
    assert_eq!(
        imports::full_rich_message(response(missing), 203)
            .err()
            .ok_or("accepted missing")?
            .status,
        502
    );
    Ok(())
}

#[tokio::test]
async fn partial_rich_message_is_replaced_with_complete_blocks_before_import() -> TestResult {
    let complete = tl::types::Message {
        rich_message: Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![
                    tl::types::PageBlockParagraph {
                        text: tl::types::TextPlain {
                            text: "Complete".into(),
                        }
                        .into(),
                    }
                    .into(),
                ],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        ),
        ..mt_message(203, true)
    };
    let response = tl::types::messages::Messages {
        messages: vec![complete.into()],
        topics: vec![],
        chats: vec![],
        users: vec![],
    }
    .into();
    let complete = imports::full_rich_message(response, 203)?;
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let pool = grammers_client::SenderPool::new(Arc::new(MemorySession::default()), 1);
    let client = grammers_client::Client::new(pool.handle.clone());
    let mut saved = SavedMedia::default();
    let mut warnings = vec![];
    let blocks = tokio::time::timeout(
        Duration::from_millis(200),
        imports::message_blocks(
            &client,
            &context,
            &settings,
            &complete,
            &mut saved,
            &mut warnings,
        ),
    )
    .await??;
    assert_eq!(blocks, vec![json!({"type":"paragraph","text":"Complete"})]);
    assert!(warnings.is_empty(), "{warnings:?}");
    Ok(())
}

#[tokio::test]
async fn protected_ttl_and_partial_messages_fail_before_any_network_read() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let pool = grammers_client::SenderPool::new(Arc::new(MemorySession::default()), 1);
    let client = grammers_client::Client::new(pool.handle.clone());
    // No sender runner is started; an accidental network invocation would never complete.
    let mut protected = mt_message(203, false);
    protected.noforwards = true;
    let mut ttl = mt_message(203, false);
    ttl.ttl_period = Some(60);
    for (message, status) in [(protected, 403), (ttl, 403), (mt_message(203, true), 502)] {
        let mut saved = SavedMedia::default();
        let mut warnings = vec![];
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            imports::message_blocks(
                &client,
                &context,
                &settings,
                &message,
                &mut saved,
                &mut warnings,
            ),
        )
        .await?;
        assert_eq!(result.err().ok_or("accepted protected")?.status, status);
    }
    let mut saved = SavedMedia::default();
    let mut warnings = vec![];
    let result = imports::message_blocks(
        &client,
        &context,
        &settings,
        &mt_message(203, false),
        &mut saved,
        &mut warnings,
    )
    .await?;
    assert_eq!(result[0]["text"], "Full body");
    Ok(())
}

#[test]
fn real_tl_tables_and_empty_caption_are_not_lost_in_enum_normalization() -> TestResult {
    let raw = json!([{"Table":{"title":"TextEmpty","rows":[{"Row":{"cells":[{"Cell":{"header":true,"text":{"TextPlain":{"text":"Value"}},"align_center":false,"align_right":false,"valign_middle":false,"valign_bottom":false,"colspan":null,"rowspan":null}}]}}],"bordered":true,"striped":false,"compact":false}},{"Photo":{"photo_id":42,"caption":{"Caption":{"text":"TextEmpty","credit":"TextEmpty"}},"spoiler":false}}]);
    let typed: Vec<tl::enums::PageBlock> = serde_json::from_value(raw)?;
    let value = imports::normalize_tl(serde_json::to_value(typed)?, "PageBlock");
    let mut converter = Converter::new(BTreeMap::from([(
        "42".into(),
        json!({"type":"photo","media":"upload:a.jpg"}),
    )]));
    let blocks = converter.blocks(value.as_array().ok_or("blocks")?)?;
    assert_eq!(blocks[0]["cells"][0][0]["text"], "Value");
    assert_eq!(blocks[1]["caption"], json!({"text":"","credit":""}));
    Ok(())
}

#[test]
fn entities_preserve_astral_utf16_and_crossing_ranges() -> TestResult {
    let mut python_warnings = Vec::new();
    let python = entities_text(
        "😀bold<script>",
        &[json!({"type":"bold","offset":2,"length":4})],
        &mut python_warnings,
    )?;
    assert_eq!(plain(&python), "😀bold<script>");
    let (python_html, _, _) = representations(
        &[json!({"type":"paragraph","text":python})],
        &mut python_warnings,
    )?;
    assert!(python_html.contains("<b>bold</b>"));
    assert!(python_html.contains("&lt;script&gt;"));
    assert!(
        python_warnings.is_empty(),
        "unexpected warnings: {python_warnings:?}"
    );
    let mut warnings = Vec::new();
    let text = entities_text(
        "A😀<&B",
        &[
            json!({"type":"bold","offset":1,"length":2}),
            json!({"type":"italic","offset":2,"length":2}),
            json!({"type":"text_link","offset":3,"length":2,"url":"javascript:alert(1)"}),
        ],
        &mut warnings,
    )?;
    assert_eq!(plain(&text), "A😀<&B");
    assert_eq!(text_html(&text), "A<b>😀</b>&lt;&amp;B");
    assert_eq!(warnings.len(), 1);
    let crossing = entities_text(
        "abcdef",
        &[
            json!({"type":"bold","offset":0,"length":4}),
            json!({"type":"italic","offset":2,"length":4}),
        ],
        &mut warnings,
    )?;
    assert_eq!(plain(&crossing), "abcdef");
    assert!(text_html(&crossing).contains("<i><b>cd</b></i>"));
    assert!(entities_text(&"a".repeat(100001), &[], &mut warnings).is_err());
    Ok(())
}

#[test]
fn widget_requires_matching_message_and_rejects_placeholders() -> TestResult {
    let body = r#"<div class="tgme_widget_message" data-post="simple_agi/203"><div class="tgme_widget_message_text"><b>A&amp;B</b><script>steal()</script><a href="javascript:evil()">plain</a><br><i>last</i></div><a class="tgme_widget_message_photo_wrap" style="background-image:url('https://cdn.example/a.jpg')"></a><video></video></div>"#;
    let widget = import_web::parse_widget(body, "simple_agi/203")?;
    let html = text_html(&json!(widget.root));
    assert_eq!(html, "<b>A&amp;B</b>plain<br><i>last</i>");
    assert_eq!(widget.media_urls, vec!["https://cdn.example/a.jpg"]);
    assert_eq!(widget.warnings.len(), 1);
    assert_eq!(
        import_web::parse_widget(body, "different/203")
            .err()
            .ok_or("accepted wrong post")?
            .status,
        409
    );
    for body in [
        r#"<div class="tgme_widget_message" data-post="simple_agi/203"><div class="message_media_not_supported">Please view in app</div></div>"#,
        r#"<div class="tgme_widget_message" data-post="simple_agi/203"></div>"#,
    ] {
        let error = import_web::parse_widget(body, "simple_agi/203")
            .err()
            .ok_or("accepted placeholder")?;
        assert_eq!(error.detail["code"], "telegram_connection_required");
    }
    Ok(())
}

#[test]
fn real_tl_rich_constructors_normalize_nested_text_and_media() -> TestResult {
    let blocks: Vec<tl::enums::PageBlock> = vec![
        tl::types::PageBlockHeading1 {
            text: tl::types::TextBold {
                text: tl::types::TextPlain {
                    text: "Rich <title>".into(),
                }
                .into(),
            }
            .into(),
        }
        .into(),
    ];
    let value = imports::normalize_tl(serde_json::to_value(blocks)?, "PageBlock");
    assert_eq!(value[0]["_"], "PageBlockHeading1");
    assert_eq!(value[0]["text"]["_"], "TextBold");
    let mut converter = Converter::default();
    let blocks = converter.blocks(value.as_array().ok_or("blocks")?)?;
    assert_eq!(
        blocks,
        json!([{"type":"heading","size":1,"text":{"type":"bold","text":"Rich <title>"}}])
            .as_array()
            .ok_or("expected blocks")?
            .clone()
    );
    let mut warnings = Vec::new();
    let (html, _, _) = representations(&blocks, &mut warnings)?;
    assert_eq!(html, "<h1><b>Rich &lt;title&gt;</b></h1>");
    Ok(())
}

#[test]
fn converters_keep_nested_rich_blocks_and_unsupported_markers() -> TestResult {
    let source = json!([
        {"_":"PageBlockHeading2","text":{"_":"TextBold","text":{"_":"TextPlain","text":"Heading"}}},
        {"_":"PageBlockDetails","title":{"_":"TextPlain","text":"Details"},"open":true,"blocks":[{"_":"PageBlockParagraph","text":{"_":"TextPlain","text":"inside"}},{"_":"PageBlockPhoto","photo_id":42,"spoiler":true,"caption":{"text":{"_":"TextPlain","text":"Caption"},"credit":{"_":"TextPlain","text":"Author"}}}]},
        {"_":"PageBlockTable","title":{"_":"TextPlain","text":"Table"},"bordered":true,"rows":[{"cells":[{"header":true,"text":{"_":"TextPlain","text":"Cell"}}]}]},
        {"_":"PageBlockFuture"}
    ]);
    let mut converter = Converter::new(BTreeMap::from([(
        "42".into(),
        json!({"type":"photo","media":"upload:photo.jpg"}),
    )]));
    let blocks = converter.blocks(source.as_array().ok_or("blocks")?)?;
    assert_eq!(blocks[0]["text"]["type"], "bold");
    assert_eq!(blocks[1]["blocks"][0]["text"], "inside");
    assert_eq!(blocks[1]["blocks"][1]["photo"]["has_spoiler"], true);
    assert_eq!(blocks[2]["cells"][0][0]["text"], "Cell");
    assert_eq!(converter.warnings.len(), 1);
    assert!(plain(&blocks[3]).contains("PageBlockFuture"));
    let dir = TempDir::new()?;
    let spec = Spec::load_initial(&settings(&dir)?);
    let report = Validator::new(&spec).check(&json!({"rich_message":{"blocks":blocks}}));
    assert_eq!(report["ok"], true, "{report}");
    let mut warnings = Vec::new();
    let (html, markdown, media) = representations(&blocks, &mut warnings)?;
    assert!(html.contains("<details open>"), "{html}");
    assert!(html.contains("<table bordered>"), "{html}");
    assert!(html.contains("tg://photo?id=import1"), "{html}");
    assert_eq!(media[0]["media"]["media"], "upload:photo.jpg");
    assert!(!markdown.is_empty());
    assert!(html.contains("Caption"), "{html}");
    Ok(())
}
