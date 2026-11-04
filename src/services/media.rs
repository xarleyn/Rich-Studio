//! Медиа учётки: файлы на диске, ссылки на них в постах и скачивание по ссылке.
//!
//! Фасад: подмодули приватные, публичные пути `crate::services::media::*` сохраняются.
//! Политика скачивания — только публичные адреса, каждый редирект проверяется,
//! через системный прокси, если он задан (ADR 0012).

mod fetch;
mod network;
mod references;
mod storage;

use std::fmt;
use std::path::Path;

use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::StoreError;

pub(crate) use network::system_proxy;
pub use network::{internal_name, public_address};

const INLINE: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "mp4", "mov", "webm", "mp3", "m4a", "ogg", "oga", "opus",
    "wav", "flac",
];

#[derive(Debug)]
pub enum MediaError {
    NotFound,
    BadRequest(String),
    TooLarge,
    Capacity,
    Conflict(Vec<String>),
    Fetch(String),
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Store(StoreError),
}

impl fmt::Display for MediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("Not Found"),
            Self::BadRequest(message) | Self::Fetch(message) => formatter.write_str(message),
            Self::TooLarge => {
                formatter.write_str("файл больше разрешённого лимита в МБ (MAX_UPLOAD_MB)")
            }
            Self::Capacity => {
                formatter.write_str("Лимит файлов учётки исчерпан. Удалите ненужные файлы.")
            }
            Self::Conflict(users) => write!(formatter, "файл ещё нужен: {}", users.join(", ")),
            Self::Io(error) => error.fmt(formatter),
            Self::Sqlite(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MediaError {}

impl From<std::io::Error> for MediaError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for MediaError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<StoreError> for MediaError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone)]
pub struct MediaService {
    context: AccountContext,
    max_upload_bytes: usize,
    account_storage_bytes: usize,
}

impl MediaService {
    pub async fn run<T, F>(&self, action: F) -> Result<T, MediaError>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Result<T, MediaError> + Send + 'static,
    {
        let service = self.clone();
        tokio::task::spawn_blocking(move || action(service))
            .await
            .map_err(|_| MediaError::Fetch("операция файлов прервана".into()))?
    }
    pub fn new(context: AccountContext, max_upload_mb: f64, account_storage_mb: f64) -> Self {
        Self {
            context,
            max_upload_bytes: (max_upload_mb * 1024.0 * 1024.0) as usize,
            account_storage_bytes: (account_storage_mb * 1024.0 * 1024.0) as usize,
        }
    }
}

pub fn describe(name: &str) -> Value {
    json!({"name":name,"kind":kind(name),"url":format!("/media/{name}"),"ref":format!("upload:{name}")})
}

pub fn kind(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "png" | "webp" => "photo",
        "mp4" | "mov" | "webm" | "mkv" => "video",
        "gif" => "animation",
        "mp3" | "m4a" | "flac" | "wav" => "audio",
        "ogg" | "oga" | "opus" => "voice_note",
        _ => "document",
    }
}

pub fn inline(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|extension| INLINE.contains(&extension.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}
