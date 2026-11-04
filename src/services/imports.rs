//! Account-scoped import into a new editable draft; this module never publishes.

use super::{
    import_links::{MessageLink, parse_link},
    import_web,
};
use crate::{
    account_context::AccountContext,
    errors::ImportError,
    settings::Settings,
    spec::{
        import_content::{Converter, entities_text, representations},
        source::Spec,
    },
    store::{db, drafts},
    telegram::import_client::{Connection, rpc_error},
};
use futures_util::{Stream, StreamExt};
use grammers_client::{
    Client,
    media::{Document, Photo},
    peer::Peer,
};
use grammers_session::types::PeerRef;
use grammers_tl_types as tl;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const MAX_FILES: usize = 30;
pub const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const DEADLINE: Duration = Duration::from_secs(120);
static ACTIVE: OnceLock<Mutex<HashSet<Option<i64>>>> = OnceLock::new();

pub struct ActiveImport(Option<i64>);
impl ActiveImport {
    pub fn acquire(uid: Option<i64>) -> Result<Self, ImportError> {
        let mut active = ACTIVE
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| ImportError::new(500, "Импорт недоступен."))?;
        if active.contains(&uid) || active.len() >= 4 {
            return Err(ImportError::new(
                429,
                "Импорт уже выполняется. Дождитесь завершения.",
            ));
        }
        active.insert(uid);
        Ok(Self(uid))
    }
}
impl Drop for ActiveImport {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.get_or_init(Default::default).lock() {
            active.remove(&self.0);
        }
    }
}

pub fn session_path(settings: &Settings, context: &AccountContext) -> PathBuf {
    context
        .account_id
        .map_or_else(
            || settings.data_dir.clone(),
            |uid| settings.account_dir(uid),
        )
        .join("telegram-import.session.txt")
}
pub fn status(settings: &Settings, context: &AccountContext) -> Value {
    json!({"mtproto_configured":settings.import_api_id!=0&&!settings.import_api_hash.is_empty(),"own_session":session_path(settings,context).is_file(),"bot_username":settings.bot_username,"forward_available":settings.updates_on&&!settings.bot_token.is_empty()&&(context.account_id.is_some()||(!settings.preview_chat.is_empty()&&settings.preview_chat.bytes().all(|ch|ch.is_ascii_digit())))})
}

pub fn save_result(
    context: &AccountContext,
    blocks: Vec<Value>,
    mut warnings: Vec<String>,
    link: &MessageLink,
    provider: &str,
    title: &str,
) -> Result<Value, ImportError> {
    if blocks.is_empty() {
        return Err(ImportError::new(
            422,
            "Сообщение не содержит поддерживаемого содержимого.",
        ));
    }
    let encoded = crate::store::templates::encode(&json!(blocks))
        .map_err(|_| ImportError::new(422, "Неверное содержимое сообщения."))?;
    if encoded.len() > 2 * 1024 * 1024 {
        return Err(ImportError::new(
            413,
            "Содержимое превышает лимит импорта 2 МБ.",
        ));
    }
    let (html, markdown, files) = representations(&blocks, &mut warnings)?;
    let mut seen = BTreeSet::new();
    warnings.retain(|warning| seen.insert(warning.clone()));
    let mut source = link.describe();
    source["provider"] = json!(provider);
    let payload = json!({"rich_message":{"blocks":blocks,"media":files},"options":{},"_studio":{"format":"blocks","buffers":{"blocks":blocks,"html":html,"markdown":markdown},"import_source":source,"import_warnings":warnings}});
    let title = if title.is_empty() {
        format!(
            "Импорт {}/{}",
            link.username.as_deref().unwrap_or("Telegram"),
            link.message_id
        )
    } else {
        title.to_owned()
    };
    db::migrate(&mut db::Store::connect(context)?)?;
    let draft = drafts::save(context, None, &title, &payload, None, None, None)?
        .ok_or_else(|| ImportError::new(500, "Не удалось сохранить импорт."))?;
    Ok(
        json!({"draft":draft,"source":source,"warnings":warnings,"block_count":blocks.len(),"media_count":files.len()}),
    )
}

