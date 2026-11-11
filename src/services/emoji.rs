//! Custom emoji packs: the editor panel inserts entities, the preview shows real pictures.
//!
//! The Bot API covers everything without MTProto: `getStickerSet` lists a set,
//! `getCustomEmojiStickers` + `getFile` download the pictures. Packs live in the
//! account's meta, pictures are cached under the account directory — outside
//! `media/`, so storage limits and housekeeping never touch them (an emoji pack is
//! configuration, not a user upload).

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::meta;
use crate::telegram::{client::TelegramTransport, uploads::Uploads};

pub const PACKS_KEY: &str = "emoji-packs";
pub const PACK_KEY_PREFIX: &str = "emoji-pack:";
pub const MAX_PACKS: usize = 20;

/// (extension, media type) for a cached picture. Animated emoji keep a webp
/// thumbnail for the static view and their webm/tgs file for the full one.
const KINDS: [(&str, &str); 3] = [
    ("webp", "image/webp"),
    ("webm", "video/webm"),
    ("tgs", "application/gzip"),
];

#[derive(Debug)]
pub enum EmojiError {
    /// The request is wrong and the same request will fail again.
    Invalid(String),
    NotFound(String),
    /// Telegram failed or could not be reached; nothing was written locally.
    Upstream(String),
    Store(StoreError),
    Io(std::io::Error),
}

impl fmt::Display for EmojiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::NotFound(message) | Self::Upstream(message) => {
                formatter.write_str(message)
            }
            Self::Store(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EmojiError {}

impl From<StoreError> for EmojiError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<std::io::Error> for EmojiError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone)]
pub struct EmojiService {
    context: AccountContext,
    transport: TelegramTransport,
    cache_dir: PathBuf,
}

pub fn from_state(state: &crate::AppState) -> EmojiService {
    EmojiService::new(
        state.account.clone(),
        state.transport(),
        state.settings.emoji_cache_dir(state.account.account_id),
    )
}

impl EmojiService {
    pub fn new(context: AccountContext, transport: TelegramTransport, cache_dir: PathBuf) -> Self {
        Self {
            context,
            transport,
            cache_dir,
        }
    }

    pub fn list_packs(&self) -> Result<Vec<Value>, EmojiError> {
        packs(&self.context)
    }

    /// The pack entry (emoji char) for an id, or `None`: the preview uses it as
    /// the fallback text.
    pub fn find_sticker(&self, custom_emoji_id: &str) -> Result<Option<Value>, EmojiError> {
        for pack in self.list_packs()? {
            let Some(stickers) = pack.get("stickers").and_then(Value::as_array) else {
                continue;
            };
            for sticker in stickers {
                if sticker.get("custom_emoji_id").and_then(Value::as_str) == Some(custom_emoji_id) {
                    return Ok(Some(sticker.clone()));
                }
            }
        }
        Ok(None)
    }

    pub fn remove_pack(&self, name: &str) -> Result<(), EmojiError> {
        let short = normalize(name);
        let names = pack_names(&self.context)?;
        if !names.iter().any(|existing| existing == &short) {
            return Err(EmojiError::NotFound(format!("Пака «{short}» нет.")));
        }
        let remaining = names
            .into_iter()
            .filter(|existing| existing != &short)
            .collect::<Vec<_>>();
        write_names(&self.context, &remaining)?;
        // Cached pictures stay on disk: a re-added pack shows them again without
        // downloading anything.
        Ok(())
    }

    pub async fn add_pack(&self, name: &str) -> Result<Value, EmojiError> {
        let short = normalize(name);
        if !valid_short(&short) {
            return Err(EmojiError::Invalid(
                "Нужен короткий код набора или ссылка вида t.me/addemoji/<код>.".to_owned(),
            ));
        }
        let set = self
            .transport
            .call(
                &self.context,
                "getStickerSet",
                json!({"name": short}),
                Uploads::default(),
            )
            .await
            .map_err(|error| EmojiError::NotFound(format!("Набор «{short}» не найден: {error}")))?;
        if set.get("sticker_type").and_then(Value::as_str) != Some("custom_emoji") {
            return Err(EmojiError::Invalid(format!(
                "«{short}» — обычные стикеры, а не набор кастомных эмодзи."
            )));
        }
        let mut stickers = Vec::new();
        for sticker in set
            .get("stickers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(custom_emoji_id) = sticker
                .get("custom_emoji_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            // The panel and the preview pick the renderer by kind: webm video,
            // tgs Lottie or a static webp.
            let kind = if sticker
                .get("is_video")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "video"
            } else if sticker
                .get("is_animated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "tgs"
            } else {
                "static"
            };
            stickers.push(json!({
                "custom_emoji_id": custom_emoji_id,
                "emoji": sticker.get("emoji").and_then(Value::as_str).unwrap_or_default(),
                "kind": kind,
            }));
        }
        if stickers.is_empty() {
            return Err(EmojiError::Invalid(format!(
                "В наборе «{short}» нет кастомных эмодзи."
            )));
        }
        let short_name = set
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(&short)
            .to_owned();
        let title = set.get("title").and_then(Value::as_str).unwrap_or(&short);
        let pack = json!({
            "short_name": short_name,
            "title": title,
            "stickers": stickers,
        });
        let pack_json = serde_json::to_string(&pack).map_err(|error| {
            EmojiError::Store(StoreError::Migration(format!(
                "emoji pack serialization: {error}"
            )))
        })?;
        let context = self.context.clone();
        let key_name = short_name.clone();
        tokio::task::spawn_blocking(move || persist_pack(&context, &key_name, &pack_json))
            .await
            .map_err(|error| {
                EmojiError::Store(StoreError::Migration(format!(
                    "emoji worker failed: {error}"
                )))
            })??;
        Ok(pack)
    }

