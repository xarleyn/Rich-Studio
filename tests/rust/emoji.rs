//! Custom emoji packs: account-local meta, Bot API only, cached pictures and the
//! HTTP surface the editor panel uses.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    build_router,
    services::emoji::{EmojiError, EmojiService},
    settings::Settings,
    store,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SET: &str = "richpost_test_emoji";
const LINK: &str = "https://t.me/addemoji/richpost_test_emoji";
const ID1: &str = "1111111111111111111";
const ID2: &str = "2222222222222222222";
const ID3: &str = "3333333333333333333";

fn settings(dir: &TempDir, base: &str) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "123:TEST".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]))?)
}

fn state(dir: &TempDir, base: &str) -> Result<AppState, Box<dyn std::error::Error>> {
    Ok(AppState::new(settings(dir, base)?))
}

fn service(state: &AppState) -> EmojiService {
    EmojiService::new(
        state.account.clone(),
        state.transport(),
        state.settings.emoji_cache_dir(state.account.account_id),
    )
}

#[derive(Clone, Debug)]
struct Call {
    method: String,
    body: Value,
}

#[derive(Clone, Default)]
struct MockTelegram {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl MockTelegram {
    fn record(&self, method: &str, body: Value) {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .push(Call {
                method: method.to_owned(),
                body,
            });
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|error| panic!("calls: {error}"))
            .clone()
    }

    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.method == method)
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.calls_to(method).len()
    }
}

fn sticker(
    custom_emoji_id: &str,
    emoji: &str,
    is_video: bool,
    is_animated: bool,
    thumbnail: Option<&str>,
) -> Value {
    let mut sticker = json!({
        "file_id": match custom_emoji_id {
            ID1 => "emoji-file-1",
            ID2 => "emoji-file-2",
            _ => "emoji-file-3",
        },
        "file_unique_id": custom_emoji_id,
        "type": "custom_emoji",
        "width": 100,
        "height": 100,
        "is_animated": is_animated,
        "is_video": is_video,
        "emoji": emoji,
        "set_name": SET,
        "custom_emoji_id": custom_emoji_id,
    });
    if let Some(thumbnail) = thumbnail {
        sticker["thumbnail"] =
            json!({"file_id": thumbnail, "file_unique_id": thumbnail, "width": 100, "height": 100});
    }
    sticker
}

fn emoji_set() -> Value {
    json!({
        "name": SET,
        "title": "RichPost Test",
        "sticker_type": "custom_emoji",
        "stickers": [
            sticker(ID1, "🚀", false, false, None),
            sticker(ID2, "🔥", true, false, Some("emoji-thumb-2")),
            sticker(ID3, "🎉", false, true, Some("emoji-thumb-3")),
        ],
    })
}

/// file_id -> (getFile file_path, bytes)
fn telegram_file(file_id: &str) -> Option<(&'static str, &'static [u8])> {
    match file_id {
        "emoji-file-1" => Some(("emojis/1111111111111111111.webp", b"fake emoji webp")),
        "emoji-file-2" => Some(("emojis/2222222222222222222.webm", b"fake emoji webm")),
        "emoji-file-3" => Some(("emojis/3333333333333333333.tgs", b"fake emoji tgs")),
        "emoji-thumb-2" => Some(("emojis/thumb-2.webp", b"fake thumb webp")),
        "emoji-thumb-3" => Some(("emojis/thumb-3.webp", b"fake thumb webp")),
        _ => None,
    }
}

fn file_bytes(path: &str) -> Option<&'static [u8]> {
    [
        "emoji-file-1",
        "emoji-file-2",
        "emoji-file-3",
        "emoji-thumb-2",
        "emoji-thumb-3",
    ]
    .iter()
    .find_map(|file_id| {
        telegram_file(file_id)
            .filter(|(reported, _)| *reported == path)
            .map(|(_, bytes)| bytes)
    })
}