#[derive(Default)]
pub struct SavedMedia {
    paths: Vec<PathBuf>,
    pub total: u64,
    committed: bool,
}
impl SavedMedia {
    pub fn commit(&mut self) {
        self.committed = true;
    }
    pub fn count(&self) -> usize {
        self.paths.len()
    }
}
impl Drop for SavedMedia {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

struct PendingFile {
    part: PathBuf,
    target: PathBuf,
    file: Option<std::fs::File>,
    keep: bool,
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.keep {
            let _ = std::fs::remove_file(&self.part);
            let _ = std::fs::remove_file(&self.target);
        }
    }
}

async fn file_step<F>(action: F) -> Result<PendingFile, ImportError>
where
    F: FnOnce() -> Result<PendingFile, ImportError> + Send + 'static,
{
    tokio::task::spawn_blocking(action)
        .await
        .map_err(|_| ImportError::new(500, "Запись медиа прервана."))?
}

fn io_error(_: std::io::Error) -> ImportError {
    ImportError::new(502, "Не удалось сохранить медиа сообщения Telegram.")
}
fn media_limit() -> ImportError {
    ImportError::new(413, "Медиа превышает лимит размера импорта.")
}
async fn capacity(
    context: &AccountContext,
    settings: &Settings,
    additional: u64,
) -> Result<(), ImportError> {
    if context.account_id.is_none() || settings.account_storage_mb == 0.0 {
        return Ok(());
    }
    let directory = context.media_dir.clone();
    let limit = (settings.account_storage_mb * 1024.0 * 1024.0) as u64;
    tokio::task::spawn_blocking(move || {
        let mut size = additional;
        for entry in std::fs::read_dir(directory).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let metadata = entry.metadata().map_err(io_error)?;
            if metadata.is_file() {
                size = size.saturating_add(metadata.len());
            }
        }
        if size > limit {
            Err(ImportError::new(
                413,
                "Лимит файлов учётки исчерпан. Удалите ненужные файлы.",
            ))
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|_| ImportError::new(500, "Проверка места прервана."))?
}

/// Stream a single file with independent per-file, total and account limits.
/// Each filesystem operation owns its cleanup guard inside the blocking worker. A cancelled
/// await cannot race a detached file creation/write/rename, including on Windows.
pub async fn save_download<S>(
    context: &AccountContext,
    settings: &Settings,
    saved: &mut SavedMedia,
    kind: &str,
    filename: Option<&str>,
    declared: u64,
    chunks: S,
) -> Result<Value, ImportError>
where
    S: Stream<Item = Result<Vec<u8>, ImportError>>,
{
    if saved.count() >= MAX_FILES {
        return Err(ImportError::new(
            413,
            "В одном импорте допускается не больше 30 файлов.",
        ));
    }
    let limit = ((settings.max_upload_mb * 1024.0 * 1024.0) as u64)
        .min(MAX_TOTAL_BYTES.saturating_sub(saved.total));
    if declared > limit {
        return Err(media_limit());
    }
    let extension = match kind {
        "photo" => ".jpg",
        "animation" | "video" => ".mp4",
        "audio" => ".mp3",
        "voice_note" => ".ogg",
        _ => ".bin",
    };
    let suffix = filename
        .map(|name| {
            let leaf = name.rsplit(['/', '\\']).next().unwrap_or("file");
            format!(
                "-{}",
                leaf.chars()
                    .map(|ch| if ch.is_alphanumeric() || ".-_".contains(ch) {
                        ch
                    } else {
                        '_'
                    })
                    .take(120)
                    .collect::<String>()
            )
        })
        .unwrap_or_else(|| extension.to_owned());
    let name = format!("import-{}{suffix}", uuid::Uuid::new_v4().simple());
    let path = context.media_dir.join(&name);
    let part = context.media_dir.join(format!(".{name}.part"));
    let directory = context.media_dir.clone();
    let target = path.clone();
    let mut pending = file_step(move || {
        let mut pending = PendingFile {
            part,
            target,
            file: None,
            keep: false,
        };
        std::fs::create_dir_all(directory).map_err(io_error)?;
        pending.file = Some(
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&pending.part)
                .map_err(io_error)?,
        );
        Ok(pending)
    })
    .await?;
    let mut size = 0u64;
    futures_util::pin_mut!(chunks);
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        size = size.saturating_add(chunk.len() as u64);
        if size > limit {
            return Err(media_limit());
        }
        capacity(context, settings, chunk.len() as u64).await?;
        pending = file_step(move || {
            pending
                .file
                .as_mut()
                .ok_or_else(|| ImportError::new(500, "Файл импорта закрыт."))?
                .write_all(&chunk)
                .map_err(io_error)?;
            Ok(pending)
        })
        .await?;
    }
    if size == 0 {
        return Err(ImportError::new(422, "Telegram вернул пустой файл."));
    }
    pending = file_step(move || {
        if let Some(mut file) = pending.file.take() {
            file.flush().map_err(io_error)?;
        }
        std::fs::rename(&pending.part, &pending.target).map_err(io_error)?;
        Ok(pending)
    })
    .await?;
    saved.paths.push(path);
    pending.keep = true;
    drop(pending);
    saved.total += size;
    Ok(json!({"type":kind,"media":format!("upload:{name}")}))
}