    /// A cached picture for the id, downloaded once on a miss.
    pub async fn static_file(
        &self,
        custom_emoji_id: &str,
    ) -> Result<(PathBuf, &'static str), EmojiError> {
        self.cached(custom_emoji_id, false).await
    }

    /// The full emoji file: webm, tgs or a static webp.
    pub async fn full_file(
        &self,
        custom_emoji_id: &str,
    ) -> Result<(PathBuf, &'static str), EmojiError> {
        self.cached(custom_emoji_id, true).await
    }

    async fn cached(
        &self,
        custom_emoji_id: &str,
        full: bool,
    ) -> Result<(PathBuf, &'static str), EmojiError> {
        if !valid_id(custom_emoji_id) {
            return Err(EmojiError::NotFound(
                "Нет такого кастомного эмодзи.".to_owned(),
            ));
        }
        let prefix = if full { "full" } else { "thumb" };
        for (extension, media_type) in KINDS {
            let path = self
                .cache_dir
                .join(format!("{custom_emoji_id}.{prefix}.{extension}"));
            if tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.is_file())
            {
                return Ok((path, media_type));
            }
        }
        let sticker = self.sticker_of(custom_emoji_id).await?;
        let (file_id, extension) = file_id_for(&sticker, full)?;
        let (data, _file_path) = self
            .transport
            .download_file(&self.context, &file_id)
            .await
            .map_err(|error| EmojiError::Upstream(format!("Telegram не отдал эмодзи: {error}")))?;
        let path = self
            .cache_dir
            .join(format!("{custom_emoji_id}.{prefix}.{extension}"));
        let temporary = self
            .cache_dir
            .join(format!("{custom_emoji_id}.{prefix}.{extension}.part"));
        write_atomically(&path, &temporary, &data).await?;
        Ok((path, media_type(extension)))
    }

    async fn sticker_of(&self, custom_emoji_id: &str) -> Result<Value, EmojiError> {
        let result = self
            .transport
            .call(
                &self.context,
                "getCustomEmojiStickers",
                json!({"custom_emoji_ids": [custom_emoji_id]}),
                Uploads::default(),
            )
            .await
            .map_err(|error| EmojiError::Upstream(format!("Telegram не отдал эмодзи: {error}")))?;
        result
            .as_array()
            .and_then(|stickers| stickers.first())
            .filter(|sticker| sticker.is_object())
            .cloned()
            .ok_or_else(|| {
                EmojiError::NotFound("Telegram не знает такой кастомный эмодзи.".to_owned())
            })
    }
}

/// Accept `t.me/addemoji/<name>` (with or without scheme) or a bare short name.
pub fn normalize(name: &str) -> String {
    let name = name.trim().trim_start_matches('@');
    for prefix in [
        "https://t.me/addemoji/",
        "http://t.me/addemoji/",
        "t.me/addemoji/",
    ] {
        if let Some(code) = name.strip_prefix(prefix) {
            return code.to_owned();
        }
    }
    name.to_owned()
}

