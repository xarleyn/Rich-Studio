use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::account_context::media_file_path;
use crate::telegram::TelegramError;

const FILE_KEYS: &[&str] = &["media", "photo", "live_photo", "thumbnail", "cover"];

#[derive(Default, Debug)]
pub struct Uploads {
    pub files: Vec<(String, PathBuf)>,
}

#[derive(Debug)]
pub struct UploadError(String);

impl fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UploadError {}

pub fn collect_uploads(
    value: &Value,
    media_dir: &Path,
    uploads: &mut Uploads,
) -> Result<Value, UploadError> {
    collect(value, media_dir, uploads, "")
}

fn collect(
    value: &Value,
    media_dir: &Path,
    uploads: &mut Uploads,
    key: &str,
) -> Result<Value, UploadError> {
    match value {
        Value::Object(object) => Ok(Value::Object(
            object
                .iter()
                .map(|(key, value)| Ok((key.clone(), collect(value, media_dir, uploads, key)?)))
                .collect::<Result<_, UploadError>>()?,
        )),
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|value| collect(value, media_dir, uploads, key))
                .collect::<Result<_, _>>()?,
        )),
        Value::String(reference)
            if FILE_KEYS.contains(&key) && reference.starts_with("upload:") =>
        {
            let name = &reference[7..];
            let path = media_file_path(media_dir, name)
                .ok_or_else(|| UploadError(format!("файл {name:?} не найден в загрузках")))?;
            if let Some((key, _)) = uploads.files.iter().find(|(_, existing)| existing == &path) {
                return Ok(Value::String(format!("attach://{key}")));
            }
            let key = format!("f{}", uploads.files.len());
            uploads.files.push((key.clone(), path));
            Ok(Value::String(format!("attach://{key}")))
        }
        _ => Ok(value.clone()),
    }
}

impl From<UploadError> for TelegramError {
    fn from(error: UploadError) -> Self {
        Self::MissingUpload(error.0)
    }
}