pub fn document_kind(document: &tl::types::Document) -> &'static str {
    if document
        .attributes
        .iter()
        .any(|item| matches!(item, tl::enums::DocumentAttribute::Animated))
    {
        return "animation";
    }
    if let Some(audio) = document.attributes.iter().find_map(|item| match item {
        tl::enums::DocumentAttribute::Audio(value) => Some(value),
        _ => None,
    }) {
        return if audio.voice { "voice_note" } else { "audio" };
    }
    if document
        .attributes
        .iter()
        .any(|item| matches!(item, tl::enums::DocumentAttribute::Video(_)))
    {
        "video"
    } else {
        "document"
    }
}

async fn download_photo(
    client: &Client,
    context: &AccountContext,
    settings: &Settings,
    saved: &mut SavedMedia,
    photo: tl::enums::Photo,
) -> Result<Value, ImportError> {
    let photo = Photo::from_raw_media(tl::types::MessageMediaPhoto {
        spoiler: false,
        live_photo: false,
        photo: Some(photo),
        ttl_seconds: None,
        video: None,
    });
    let iterator = client.iter_download(&photo);
    let chunks = futures_util::stream::unfold(iterator, |mut iterator| async move {
        match iterator.next().await {
            Ok(Some(chunk)) => Some((Ok(chunk), iterator)),
            Ok(None) => None,
            Err(error) => Some((Err(rpc_error(error)), iterator)),
        }
    });
    save_download(context, settings, saved, "photo", None, 0, chunks).await
}
async fn download_document(
    client: &Client,
    context: &AccountContext,
    settings: &Settings,
    saved: &mut SavedMedia,
    document: tl::types::Document,
) -> Result<Value, ImportError> {
    let kind = document_kind(&document);
    let filename = document.attributes.iter().find_map(|item| match item {
        tl::enums::DocumentAttribute::Filename(value) => Some(value.file_name.clone()),
        _ => None,
    });
    let declared = u64::try_from(document.size).unwrap_or(u64::MAX);
    let document = Document::from_raw_media(tl::types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: false,
        round: false,
        voice: false,
        document: Some(document.into()),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    });
    let iterator = client.iter_download(&document);
    let chunks = futures_util::stream::unfold(iterator, |mut iterator| async move {
        match iterator.next().await {
            Ok(Some(chunk)) => Some((Ok(chunk), iterator)),
            Ok(None) => None,
            Err(error) => Some((Err(rpc_error(error)), iterator)),
        }
    });
    save_download(
        context,
        settings,
        saved,
        kind,
        filename.as_deref(),
        declared,
        chunks,
    )
    .await
}

