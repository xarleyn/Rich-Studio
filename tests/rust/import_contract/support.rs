use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use grammers_tl_types as tl;
use rich_studio::{
    account_context::AccountContext, errors::ImportError, services::import_links::parse_link,
    settings::Settings, store::db,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;
pub(crate) fn settings(dir: &TempDir) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([
        ("DATA_DIR".into(), dir.path().to_string_lossy().into_owned()),
        ("SPEC_PIN".into(), "on".into()),
        ("TG_UPDATES".into(), "off".into()),
    ]))?)
}
pub(crate) fn session(ipv6: bool) -> String {
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
pub(crate) fn link() -> Result<rich_studio::services::import_links::MessageLink, ImportError> {
    parse_link("https://t.me/simple_agi/203")
}

pub(crate) fn mt_message(id: i32, part: bool) -> tl::types::Message {
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
pub(crate) struct BotCalls(Arc<Mutex<Vec<(String, Value)>>>);
impl BotCalls {
    pub(crate) fn all(&self) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap_or_else(|error| panic!("lock: {error}"))
            .clone()
    }
}

pub(crate) async fn bot_mock()
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
pub(crate) fn fill_account_database(
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
