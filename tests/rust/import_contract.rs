//! Offline parity and failure-boundary tests: never connects to Telegram.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use futures_util::{StreamExt, stream};
use grammers_session::{Session, storages::MemorySession};
use grammers_tl_types as tl;
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    errors::ImportError,
    mcp_tools::tools::imports as import_tools,
    services::{
        accounts, import_forward,
        import_links::parse_link,
        import_web,
        imports::{self, SavedMedia},
        media::{MediaError, MediaService, public_address},
        updates,
    },
    settings::Settings,
    spec::{
        import_content::{Converter, entities_text, plain, representations, text_html},
        source::Spec,
        validator::Validator,
    },
    store::{auth, db, drafts, import_inbox},
    telegram::import_client,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn settings(dir: &TempDir) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([
        ("DATA_DIR".into(), dir.path().to_string_lossy().into_owned()),
        ("SPEC_PIN".into(), "on".into()),
        ("TG_UPDATES".into(), "off".into()),
    ]))?)
}
fn session(ipv6: bool) -> String {
    let mut raw = vec![2];
    if ipv6 {
        raw.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    } else {
        raw.extend_from_slice(&[149, 154, 167, 51]);
    }
    raw.extend_from_slice(&443u16.to_be_bytes());
    raw.extend_from_slice(&[0x42; 256]);
    format!("1{}", URL_SAFE.encode(raw))
}
fn link() -> Result<rich_studio::services::import_links::MessageLink, ImportError> {
    parse_link("https://t.me/simple_agi/203")
}