fn answer(method: &str, body: &Value) -> Value {
    match method {
        "getStickerSet" => match body["name"].as_str().unwrap_or_default() {
            SET => json!({"ok": true, "result": emoji_set()}),
            "cats" => {
                json!({"ok": true, "result": {"name": "cats", "title": "Cats", "sticker_type": "sticker", "stickers": []}})
            }
            "empty" => {
                json!({"ok": true, "result": {"name": "empty", "title": "Empty", "sticker_type": "custom_emoji", "stickers": []}})
            }
            _ => {
                json!({"ok": false, "error_code": 400, "description": "Bad Request: STICKER_SET_INVALID"})
            }
        },
        "getCustomEmojiStickers" => {
            let ids = body["custom_emoji_ids"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let found = emoji_set()["stickers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|sticker| ids.contains(&sticker["custom_emoji_id"]))
                .collect::<Vec<_>>();
            json!({"ok": true, "result": found})
        }
        "getFile" => {
            let file_id = body["file_id"].as_str().unwrap_or_default();
            match telegram_file(file_id) {
                Some((path, _)) => {
                    json!({"ok": true, "result": {"file_id": file_id, "file_unique_id": file_id, "file_path": path}})
                }
                None => {
                    json!({"ok": false, "error_code": 400, "description": "Bad Request: invalid file_id"})
                }
            }
        }
        _ => {
            json!({"ok": false, "error_code": 400, "description": "Bad Request: method not supported"})
        }
    }
}

/// A Bot API stand-in: JSON methods plus `GET /file/bot<token>/<path>`.
async fn telegram() -> (String, MockTelegram) {
    let mock = MockTelegram::default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("address: {error}"));
    let server = mock.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(value) => value,
                Err(_) => break,
            };
            let mut data = Vec::new();
            let mut buffer = [0u8; 16384];
            loop {
                let count = socket.read(&mut buffer).await.unwrap_or(0);
                if count == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..count]);
                if data.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data).to_lowercase();
                    let length = headers
                        .split("content-length:")
                        .nth(1)
                        .and_then(|tail| tail.lines().next())
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let end = data
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap_or(0)
                        + 4;
                    if data.len() >= end + length {
                        break;
                    }
                }
            }
            let raw = String::from_utf8_lossy(&data).into_owned();
            let path = raw.split(' ').nth(1).unwrap_or_default().to_owned();
            if let Some(rest) = path.strip_prefix("/file/bot")
                && let Some((_token, file_path)) = rest.split_once('/')
            {
                let bytes = file_bytes(file_path).unwrap_or_default();
                let content_type = if file_path.ends_with(".webp") {
                    "image/webp"
                } else {
                    "application/octet-stream"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(bytes).await;
                continue;
            }
            let method = path.rsplit('/').next().unwrap_or_default().to_owned();
            let body = raw
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .unwrap_or(Value::Null);
            server.record(&method, body.clone());
            let response = answer(&method, &body).to_string();
            let packet = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            let _ = socket.write_all(packet.as_bytes()).await;
        }
    });
    (format!("http://{address}"), mock)
}

