use super::{MAX_FILES, MAX_TOTAL_BYTES};
use crate::{
    account_context::AccountContext, errors::ImportError, settings::Settings,
    telegram::import_client::rpc_error,
};
use futures_util::{Stream, StreamExt};
use grammers_client::{
    Client,
    media::{Document, Photo},
};
use grammers_tl_types as tl;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

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

pub(super) fn io_error(_: std::io::Error) -> ImportError {
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

pub(super) async fn download_photo(
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
pub(super) async fn download_document(
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
