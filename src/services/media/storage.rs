//! Файлы учётки: загрузка, чтение, список, удаление, свободное имя и квота.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::Regex;
use serde_json::{Value, json};

use super::{MediaError, MediaService, describe};
use crate::account_context::{media_file_path, read_media_file};

impl MediaService {
    pub fn upload(&self, filename: &str, data: &[u8]) -> Result<Value, MediaError> {
        if data.len() > self.max_upload_bytes {
            return Err(MediaError::TooLarge);
        }
        self.check_capacity(data.len())?;
        fs::create_dir_all(&self.context.media_dir)?;
        let name = self.create_unique(filename, |path| write_new(path, data))?;
        Ok(describe(&name))
    }

    pub fn download(&self, name: &str) -> Result<Vec<u8>, MediaError> {
        Ok(read_media_file(&self.path_of(name)?)?)
    }

    pub fn path_of(&self, name: &str) -> Result<PathBuf, MediaError> {
        media_file_path(&self.context.media_dir, name).ok_or(MediaError::NotFound)
    }

    pub fn list(&self) -> Result<Vec<Value>, MediaError> {
        let mut files = Vec::new();
        if self.context.media_dir.exists() {
            for entry in fs::read_dir(&self.context.media_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let metadata = entry.metadata()?;
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, name, metadata.len()));
            }
        }
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let used = self.upload_refs()?;
        Ok(files
            .into_iter()
            .map(|(_, name, size)| {
                let mut item = describe(&name);
                item["size"] = json!(size);
                item["used"] = json!(used.contains(&name));
                item
            })
            .collect())
    }

    pub fn delete(&self, name: &str, force: bool) -> Result<(), MediaError> {
        if !force {
            let used = self.used_by(name)?;
            if !used.is_empty() {
                return Err(MediaError::Conflict(used));
            }
        }
        let path = self.path_of(name)?;
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn delete_unused(&self) -> Result<Vec<String>, MediaError> {
        let unused: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|file| file["used"] == false)
            .filter_map(|file| file["name"].as_str().map(str::to_owned))
            .collect();
        for name in &unused {
            fs::remove_file(self.path_of(name)?)?;
        }
        Ok(unused)
    }

    pub(super) fn create_unique(
        &self,
        filename: &str,
        create: impl Fn(&Path) -> io::Result<()>,
    ) -> Result<String, MediaError> {
        let leaf = Path::new(filename)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let safe =
            Regex::new(r"[^\w.\-]+").unwrap_or_else(|error| panic!("filename regex: {error}"));
        let base = safe
            .replace_all(&leaf, "_")
            .trim_matches(['.', '_'])
            .to_owned();
        let base = if base.is_empty() {
            "file".to_owned()
        } else {
            base
        };
        let path = Path::new(&base);
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let extension = path
            .extension()
            .map(|value| format!(".{}", value.to_string_lossy()))
            .unwrap_or_default();
        let mut name = base.clone();
        let mut index = 1;
        loop {
            match create(&self.context.media_dir.join(&name)) {
                Ok(()) => return Ok(name),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            name = format!("{stem}-{index}{extension}");
            index += 1;
        }
    }

    pub(super) fn check_capacity(&self, additional: usize) -> Result<(), MediaError> {
        if self.context.account_id.is_none() || self.account_storage_bytes == 0 {
            return Ok(());
        }
        let used = if self.context.media_dir.exists() {
            fs::read_dir(&self.context.media_dir)?
                .filter_map(Result::ok)
                .filter_map(|entry| entry.metadata().ok())
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len() as usize)
                .sum()
        } else {
            0
        };
        if used + additional > self.account_storage_bytes {
            return Err(MediaError::Capacity);
        }
        Ok(())
    }
}

pub(super) fn write_new(path: &Path, mut data: impl Read) -> io::Result<()> {
    let mut file = fs::File::create_new(path)?;
    if let Err(error) = io::copy(&mut data, &mut file) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}