fn mt_message(id: i32, part: bool) -> tl::types::Message {
    tl::types::Message {
        out: false,
        mentioned: false,
        media_unread: false,
        silent: false,
        post: false,
        from_scheduled: false,
        legacy: false,
        edit_hide: false,
        pinned: false,
        noforwards: false,
        invert_media: false,
        offline: false,
        video_processing_pending: false,
        paid_suggested_post_stars: false,
        paid_suggested_post_ton: false,
        id,
        from_id: None,
        from_boosts_applied: None,
        from_rank: None,
        peer_id: tl::types::PeerChannel { channel_id: 123 }.into(),
        saved_peer_id: None,
        fwd_from: None,
        via_bot_id: None,
        via_business_bot_id: None,
        guestchat_via_from: None,
        reply_to: None,
        date: 0,
        message: String::new(),
        media: None,
        reply_markup: None,
        entities: None,
        views: None,
        forwards: None,
        replies: None,
        edit_date: None,
        post_author: None,
        grouped_id: None,
        reactions: None,
        restriction_reason: None,
        ttl_period: None,
        quick_reply_shortcut_id: None,
        effect: None,
        factcheck: None,
        report_delivery_until_date: None,
        paid_message_stars: None,
        suggested_post: None,
        schedule_repeat_period: None,
        summary_from_language: None,
        rich_message: Some(
            tl::types::RichMessage {
                rtl: false,
                part,
                blocks: vec![
                    tl::types::PageBlockParagraph {
                        text: tl::types::TextPlain {
                            text: "Full body".into(),
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
    }
}

#[derive(Clone, Default)]
struct BotCalls(Arc<Mutex<Vec<(String, Value)>>>);
impl BotCalls {
    fn all(&self) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap_or_else(|error| panic!("lock: {error}"))
            .clone()
    }
}

async fn bot_mock()
-> Result<(String, BotCalls, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let calls = BotCalls::default();
    let app = axum::Router::new()
        .route(
            "/bot1:fixture/{method}",
            post(
                async |State(calls): State<BotCalls>,
                       Path(method): Path<String>,
                       axum::Json(body): axum::Json<Value>| {
                    calls
                        .0
                        .lock()
                        .unwrap_or_else(|error| panic!("lock: {error}"))
                        .push((method, body));
                    (
                        StatusCode::OK,
                        axum::Json(json!({"ok":true,"result":{"message_id":1}})),
                    )
                },
            ),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), calls, task))
}

/// Fill an account database up to its configured page limit, leaving only `reserve` pages.
/// The import under test must be large enough to require more than that, while the ticket's
/// short error update still fits.
fn fill_account_database(
    context: &AccountContext,
    reserve: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = db::Store::connect(context)?;
    let page_size: i64 = connection.query_row("pragma page_size", [], |row| row.get(0))?;
    let max: i64 = connection.query_row("pragma max_page_count", [], |row| row.get(0))?;
    for round in 0..1024 {
        let current: i64 = connection.query_row("pragma page_count", [], |row| row.get(0))?;
        let free: i64 = connection.query_row("pragma freelist_count", [], |row| row.get(0))?;
        let room = (max - current + free - reserve) * page_size;
        if room <= page_size {
            break;
        }
        let mut size = room - page_size;
        loop {
            let key = format!("import-filler-{round}");
            let value = "x".repeat(usize::try_from(size)?);
            match connection.execute("insert into meta(key,value) values(?1,?2)", (&key, &value)) {
                Ok(_) => break,
                Err(_) if size > page_size => size -= page_size,
                Err(error) => return Err(error.into()),
            }
        }
    }
    let current: i64 = connection.query_row("pragma page_count", [], |row| row.get(0))?;
    assert!(
        max - current <= reserve + 1,
        "database not filled: {current}/{max} pages"
    );
    Ok(())
}

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
fn flood_wait_surfaces_retry_delay_without_automatic_sleep() -> TestResult {
    let rpc = grammers_client::sender::RpcError::from(tl::types::RpcError {
        error_code: 420,
        error_message: "FLOOD_WAIT_31".into(),
    });
    let error = import_client::rpc_error(grammers_client::InvocationError::Rpc(rpc));
    assert_eq!(error.status, 429);
    assert!(error.to_string().contains("31"));
    use grammers_client::sender::RetryPolicy;
    let policy = grammers_client::sender::NoRetries;
    let context = grammers_client::sender::RetryContext {
        fail_count: std::num::NonZeroU32::new(1).ok_or("count")?,
        slept_so_far: Duration::ZERO,
        error: grammers_client::InvocationError::Rpc(grammers_client::sender::RpcError::from(
            tl::types::RpcError {
                error_code: 420,
                error_message: "FLOOD_WAIT_31".into(),
            },
        )),
    };
    assert!(policy.should_retry(&context).is_break());
    Ok(())
}

#[test]
fn strict_reference_matches_shared_python_corpus() -> TestResult {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../import_links.json"))?;
    for case in cases {
        let url = case["url"].as_str().ok_or("url missing")?;
        let result = parse_link(url);
        assert_eq!(result.is_ok(), case["valid"] == true, "{url}");
        if let Ok(link) = result {
            assert_eq!(json!(link.message_id), case["message_id"]);
            assert_eq!(json!(link.topic_id), case["topic_id"]);
            assert_eq!(parse_link(&link.url())?, link, "{url}");
        }
    }
    Ok(())
}

#[test]
fn telethon_sessions_decode_both_address_families_round_trip_without_key_debug() -> TestResult {
    for ipv6 in [false, true] {
        let encoded = session(ipv6);
        let (data, is_ipv6) = import_client::decode_session(&encoded)?;
        assert_eq!(data.home_dc, 2);
        assert_eq!(is_ipv6, ipv6);
        let storage = MemorySession::from(data);
        assert_eq!(import_client::encode_session(&storage, ipv6)?, encoded);
        assert_eq!(
            storage.dc_option(2)?.ok_or("missing dc")?.auth_key,
            Some([0x42; 256])
        );
    }
    for broken in [
        String::new(),
        "2bad".into(),
        "1bad".into(),
        format!("1{}", URL_SAFE.encode(vec![0; 279])),
    ] {
        assert_eq!(
            import_client::decode_session(&broken)
                .err()
                .ok_or("accepted invalid")?
                .status,
            409
        );
    }
    Ok(())
}

#[test]
fn session_identity_requires_account_user_and_bot_only_for_shared_transport() -> TestResult {
    let cross = import_client::check_identity(true, Some(111), 999, false)
        .err()
        .ok_or("cross uid")?;
    assert_eq!(cross.status, 403);
    assert!(cross.to_string().contains("другой учётке"), "{cross}");
    assert!(import_client::check_identity(true, Some(111), 111, true).is_err());
    assert!(import_client::check_identity(false, None, 111, false).is_err());
    import_client::check_identity(true, Some(111), 111, false)?;
    import_client::check_identity(false, None, 111, true)?;
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let owner = AccountContext::root(&settings);
    std::fs::create_dir_all(&settings.data_dir)?;
    std::fs::write(imports::session_path(&settings, &owner), session(false))?;
    assert_eq!(imports::status(&settings, &owner)["own_session"], true);
    for uid in [111, 222] {
        let account = AccountContext::account(&settings, uid, Role::Owner);
        assert_eq!(imports::status(&settings, &account)["own_session"], false);
        assert_ne!(
            imports::session_path(&settings, &owner),
            imports::session_path(&settings, &account)
        );
    }
    Ok(())
}

#[test]
fn proxies_decode_credentials_defaults_and_never_print_them() -> TestResult {
    for (scheme, port) in [("socks5", 1080), ("http", 8080)] {
        let config = import_client::proxy_config(&format!("{scheme}://u%40ser:p%3Ass@127.0.0.1"))?
            .ok_or("missing proxy")?;
        assert_eq!(config.port, port);
        assert_eq!(config.username.as_deref(), Some("u@ser"));
        assert_eq!(config.password.as_deref(), Some("p:ss"));
        let debug = format!("{config:?}");
        assert!(!debug.contains("u@ser") && !debug.contains("p:ss"));
    }
    assert!(import_client::proxy_config("file:///secret").is_err());
    Ok(())
}

#[tokio::test]
async fn http_connect_proxy_uses_decoded_auth_and_preserves_tunnel_bytes() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(socket.read_u8().await?);
        }
        let header = String::from_utf8(header).map_err(std::io::Error::other)?;
        assert!(header.starts_with("CONNECT 149.154.167.51:443 HTTP/1.1\r\n"));
        assert!(header.contains("Proxy-Authorization: Basic dUBzZXI6cDpzcw=="));
        socket
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nhello")
            .await?;
        Ok::<_, std::io::Error>(())
    });
    let config = import_client::proxy_config(&format!("http://u%40ser:p%3Ass@{address}"))?
        .ok_or("missing proxy")?;
    let mut socket = import_client::connect_proxy(&config, "149.154.167.51", 443).await?;
    let mut data = [0; 5];
    socket.read_exact(&mut data).await?;
    assert_eq!(&data, b"hello");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn socks_proxy_auth_and_remote_dns_are_preserved() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut header = [0; 3];
        socket.read_exact(&mut header).await?;
        assert_eq!(header, [5, 1, 2]);
        socket.write_all(&[5, 2]).await?;
        assert_eq!(socket.read_u8().await?, 1);
        let n = socket.read_u8().await?;
        let mut user = vec![0; usize::from(n)];
        socket.read_exact(&mut user).await?;
        assert_eq!(user, b"u@ser");
        let n = socket.read_u8().await?;
        let mut password = vec![0; usize::from(n)];
        socket.read_exact(&mut password).await?;
        assert_eq!(password, b"p:ss");
        socket.write_all(&[1, 0]).await?;
        let mut request = [0; 4];
        socket.read_exact(&mut request).await?;
        assert_eq!(request, [5, 1, 0, 3]);
        let n = socket.read_u8().await?;
        let mut host = vec![0; usize::from(n)];
        socket.read_exact(&mut host).await?;
        assert_eq!(host, b"dc.example.invalid");
        assert_eq!(socket.read_u16().await?, 443);
        socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        socket.write_all(b"ok").await?;
        Ok::<_, std::io::Error>(())
    });
    let config = import_client::proxy_config(&format!("socks5://u%40ser:p%3Ass@{address}"))?
        .ok_or("missing proxy")?;
    let mut socket = import_client::connect_proxy(&config, "dc.example.invalid", 443).await?;
    let mut data = [0; 2];
    socket.read_exact(&mut data).await?;
    assert_eq!(&data, b"ok");
    server.await??;
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