/// Normalize serde's externally tagged TL enums to Telethon's constructor-tagged data.
/// Prefixes are selected by schema position; text constructors retain their exact TL names.
pub fn normalize_tl(value: Value, prefix: &str) -> Value {
    match value {
        Value::String(name) if !prefix.is_empty() => {
            json!({"_":if name.starts_with("Text")||name.starts_with(prefix){name}else{format!("{prefix}{name}")}})
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalize_tl(item, prefix))
                .collect(),
        ),
        Value::Object(mut object) => {
            if !prefix.is_empty() && object.len() == 1 {
                let key = object.keys().next().cloned().unwrap_or_default();
                if key.chars().next().is_some_and(char::is_uppercase) {
                    let child = object.remove(&key).unwrap_or(Value::Null);
                    let mut output = match child {
                        Value::Object(child) => child,
                        _ => serde_json::Map::new(),
                    };
                    output.insert(
                        "_".into(),
                        json!(if key.starts_with("Text") || key.starts_with(prefix) {
                            key
                        } else {
                            format!("{prefix}{key}")
                        }),
                    );
                    object = output;
                }
            }
            let constructor = object
                .get("_")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            for (key, child) in object.iter_mut() {
                if key == "_" {
                    continue;
                }
                let nested = match key.as_str() {
                    "blocks" | "cover" => "PageBlock",
                    "text" if constructor == "TextPlain" => "",
                    "text" | "texts" | "title" | "credit" => "Text",
                    "caption"
                        if [
                            "PageBlockBlockquote",
                            "PageBlockBlockquoteBlocks",
                            "PageBlockPullquote",
                        ]
                        .contains(&constructor.as_str()) =>
                    {
                        "Text"
                    }
                    "caption" => "PageCaption",
                    "rows" => "PageTableRow",
                    "cells" => "PageTableCell",
                    "items" if constructor == "PageBlockList" => "PageListItem",
                    "items" if constructor == "PageBlockOrderedList" => "PageListOrderedItem",
                    "items"
                        if ["PageBlockCollage", "PageBlockSlideshow"]
                            .contains(&constructor.as_str()) =>
                    {
                        "PageBlock"
                    }
                    "geo" => "GeoPoint",
                    _ => "",
                };
                *child = normalize_tl(std::mem::take(child), nested);
            }
            Value::Object(object)
        }
        other => other,
    }
}
fn tl_value<T: Serialize>(value: &T, prefix: &str) -> Result<Value, ImportError> {
    serde_json::to_value(value)
        .map(|value| normalize_tl(value, prefix))
        .map_err(|_| ImportError::new(422, "Неверное содержимое Telegram."))
}

pub fn mt_entities(values: &[tl::enums::MessageEntity]) -> Result<Vec<Value>, ImportError> {
    values
        .iter()
        .map(|entity| {
            let mut value = tl_value(entity, "MessageEntity")?;
            let name = value["_"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches("MessageEntity");
            let kind = match name {
                "Strike" => "strikethrough".to_owned(),
                "TextUrl" => "text_link".to_owned(),
                "MentionName" => "text_mention".to_owned(),
                _ => name
                    .chars()
                    .enumerate()
                    .flat_map(|(index, ch)| {
                        if ch.is_uppercase() && index > 0 {
                            vec!['_', ch.to_ascii_lowercase()]
                        } else {
                            vec![ch.to_ascii_lowercase()]
                        }
                    })
                    .collect(),
            };
            if let Some(object) = value.as_object_mut() {
                object.remove("_");
                object.insert("type".into(), json!(kind));
                if let Some(id) = object.remove("document_id") {
                    object.insert("custom_emoji_id".into(), json!(id.to_string()));
                }
            }
            Ok(value)
        })
        .collect()
}

fn referenced_media(value: &Value, needed: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                referenced_media(item, needed)
            }
        }
        Value::Object(object) => {
            if [
                "PageBlockPhoto",
                "PageBlockVideo",
                "PageBlockAudio",
                "PageBlockDocument",
            ]
            .contains(&value["_"].as_str().unwrap_or(""))
            {
                for field in ["photo_id", "video_id", "audio_id", "document_id"] {
                    if let Some(id) = object.get(field) {
                        needed.insert(id.to_string());
                    }
                }
            }
            for child in object.values() {
                referenced_media(child, needed)
            }
        }
        _ => {}
    }
}