async fn call(
    app: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let (body, content_type) = match body {
        Some(value) => (Body::from(value.to_string()), Some("application/json")),
        None => (Body::empty(), None),
    };
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder.body(body)?;
    let response = app.oneshot(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?.to_vec();
    Ok((status, headers, bytes))
}

#[tokio::test]
async fn packs_add_list_remove_and_reject_regular_stickers() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base)?;
    let service = service(&state);

    let added = service.add_pack(LINK).await?;
    assert_eq!(added["short_name"], SET);
    assert_eq!(added["title"], "RichPost Test");
    let stickers = added["stickers"]
        .as_array()
        .ok_or("stickers")?
        .iter()
        .map(|sticker| {
            (
                sticker["emoji"].as_str().unwrap_or_default(),
                sticker["kind"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stickers,
        vec![("🚀", "static"), ("🔥", "video"), ("🎉", "tgs")]
    );
    assert_eq!(
        telegram.calls_to("getStickerSet")[0].body,
        json!({"name": SET})
    );
    assert_eq!(service.list_packs()?.len(), 1);
    assert_eq!(
        service.find_sticker(ID1)?.ok_or("missing sticker")?["emoji"].as_str(),
        Some("🚀")
    );

    // The t.me link normalizes to the same short name: no duplicate entry.
    let repeated = service
        .add_pack("@t.me/addemoji/richpost_test_emoji")
        .await?;
    assert_eq!(repeated["short_name"], SET);
    assert_eq!(service.list_packs()?.len(), 1);

    service.remove_pack(SET)?;
    assert!(service.list_packs()?.is_empty());
    assert!(matches!(
        service.remove_pack(SET),
        Err(EmojiError::NotFound(_))
    ));

    let before = telegram.count("getStickerSet");
    for name in ["", &"x".repeat(100), "../etc/passwd"] {
        let error = service
            .add_pack(name)
            .await
            .expect_err("an invalid short name must be refused before Telegram");
        assert!(
            matches!(&error, EmojiError::Invalid(_)),
            "{name}: {error:?}"
        );
    }
    assert_eq!(telegram.count("getStickerSet"), before);

    let error = service
        .add_pack("cats")
        .await
        .expect_err("ordinary stickers must be refused");
    assert!(matches!(&error, EmojiError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("не набор кастомных эмодзи"));
    let error = service
        .add_pack("empty")
        .await
        .expect_err("a custom emoji set without stickers must be refused");
    assert_eq!(error.to_string(), "В наборе «empty» нет кастомных эмодзи.");
    assert!(matches!(
        service.add_pack("no_such_set_emoji").await,
        Err(EmojiError::NotFound(_))
    ));
    assert!(
        !telegram
            .calls()
            .iter()
            .any(|call| call.method.starts_with("send"))
    );
    Ok(())
}

#[tokio::test]
async fn the_pack_limit_is_twenty_and_existing_packs_still_refresh() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let state = state(&dir, &base)?;
    let service = service(&state);
    let names = (0..20)
        .map(|index| format!("pack{index}"))
        .collect::<Vec<_>>();
    store::meta::set(&state.account, "emoji-packs", &json!(names).to_string())?;
    let error = service
        .add_pack(SET)
        .await
        .expect_err("a 21st pack must be refused");
    assert!(matches!(&error, EmojiError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("Паков уже 20"));
    assert!(service.list_packs()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn pictures_download_once_and_keep_their_headers() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base)?;
    service(&state).add_pack(SET).await?;
    let app = build_router(state.clone());

    let (status, headers, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/img/{ID1}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/webp");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        "sandbox; default-src 'none'"
    );
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "private, immutable, max-age=31536000"
    );
    assert_eq!(body, b"fake emoji webp");
    assert_eq!(telegram.count("getCustomEmojiStickers"), 1);
    assert_eq!(telegram.count("getFile"), 1);

    // A second request is served from the cache, without any Telegram call.
    let (status, _, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/img/{ID1}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"fake emoji webp");
    assert_eq!(telegram.count("getCustomEmojiStickers"), 1);
    assert_eq!(telegram.count("getFile"), 1);

    // The video emoji has a webp thumbnail for the static view and a webm full file.
    let (_, headers, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/img/{ID2}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(headers[header::CONTENT_TYPE], "image/webp");
    assert_eq!(body, b"fake thumb webp");
    assert_eq!(telegram.count("getCustomEmojiStickers"), 2);
    assert_eq!(telegram.count("getFile"), 2);
    let (_, _, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/img/{ID2}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(body, b"fake thumb webp");
    assert_eq!(telegram.count("getFile"), 2);

    let (status, headers, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/file/{ID2}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "video/webm");
    assert_eq!(body, b"fake emoji webm");
    let (status, headers, body) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/file/{ID3}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/gzip");
    assert_eq!(body, b"fake emoji tgs");

    let before = telegram.count("getCustomEmojiStickers");
    let (status, _, _) = call(app.clone(), "GET", "/api/emoji/img/not-an-id", None, &[]).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(telegram.count("getCustomEmojiStickers"), before);
    let (status, _, _) = call(
        app.clone(),
        "GET",
        &format!("/api/emoji/img/{}", "9".repeat(31)),
        None,
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(telegram.count("getCustomEmojiStickers"), before + 1);

    let cache = state.settings.emoji_cache_dir(None);
    for name in [
        format!("{ID1}.thumb.webp"),
        format!("{ID2}.thumb.webp"),
        format!("{ID2}.full.webm"),
        format!("{ID3}.full.tgs"),
    ] {
        assert!(cache.join(name).is_file(), "missing cached picture");
    }
    let media = state.settings.media_dir(None);
    assert!(!media.exists() || media.read_dir()?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn http_contract_and_reference_roles() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let state = state(&dir, &base)?;
    let app = build_router(state.clone());

    let (status, _, body) = call(
        app.clone(),
        "POST",
        "/api/emoji/packs",
        Some(json!({"name": LINK})),
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let added: Value = serde_json::from_slice(&body)?;
    assert_eq!(added["added"]["short_name"], SET);
    assert_eq!(added["packs"].as_array().map(Vec::len), Some(1));

    let (status, _, body) = call(app.clone(), "GET", "/api/emoji/packs", None, &[]).await?;
    assert_eq!(status, StatusCode::OK);
    let listed: Value = serde_json::from_slice(&body)?;
    assert_eq!(listed["packs"][0]["short_name"], SET);

    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            "/api/emoji/packs/no_such_pack",
            None,
            &[]
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/emoji/packs",
            Some(json!({"name": ""})),
            &[]
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/emoji/packs",
            Some(json!({"name": 42})),
            &[]
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    store::auth::add_token(&state.settings, "reader", "emoji-reader", "read")?;
    let auth = [("authorization", "Bearer emoji-reader")];
    assert_eq!(
        call(app.clone(), "GET", "/api/emoji/packs", None, &auth)
            .await?
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/emoji/packs",
            Some(json!({"name": LINK})),
            &auth
        )
        .await?
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/emoji/packs/{SET}"),
            None,
            &auth
        )
        .await?
        .0,
        StatusCode::FORBIDDEN
    );

    store::auth::add_token(&state.settings, "drafter", "emoji-drafter", "draft")?;
    let draft = [("authorization", "Bearer emoji-drafter")];
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/emoji/packs",
            Some(json!({"name": LINK})),
            &draft
        )
        .await?
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/emoji/packs/{SET}"),
            None,
            &draft
        )
        .await?
        .0,
        StatusCode::OK
    );
    let (status, _, body) = call(app.clone(), "GET", "/api/emoji/packs", None, &draft).await?;
    assert_eq!(status, StatusCode::OK);
    let listed: Value = serde_json::from_slice(&body)?;
    assert_eq!(listed["packs"], json!([]));

    assert_eq!(
        call(app, "DELETE", &format!("/api/emoji/packs/{SET}"), None, &[])
            .await?
            .0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn packs_are_account_local() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let state = state(&dir, &base)?;
    let one = AccountContext::account(&state.settings, 101, Role::Drafts);
    let two = AccountContext::account(&state.settings, 202, Role::Drafts);
    let one_service = EmojiService::new(
        one.clone(),
        state.transport(),
        state.settings.emoji_cache_dir(Some(101)),
    );
    let two_service = EmojiService::new(
        two.clone(),
        state.transport(),
        state.settings.emoji_cache_dir(Some(202)),
    );
    one_service.add_pack(SET).await?;
    assert_eq!(one_service.list_packs()?.len(), 1);
    assert!(two_service.list_packs()?.is_empty());
    assert!(matches!(
        two_service.remove_pack(SET),
        Err(EmojiError::NotFound(_))
    ));
    assert_ne!(
        state.settings.emoji_cache_dir(Some(101)),
        state.settings.emoji_cache_dir(Some(202))
    );
    Ok(())
}