#[test]
fn saved_result_creates_new_unpublished_draft_with_all_editor_buffers() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    db::migrate(&mut db::Store::connect(&context)?)?;
    let link = link()?;
    let existing = drafts::save(
        &context,
        None,
        "Existing",
        &json!({"rich_message":{"html":"Keep"}}),
        None,
        None,
        None,
    )?
    .ok_or("existing draft")?;
    let one = imports::save_result(
        &context,
        vec![
            json!({"type":"heading","size":2,"text":"Title"}),
            json!({"type":"paragraph","text":"Body"}),
        ],
        vec![],
        &link,
        "mtproto",
        "",
    )?;
    let one_id = one["draft"]["id"].as_str().ok_or("draft id")?;
    let existing_id = existing["id"].as_str().ok_or("existing id")?;
    assert_ne!(one_id, existing_id);
    assert_eq!(
        drafts::get(&context, existing_id)?.ok_or("existing gone")?["payload"]["rich_message"]["html"],
        "Keep"
    );
    assert_eq!(one["draft"]["payload"]["options"], json!({}));
    let buffers = &one["draft"]["payload"]["_studio"]["buffers"];
    assert!(
        buffers["html"]
            .as_str()
            .unwrap_or_default()
            .contains("<h2>Title</h2>")
    );
    assert!(
        buffers["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("## Title")
    );
    let two = imports::save_result(
        &context,
        vec![
            json!({"type":"photo","photo":{"type":"photo","media":"upload:photo.jpg"}}),
            json!({"type":"paragraph","text":"Second"}),
        ],
        vec!["Inspect".into(), "Inspect".into()],
        &link,
        "mtproto",
        "",
    )?;
    assert_ne!(one["draft"]["id"], two["draft"]["id"]);
    assert_eq!(drafts::list(&context)?.len(), 3);
    assert_eq!(two["warnings"], json!(["Inspect"]));
    assert_eq!(two["media_count"], 1);
    let payload = &two["draft"]["payload"];
    assert_eq!(payload["options"], json!({}));
    assert!(two["draft"]["post_id"].is_null());
    assert!(two["draft"]["chat_ids"].is_null());
    assert_eq!(payload["_studio"]["format"], "blocks");
    assert!(payload["_studio"]["buffers"]["html"].is_string());
    assert!(payload["_studio"]["buffers"]["markdown"].is_string());
    let count: i64 =
        db::Store::connect(&context)?
            .query_row("select count(*) from posts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn downloads_enforce_limits_and_clean_completed_and_partial_files_on_failure() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    settings.max_upload_mb = 0.00001;
    {
        let mut saved = SavedMedia::default();
        let chunks = stream::iter(vec![Ok(vec![1; 5]), Ok(vec![2; 100])]);
        let error =
            imports::save_download(&context, &settings, &mut saved, "photo", None, 0, chunks)
                .await
                .err()
                .ok_or("accepted large stream")?;
        assert_eq!(error.status, 413);
        assert_eq!(saved.count(), 0);
    }
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    {
        let mut saved = SavedMedia::default();
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            5,
            stream::iter(vec![Ok(vec![1; 5])]),
        )
        .await?;
        let result = imports::save_download(
            &context,
            &settings,
            &mut saved,
            "document",
            Some("../../danger.exe"),
            0,
            stream::iter(vec![Err(ImportError::new(502, "network"))]),
        )
        .await;
        assert!(result.is_err());
    }
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn failed_draft_save_cleans_downloaded_media_and_success_keeps_it() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::account(&settings, 123, Role::Drafts);
    db::migrate(&mut db::Store::connect(&context)?)?;
    let connection = db::Store::connect(&context)?;
    connection.execute("with recursive seq(n) as (select 1 union all select n+1 from seq where n<1000) insert into drafts(id,title,payload,created_at,updated_at) select 'seed-'||n,'','{}',0,0 from seq",[])?;
    let mut saved = SavedMedia::default();
    let photo = imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![1; 5])]),
    )
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    let error = imports::save_import(
        context.clone(),
        vec![json!({"type":"photo","photo":photo})],
        vec![],
        link()?,
        "mtproto",
        saved,
    )
    .await
    .err()
    .ok_or("draft quota ignored")?;
    assert_eq!(error.status, 413);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    assert_eq!(drafts::list(&context)?.len(), 1000);

    connection.execute("delete from drafts where id='seed-1'", [])?;
    let mut saved = SavedMedia::default();
    let photo = imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![2; 5])]),
    )
    .await?;
    let result = imports::save_import(
        context.clone(),
        vec![json!({"type":"photo","photo":photo})],
        vec![],
        link()?,
        "mtproto",
        saved,
    )
    .await?;
    assert_eq!(result["media_count"], 1);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    assert_eq!(drafts::list(&context)?.len(), 1000);
    Ok(())
}