pub async fn message_blocks(
    client: &Client,
    context: &AccountContext,
    settings: &Settings,
    message: &tl::types::Message,
    saved: &mut SavedMedia,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>, ImportError> {
    if message.noforwards || message.ttl_period.is_some_and(|ttl| ttl != 0) {
        return Err(ImportError::new(
            403,
            "Импорт защищённых или исчезающих сообщений недоступен.",
        ));
    }
    if let Some(tl::enums::RichMessage::Message(rich)) = &message.rich_message {
        if rich.part {
            return Err(ImportError::new(
                502,
                "Telegram вернул только часть rich-поста. Импорт отменён.",
            ));
        }
        let data = tl_value(&rich.blocks, "PageBlock")?;
        let mut needed = BTreeSet::new();
        referenced_media(&data, &mut needed);
        let mut media = BTreeMap::new();
        for photo in &rich.photos {
            if let tl::enums::Photo::Photo(photo) = photo
                && needed.contains(&photo.id.to_string())
            {
                media.insert(
                    photo.id.to_string(),
                    download_photo(client, context, settings, saved, photo.clone().into()).await?,
                );
            }
        }
        for document in &rich.documents {
            if let tl::enums::Document::Document(document) = document
                && needed.contains(&document.id.to_string())
            {
                media.insert(
                    document.id.to_string(),
                    download_document(client, context, settings, saved, document.clone()).await?,
                );
            }
        }
        let mut converter = Converter::new(media);
        let blocks = converter.blocks(data.as_array().map(Vec::as_slice).unwrap_or(&[]))?;
        warnings.extend(converter.warnings);
        return Ok(blocks);
    }
    let mut blocks = Vec::new();
    if !message.message.is_empty() {
        let entities = mt_entities(message.entities.as_deref().unwrap_or(&[]))?;
        blocks.push(
            json!({"type":"paragraph","text":entities_text(&message.message,&entities,warnings)?}),
        );
    }
    let media = match &message.media {
        Some(tl::enums::MessageMedia::Photo(photo)) => {
            if photo.ttl_seconds.is_some_and(|ttl| ttl != 0) {
                return Err(ImportError::new(403, "Импорт исчезающих медиа недоступен."));
            }
            match &photo.photo {
                Some(value @ tl::enums::Photo::Photo(_)) => Some((
                    download_photo(client, context, settings, saved, value.clone()).await?,
                    photo.spoiler,
                )),
                _ => None,
            }
        }
        Some(tl::enums::MessageMedia::Document(document)) => {
            if document.ttl_seconds.is_some_and(|ttl| ttl != 0) {
                return Err(ImportError::new(403, "Импорт исчезающих медиа недоступен."));
            }
            match &document.document {
                Some(tl::enums::Document::Document(value)) => Some((
                    download_document(client, context, settings, saved, value.clone()).await?,
                    document.spoiler,
                )),
                _ => None,
            }
        }
        None | Some(tl::enums::MessageMedia::Empty) | Some(tl::enums::MessageMedia::WebPage(_)) => {
            None
        }
        Some(_) => {
            warnings.push("Неподдерживаемое вложение исходного сообщения не перенесено.".into());
            blocks.push(json!({"type":"paragraph","text":"[Неподдерживаемое вложение Telegram]"}));
            None
        }
    };
    if let Some((mut item, spoiler)) = media {
        let kind = item["type"].as_str().unwrap_or("document").to_owned();
        if spoiler {
            item["has_spoiler"] = json!(true);
        }
        let mut block = json!({"type":kind});
        block[&kind] = item;
        blocks.insert(0, block);
    }
    Ok(blocks)
}

pub fn full_rich_message(
    response: tl::enums::messages::Messages,
    id: i32,
) -> Result<tl::types::Message, ImportError> {
    let messages = match response {
        tl::enums::messages::Messages::Messages(value) => value.messages,
        tl::enums::messages::Messages::Slice(value) => value.messages,
        tl::enums::messages::Messages::ChannelMessages(value) => value.messages,
        tl::enums::messages::Messages::NotModified(_) => vec![],
    };
    messages.into_iter().find_map(|message|match message{tl::enums::Message::Message(value)if value.id==id&&matches!(&value.rich_message,Some(tl::enums::RichMessage::Message(rich))if !rich.part)=>Some(value),_=>None}).ok_or_else(||ImportError::new(502,"Telegram вернул только часть rich-поста. Импорт отменён."))
}

async fn resolve_peer(
    client: &Client,
    link: &MessageLink,
    is_bot: bool,
) -> Result<PeerRef, ImportError> {
    let peer = if let Some(username) = &link.username {
        client.resolve_username(username).await.map_err(rpc_error)?
    } else {
        let mut dialogs = client.iter_dialogs();
        let mut found = None;
        while let Some(dialog) = dialogs.next().await.map_err(rpc_error)? {
            if dialog.peer_id().bare_id() == link.channel_id {
                found = Some(dialog.peer().clone());
                break;
            }
        }
        found
    }
    .ok_or_else(|| ImportError::new(404, "Источник сообщения не найден."))?;
    let raw = match &peer {
        Peer::Channel(channel) => Some(&channel.raw),
        Peer::Group(group) => match &group.raw {
            tl::enums::Chat::Channel(channel) => Some(channel),
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| ImportError::new(422, "Импорт поддерживает сообщения каналов и супергрупп."))?;
    if raw.noforwards {
        return Err(ImportError::new(
            403,
            "Импорт защищённого канала недоступен.",
        ));
    }
    if is_bot
        && link.username.as_ref().is_none_or(|username| {
            !peer
                .usernames()
                .iter()
                .any(|known| known.eq_ignore_ascii_case(username))
        })
    {
        return Err(ImportError::new(
            403,
            "Общее подключение импортирует только публичные каналы.",
        ));
    }
    peer.to_ref()
        .await
        .map_err(|_| ImportError::new(502, "Источник сообщения недоступен."))?
        .ok_or_else(|| ImportError::new(404, "Источник сообщения недоступен."))
}

async fn mtproto(
    context: &AccountContext,
    settings: &Settings,
    link: &MessageLink,
    saved: &mut SavedMedia,
) -> Result<(Vec<Value>, Vec<String>), ImportError> {
    let path = session_path(settings, context);
    let session = match tokio::fs::read_to_string(path).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(error)),
    };
    if link.channel_id.is_some() && session.trim().is_empty() {
        return Err(ImportError::new(
            403,
            "Для приватной ссылки требуется собственное подключение Telegram.",
        ));
    }
    let connection = Connection::connect(settings, session.trim(), context.account_id).await?;
    let client = &connection.client;
    let peer = resolve_peer(client, link, connection.is_bot).await?;
    let original = client
        .get_messages_by_id(peer, &[link.message_id])
        .await
        .map_err(rpc_error)?
        .into_iter()
        .flatten()
        .find_map(|message| match message.raw {
            tl::enums::Message::Message(value) => Some(value),
            _ => None,
        })
        .ok_or_else(|| ImportError::new(404, "Сообщение не найдено."))?;
    if let Some(topic) = link.topic_id {
        let actual = match &original.reply_to {
            Some(tl::enums::MessageReplyHeader::Header(reply)) => {
                reply.reply_to_top_id.or(reply.reply_to_msg_id)
            }
            _ => None,
        };
        if actual != Some(topic) {
            return Err(ImportError::new(
                422,
                "Сообщение не принадлежит теме, указанной в ссылке.",
            ));
        }
    }
    let mut warnings = Vec::new();
    let messages = if original.grouped_id.is_some() && !link.single {
        let ids = ((original.id - 10).max(1)..=original.id.saturating_add(10)).collect::<Vec<_>>();
        let mut messages = client
            .get_messages_by_id(peer, &ids)
            .await
            .map_err(rpc_error)?
            .into_iter()
            .flatten()
            .filter_map(|message| match message.raw {
                tl::enums::Message::Message(value) if value.grouped_id == original.grouped_id => {
                    Some(value)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.id);
        warnings.push(
            "Импортированы доступные элементы альбома рядом с сообщением; проверьте комплектность."
                .into(),
        );
        messages
    } else {
        vec![original]
    };
    let mut blocks = Vec::new();
    for mut message in messages {
        if matches!(&message.rich_message,Some(tl::enums::RichMessage::Message(rich))if rich.part) {
            let response = client
                .invoke(&tl::functions::messages::GetRichMessage {
                    peer: peer.into(),
                    id: message.id,
                })
                .await
                .map_err(rpc_error)?;
            message = full_rich_message(response, message.id)?;
        }
        blocks.extend(
            message_blocks(client, context, settings, &message, saved, &mut warnings).await?,
        );
        if message.reply_markup.is_some() {
            warnings.push(
                "Кнопки исходного сообщения не перенесены: их действия принадлежат другому боту."
                    .into(),
            );
        }
    }
    connection.close().await;
    Ok((blocks, warnings))
}

pub async fn import_post(
    settings: &Settings,
    context: &AccountContext,
    url: &str,
) -> Result<Value, ImportError> {
    let link = parse_link(url)?;
    let _active = ActiveImport::acquire(context.account_id)?;
    let mut saved = SavedMedia::default();
    let read = async {
        if settings.import_api_id != 0 && !settings.import_api_hash.is_empty() {
            let (blocks, warnings) = mtproto(context, settings, &link, &mut saved).await?;
            return Ok((blocks, warnings, "mtproto"));
        }
        let widget = import_web::fetch_widget(&link).await?;
        if widget.media_urls.len() > MAX_FILES {
            return Err(ImportError::new(
                413,
                "В одном импорте допускается не больше 30 файлов.",
            ));
        }
        let mut blocks = if widget.root.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type":"paragraph","text":widget.root})]
        };
        for url in widget.media_urls {
            let response = import_web::fetch_media(&url).await?;
            let declared = response.content_length().unwrap_or(0);
            let chunks = response.bytes_stream().map(|result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(|_| ImportError::new(502, "Не удалось скачать медиа Telegram."))
            });
            let item = save_download(
                context, settings, &mut saved, "photo", None, declared, chunks,
            )
            .await?;
            blocks.push(json!({"type":"photo","photo":item}));
        }
        let mut warnings =
            vec!["Импорт из веб-виджета: проверьте полноту содержимого и медиа.".into()];
        warnings.extend(widget.warnings);
        Ok((blocks, warnings, "web"))
    };
    let (blocks, warnings, provider) = tokio::time::timeout(DEADLINE, read)
        .await
        .map_err(|_| ImportError::new(502, "Время импорта Telegram истекло."))??;
    save_import(context.clone(), blocks, warnings, link, provider, saved).await
}