fn valid_short(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_id(custom_emoji_id: &str) -> bool {
    !custom_emoji_id.is_empty()
        && custom_emoji_id.len() <= 32
        && custom_emoji_id.bytes().all(|byte| byte.is_ascii_digit())
}

fn pack_names(context: &AccountContext) -> Result<Vec<String>, EmojiError> {
    let Some(raw) = meta::get(context, PACKS_KEY)? else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_str(&raw).map_err(|error| {
        EmojiError::Store(StoreError::Migration(format!("emoji packs meta: {error}")))
    })?;
    let Some(items) = value.as_array() else {
        return Err(EmojiError::Store(StoreError::Migration(
            "emoji packs meta is not an array".to_owned(),
        )));
    };
    Ok(items
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

fn parse_pack(raw: &str) -> Result<Value, EmojiError> {
    let value: Value = serde_json::from_str(raw).map_err(|error| {
        EmojiError::Store(StoreError::Migration(format!("emoji pack meta: {error}")))
    })?;
    if !value.is_object() {
        return Err(EmojiError::Store(StoreError::Migration(
            "emoji pack meta is not an object".to_owned(),
        )));
    }
    Ok(value)
}

fn packs(context: &AccountContext) -> Result<Vec<Value>, EmojiError> {
    let mut result = Vec::new();
    for name in pack_names(context)? {
        if let Some(raw) = meta::get(context, &format!("{PACK_KEY_PREFIX}{name}"))? {
            result.push(parse_pack(&raw)?);
        }
    }
    Ok(result)
}

fn write_names(context: &AccountContext, names: &[String]) -> Result<(), EmojiError> {
    let value = serde_json::to_string(names).map_err(|error| {
        EmojiError::Store(StoreError::Migration(format!(
            "emoji packs serialization: {error}"
        )))
    })?;
    meta::set(context, PACKS_KEY, &value)?;
    Ok(())
}

fn persist_pack(context: &AccountContext, short_name: &str, pack: &str) -> Result<(), EmojiError> {
    let mut names = pack_names(context)?;
    if !names.iter().any(|name| name == short_name) {
        if names.len() >= MAX_PACKS {
            return Err(EmojiError::Invalid(format!(
                "Паков уже {MAX_PACKS}: уберите один, чтобы добавить новый."
            )));
        }
        names.push(short_name.to_owned());
        write_names(context, &names)?;
    }
    meta::set(context, &format!("{PACK_KEY_PREFIX}{short_name}"), pack)?;
    Ok(())
}

fn file_id_for(sticker: &Value, full: bool) -> Result<(String, &'static str), EmojiError> {
    let file_id = sticker
        .get("file_id")
        .and_then(Value::as_str)
        .filter(|file_id| !file_id.is_empty());
    if full {
        if sticker
            .get("is_video")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return file_id
                .map(|file_id| (file_id.to_owned(), "webm"))
                .ok_or_else(|| EmojiError::NotFound("Telegram не отдал файл эмодзи.".to_owned()));
        }
        if sticker
            .get("is_animated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return file_id
                .map(|file_id| (file_id.to_owned(), "tgs"))
                .ok_or_else(|| EmojiError::NotFound("Telegram не отдал файл эмодзи.".to_owned()));
        }
    }
    if let Some(thumbnail) = sticker
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("file_id"))
        .and_then(Value::as_str)
        .filter(|file_id| !file_id.is_empty())
    {
        return Ok((thumbnail.to_owned(), "webp"));
    }
    file_id
        .map(|file_id| (file_id.to_owned(), "webp"))
        .ok_or_else(|| EmojiError::NotFound("Telegram не отдал файл эмодзи.".to_owned()))
}

fn media_type(extension: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(known, _)| *known == extension)
        .map_or("image/webp", |(_, media_type)| *media_type)
}

/// Write through `<name>.part` and rename, so a concurrent request never reads
/// a partial picture.
async fn write_atomically(path: &Path, temporary: &Path, data: &[u8]) -> Result<(), EmojiError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(temporary, data).await?;
    if let Err(error) = tokio::fs::rename(temporary, path).await {
        // Another request may have won the race between the miss and the rename.
        // Its file is complete either way; only a missing destination is a failure.
        let existing = tokio::fs::metadata(path)
            .await
            .is_ok_and(|metadata| metadata.is_file());
        let _ = tokio::fs::remove_file(temporary).await;
        if !existing {
            return Err(EmojiError::Io(error));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_and_short_names_normalize_and_stay_bounded() {
        assert_eq!(
            normalize(" https://t.me/addemoji/richpost_test_emoji "),
            "richpost_test_emoji"
        );
        assert_eq!(normalize("@t.me/addemoji/cats"), "cats");
        assert_eq!(normalize("plain_name"), "plain_name");
        assert!(valid_short("a_b9"));
        assert!(!valid_short(""));
        assert!(!valid_short("../etc/passwd"));
        assert!(!valid_short(&"x".repeat(65)));
        assert!(valid_id("1111111111111111111"));
        assert!(!valid_id(&"9".repeat(33)));
        assert!(!valid_id("not-an-id"));
    }

    #[test]
    fn animated_files_keep_a_thumbnail_for_the_static_view() {
        let video = json!({"file_id":"video","is_video":true,"thumbnail":{"file_id":"thumb"}});
        assert_eq!(
            file_id_for(&video, false).ok(),
            Some(("thumb".to_owned(), "webp"))
        );
        assert_eq!(
            file_id_for(&video, true).ok(),
            Some(("video".to_owned(), "webm"))
        );
        let animated = json!({"file_id":"tgs","is_animated":true,"thumbnail":{"file_id":"thumb"}});
        assert_eq!(
            file_id_for(&animated, true).ok(),
            Some(("tgs".to_owned(), "tgs"))
        );
        let plain = json!({"file_id":"static"});
        assert_eq!(
            file_id_for(&plain, false).ok(),
            Some(("static".to_owned(), "webp"))
        );
        assert_eq!(
            file_id_for(&plain, true).ok(),
            Some(("static".to_owned(), "webp"))
        );
    }
}