#[tokio::test]
async fn failed_import_rolls_back_downloaded_files_and_never_saves_a_draft() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let mut saved = SavedMedia::default();
    imports::save_download(
        &context,
        &settings,
        &mut saved,
        "photo",
        None,
        5,
        stream::iter(vec![Ok(vec![1; 5])]),
    )
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 1);
    let error = imports::save_import(context.clone(), vec![], vec![], link()?, "mtproto", saved)
        .await
        .err()
        .ok_or("empty import accepted")?;
    assert_eq!(error.status, 422);
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    db::migrate(&mut db::Store::connect(&context)?)?;
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn cancellation_cleans_download_and_releases_active_account() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let signal = Arc::new(tokio::sync::Notify::new());
    let notified = signal.clone();
    let context2 = context.clone();
    let task = tokio::spawn(async move {
        let _active = imports::ActiveImport::acquire(Some(991))?;
        let mut saved = SavedMedia::default();
        let chunks = stream::once(async move {
            notified.notify_one();
            Ok(vec![1; 5])
        })
        .chain(stream::pending());
        imports::save_download(&context2, &settings, &mut saved, "photo", None, 0, chunks).await
    });
    tokio::time::timeout(Duration::from_secs(3), signal.notified()).await?;
    task.abort();
    let _ = task.await;
    // An already-running blocking write cannot be aborted. Its owned guard must remove the
    // file as soon as that bounded write returns, even after the request future is gone.
    tokio::time::timeout(Duration::from_secs(2), async {
        while std::fs::read_dir(&context.media_dir).is_ok_and(|entries| entries.count() > 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    let _again = imports::ActiveImport::acquire(Some(991))?;
    Ok(())
}

#[tokio::test]
async fn download_count_total_and_account_quota_boundaries() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    let context = AccountContext::account(&settings, 123, Role::Owner);
    settings.account_storage_mb = 0.00001;
    {
        let mut saved = SavedMedia::default();
        let error = imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            0,
            stream::iter(vec![Ok(vec![1; 20])]),
        )
        .await
        .err()
        .ok_or("quota ignored")?;
        assert_eq!(error.status, 413);
    }
    settings.account_storage_mb = 1.0;
    let mut saved = SavedMedia::default();
    for _ in 0..30 {
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            1,
            stream::iter(vec![Ok(vec![1])]),
        )
        .await?;
    }
    assert_eq!(
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            1,
            stream::iter(vec![Ok(vec![1])])
        )
        .await
        .err()
        .ok_or("31 files")?
        .status,
        413
    );
    drop(saved);
    let mut saved = SavedMedia::default();
    saved.total = imports::MAX_TOTAL_BYTES - 2;
    assert_eq!(
        imports::save_download(
            &context,
            &settings,
            &mut saved,
            "photo",
            None,
            3,
            stream::iter(vec![Ok(vec![1; 3])])
        )
        .await
        .err()
        .ok_or("total ignored")?
        .status,
        413
    );
    Ok(())
}