pub async fn save_import(
    context: AccountContext,
    blocks: Vec<Value>,
    warnings: Vec<String>,
    link: MessageLink,
    provider: &'static str,
    mut saved: SavedMedia,
) -> Result<Value, ImportError> {
    // The bounded blocking save owns cleanup. If an HTTP client disconnects at this point,
    // the worker finishes either the complete draft+files or cleanup, never a dangling draft.
    tokio::task::spawn_blocking(move || {
        let result = save_result(&context, blocks, warnings, &link, provider, "")?;
        saved.commit();
        Ok(result)
    })
    .await
    .map_err(|_| ImportError::new(500, "Сохранение импорта прервано."))?
}

fn without_actions(value: &mut Value, warnings: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                without_actions(item, warnings)
            }
        }
        Value::Object(object) => {
            if ["buttons", "button"]
                .contains(&object.get("type").and_then(Value::as_str).unwrap_or(""))
            {
                warnings.push(
                    "Кнопки исходного бота заменены отметками; их действия не перенесены.".into(),
                );
                *value = if object.get("type") == Some(&json!("buttons")) {
                    json!({"type":"paragraph","text":"[Кнопки исходного бота]"})
                } else {
                    json!("[Кнопка исходного бота]")
                };
            } else {
                for child in object.values_mut() {
                    without_actions(child, warnings)
                }
            }
        }
        _ => {}
    }
}
pub fn from_forward(
    context: &AccountContext,
    spec: &Spec,
    message: &Value,
    link: &MessageLink,
    mut warnings: Vec<String>,
) -> Result<Value, ImportError> {
    if message["has_protected_content"].as_bool() == Some(true) {
        return Err(ImportError::new(
            422,
            "Импорт защищённого содержимого недоступен.",
        ));
    }
    let mut blocks =
        if let Some(rich) = message.get("rich_message").filter(|value| !value.is_null()) {
            let mut blocks = json!(crate::spec::convert::to_input_blocks(
                spec,
                rich["blocks"].as_array().map(Vec::as_slice).unwrap_or(&[])
            ));
            without_actions(&mut blocks, &mut warnings);
            blocks.as_array().cloned().unwrap_or_default()
        } else {
            let mut blocks = Vec::new();
            let text = message["text"]
                .as_str()
                .filter(|text| !text.is_empty())
                .or_else(|| message["caption"].as_str())
                .unwrap_or("");
            if !text.is_empty() {
                let entities = message["entities"]
                    .as_array()
                    .or_else(|| message["caption_entities"].as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                blocks.push(
                    json!({"type":"paragraph","text":entities_text(text,entities,&mut warnings)?}),
                );
            }
            for kind in ["animation", "photo", "video", "audio", "voice", "document"] {
                if let Some(item) = message.get(kind).filter(|value| !value.is_null()) {
                    let item = item
                        .as_array()
                        .and_then(|items| {
                            items.iter().max_by_key(|item| {
                                item["width"]
                                    .as_u64()
                                    .unwrap_or(0)
                                    .saturating_mul(item["height"].as_u64().unwrap_or(0))
                            })
                        })
                        .unwrap_or(item);
                    let target = if kind == "voice" { "voice_note" } else { kind };
                    let mut block = json!({"type":target});
                    block[target] = json!({"type":target,"media":item["file_id"]});
                    blocks.insert(0, block);
                    break;
                }
            }
            blocks
        };
    if !message["media_group_id"].is_null() {
        warnings.push("Пересылка импортирует одно сообщение альбома. Для всего альбома используйте импорт по ссылке.".into());
    }
    if !message["reply_markup"].is_null() {
        warnings.push("Кнопки исходного бота не перенесены.".into());
    }
    warnings.push("Содержимое получено из выбранной вами пересылки.".into());
    save_result(
        context,
        std::mem::take(&mut blocks),
        warnings,
        link,
        "forward",
        "",
    )
}