#[tokio::test]
async fn forward_requires_account_local_ticket_and_is_single_use() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.preview_chat = "777".into();
    settings.bot_username = "fixture_bot".into();
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let mut update = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Forwarded","forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    assert!(
        import_inbox::get(
            &AccountContext::account(&settings, 999, Role::Owner),
            id,
            auth::now()
        )?
        .is_none()
    );
    let arm = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(import_forward::receive(&settings, &arm, &spec).await?);
    update["message"]["from"]["id"] = json!(888);
    update["message"]["chat"]["id"] = json!(888);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    update["message"]["from"]["id"] = json!(777);
    update["message"]["chat"]["id"] = json!(777);
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    let result = import_forward::get(&context, id)?;
    assert_eq!(result["state"], "done");
    assert_eq!(drafts::list(&context)?.len(), 1);
    let other = AccountContext::account(&settings, 999, Role::Owner);
    db::migrate(&mut db::Store::connect(&other)?)?;
    assert!(drafts::list(&other)?.is_empty());
    assert_eq!(result["result"]["source"]["provider"], "forward");
    Ok(())
}

#[tokio::test]
async fn wrong_forward_consumes_ticket_with_error_without_draft() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.updates_on = true;
    settings.bot_token = "fixture".into();
    settings.preview_chat = "777".into();
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    assert!(import_inbox::arm(&context, id, 777, auth::now())?);
    assert!(import_forward::receive(&settings,&json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Wrong","forward_origin":{"type":"channel","chat":{"username":"different"},"message_id":203}}}),&spec).await?);
    assert_eq!(import_forward::get(&context, id)?["state"], "failed");
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[test]
fn forward_rich_actions_removed_unknown_fields_and_largest_photo_preserved() -> TestResult {
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let context = AccountContext::root(&settings);
    let spec = Spec::load_initial(&settings);
    let result = imports::from_forward(
        &context,
        &spec,
        &json!({"rich_message":{"blocks":[{"type":"buttons","buttons":[{"type":"button","callback_data":"evil"}]}]}}),
        &link()?,
        vec!["Origin not verified".into()],
    )?;
    assert_eq!(result["warnings"][0], "Origin not verified");
    let saved = drafts::get(&context, result["draft"]["id"].as_str().ok_or("draft id")?)?
        .ok_or("saved draft")?;
    assert_eq!(
        saved["payload"]["_studio"]["import_warnings"][0],
        "Origin not verified"
    );
    assert_eq!(
        result["draft"]["payload"]["rich_message"]["blocks"][0]["type"],
        "paragraph"
    );
    assert!(!result.to_string().contains("evil"));
    let photo = imports::from_forward(
        &context,
        &spec,
        &json!({"photo":[{"file_id":"small","width":1,"height":1},{"file_id":"large","width":10,"height":10}],"caption":"Caption"}),
        &link()?,
        vec![],
    )?;
    assert_eq!(
        photo["draft"]["payload"]["rich_message"]["blocks"][0]["photo"]["media"],
        "large"
    );
    Ok(())
}

#[tokio::test]
async fn private_link_without_session_and_invalid_link_fail_before_network() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.import_api_id = 1;
    settings.import_api_hash = "fixture".into();
    settings.import_proxy = "socks5://127.0.0.1:1".into();
    let context = AccountContext::account(&settings, 111, Role::Owner);
    let private = imports::import_post(&settings, &context, "https://t.me/c/12345/203")
        .await
        .err()
        .ok_or("private succeeded")?;
    assert_eq!(private.status, 403);
    assert!(private.to_string().contains("собственн"), "{private}");
    assert_eq!(
        imports::import_post(&settings, &context, "http://127.0.0.1/secret")
            .await
            .err()
            .ok_or("invalid succeeded")?
            .status,
        422
    );
    Ok(())
}

#[tokio::test]
async fn http_status_and_input_contract_work_behind_real_security_guard() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    let app = rich_studio::api::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/imports/telegram")
                .header("host", "localhost")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await?)?;
    assert_eq!(body["mtproto_configured"], false);
    assert_eq!(body["own_session"], false);
    for body in [
        json!({"url":"https://t.me.evil.org/a/1"}),
        json!({"url":"https://evil.example/a"}),
        json!({}),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/imports/telegram")
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    Ok(())
}

#[tokio::test]
async fn import_requires_draft_permission() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    auth::add_token(&state.settings, "reader", "read-secret", "read")?;
    let app = rich_studio::build_router(state);
    let request = Request::builder()
        .uri("/api/imports/telegram")
        .header("host", "localhost")
        .header("authorization", "Bearer read-secret")
        .body(Body::empty())?;
    assert_eq!(app.clone().oneshot(request).await?.status(), StatusCode::OK);
    for uri in ["/api/imports/telegram", "/api/imports/forward"] {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("authorization", "Bearer read-secret")
            .body(Body::from(
                json!({"url":"https://t.me/simple_agi/203"}).to_string(),
            ))?;
        assert_eq!(
            app.clone().oneshot(request).await?.status(),
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_import_uses_the_same_service_and_stays_unpublished() -> TestResult {
    let dir = TempDir::new()?;
    let state = AppState::new(settings(&dir)?);
    assert_eq!(
        import_tools::call(state.clone(), "telegram_import_status", json!({})).await?,
        imports::status(&state.settings, &state.account)
    );
    // A live MTProto/widget read is the only success path and needs Telegram. The offline
    // contract is that MCP returns exactly the service validation result and creates nothing.
    for url in ["https://t.me/c/12345/203", "https://t.me.evil.org/a/1"] {
        let expected = imports::import_post(&state.settings, &state.account, url)
            .await
            .err()
            .ok_or("service accepted")?
            .to_string();
        let error = import_tools::call(state.clone(), "import_telegram_post", json!({"url":url}))
            .await
            .err()
            .ok_or("mcp accepted")?;
        assert_eq!(error, expected, "{url}");
    }
    db::migrate(&mut db::Store::connect(&state.account)?)?;
    assert!(drafts::list(&state.account)?.is_empty());
    let count: i64 =
        db::Store::connect(&state.account)?
            .query_row("select count(*) from posts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn import_media_always_denies_private_addresses() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.media_fetch_private = true;
    assert!(settings.media_fetch_private);
    assert!(public_address(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], "public.example").is_err());
    let context = AccountContext::root(&settings);
    let service = MediaService::new(context.clone(), 50.0, 500.0);
    let error = service
        .fetch("http://127.0.0.1/secret", None, true, true)
        .await
        .err()
        .ok_or("accepted private address")?;
    assert!(matches!(error, MediaError::BadRequest(_)), "{error}");
    assert_eq!(std::fs::read_dir(&context.media_dir)?.count(), 0);
    Ok(())
}

#[test]
fn import_configuration_is_optional_and_validated() -> TestResult {
    let dir = TempDir::new()?;
    let base = || BTreeMap::from([("DATA_DIR".into(), dir.path().to_string_lossy().into_owned())]);
    let optional = Settings::from_map(&base())?;
    assert_eq!(optional.import_api_id, 0);
    assert!(optional.import_api_hash.is_empty());
    let mut id_only = base();
    id_only.insert("TG_IMPORT_API_ID".into(), "123".into());
    let error = Settings::from_map(&id_only)
        .err()
        .ok_or("accepted import id without hash")?;
    assert!(error.to_string().contains("задаются вместе"), "{error}");
    let mut proxy = base();
    proxy.insert("TG_IMPORT_PROXY_URL".into(), "file:///secret".into());
    let error = Settings::from_map(&proxy)
        .err()
        .ok_or("accepted file proxy")?;
    assert!(error.to_string().contains("TG_IMPORT_PROXY_URL"), "{error}");
    Ok(())
}

#[tokio::test]
async fn forward_ticket_is_account_local_and_other_user_cannot_arm_it() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.public = true;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.bot_username = "fixture_bot".into();
    for uid in [111, 222] {
        accounts::initialize(&settings, uid)?;
        auth::save_account(&settings, &json!({"id":uid,"first_name":"User"}))?;
    }
    let spec = Spec::load_initial(&settings);
    let one = AccountContext::account(&settings, 111, Role::Owner);
    let two = AccountContext::account(&settings, 222, Role::Owner);
    let ticket = import_forward::begin(&settings, &one, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let foreign_arm = json!({"message":{"from":{"id":222},"chat":{"id":222,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(!import_forward::receive(&settings, &foreign_arm, &spec).await?);
    let update = json!({"message":{"from":{"id":111},"chat":{"id":111,"type":"private"},"text":"Forwarded","forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    let own_arm = json!({"message":{"from":{"id":111},"chat":{"id":111,"type":"private"},"text":format!("/start import_{id}")}});
    assert!(import_forward::receive(&settings, &own_arm, &spec).await?);
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    assert!(!import_forward::receive(&settings, &update, &spec).await?);
    assert_eq!(import_forward::get(&one, id)?["state"], "done");
    assert_eq!(drafts::list(&one)?.len(), 1);
    db::migrate(&mut db::Store::connect(&two)?)?;
    assert!(drafts::list(&two)?.is_empty());
    assert!(import_inbox::get(&two, id, auth::now())?.is_none());
    let root = AccountContext::root(&settings);
    db::migrate(&mut db::Store::connect(&root)?)?;
    assert!(drafts::list(&root)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn forward_database_limit_finishes_ticket_with_error() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = settings(&dir)?;
    settings.public = true;
    settings.updates_on = true;
    settings.bot_token = "fixture-token".into();
    settings.bot_username = "fixture_bot".into();
    settings.account_database_mb = 1.0;
    accounts::initialize(&settings, 777)?;
    auth::save_account(&settings, &json!({"id":777,"first_name":"User"}))?;
    let context = AccountContext::account(&settings, 777, Role::Owner);
    let spec = Spec::load_initial(&settings);
    let ticket = import_forward::begin(&settings, &context, &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    assert!(import_inbox::arm(&context, id, 777, auth::now())?);
    fill_account_database(&context, 2)?;
    let update = json!({"message":{"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"x".repeat(90_000),"forward_origin":{"type":"channel","chat":{"username":"simple_agi"},"message_id":203}}});
    assert!(import_forward::receive(&settings, &update, &spec).await?);
    let result = import_forward::get(&context, id)?;
    assert_eq!(result["state"], "failed");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Лимит базы"),
        "{result}"
    );
    assert!(drafts::list(&context)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn forward_update_only_acknowledges_in_own_private_chat() -> TestResult {
    let dir = TempDir::new()?;
    let (base, mock, server) = bot_mock().await?;
    let mut settings = settings(&dir)?;
    settings.api_base = base;
    settings.bot_token = "1:fixture".into();
    settings.preview_chat = "777".into();
    settings.bot_username = "fixture".into();
    settings.updates_on = true;
    let state = AppState::new(settings.clone());
    let ticket =
        import_forward::begin(&settings, &AccountContext::root(&settings), &link()?.url())?;
    let id = ticket["id"].as_str().ok_or("id")?;
    let arm = json!({"message":{"message_id":1,"from":{"id":777},"chat":{"id":777,"type":"private"},"text":format!("/start import_{id}")}});
    assert_eq!(updates::handle_update(&state, &arm).await?, Some("import"));
    let forward = json!({"message":{"message_id":2,"from":{"id":777},"chat":{"id":777,"type":"private"},"text":"Hello","forward_origin":{"type":"hidden_user"}}});
    assert_eq!(
        updates::handle_update(&state, &forward).await?,
        Some("import")
    );
    let calls = mock.all();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|(method, body)| method == "sendMessage" && body["chat_id"] == json!(777)),
        "{calls:?}"
    );
    assert!(
        calls[0].1["text"]
            .as_str()
            .unwrap_or_default()
            .contains("перешлите"),
        "{calls:?}"
    );
    assert!(
        calls[1].1["text"]
            .as_str()
            .unwrap_or_default()
            .contains("обработана"),
        "{calls:?}"
    );
    server.abort();
    Ok(())
}
