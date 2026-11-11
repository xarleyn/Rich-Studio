use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::summary::gaps;
use super::{Spec, now};
use crate::settings::Settings;
use crate::spec::diff::diff_specs;
use crate::spec::{check_spec_shape, version_tuple};

impl Spec {
    pub(super) async fn download(&mut self, settings: &Settings) -> Value {
        let new = async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| "specification HTTP client failed".to_owned())?;
            let mut response = client
                .get(&settings.spec_url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|error| error.without_url().to_string())?;
            const LIMIT: usize = 32 * 1024 * 1024;
            if response
                .content_length()
                .is_some_and(|length| length > LIMIT as u64)
            {
                return Err("specification exceeds 32 MiB".to_owned());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| error.without_url().to_string())?
            {
                if bytes.len().saturating_add(chunk.len()) > LIMIT {
                    return Err("specification exceeds 32 MiB".to_owned());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string())
        }
        .await;
        let new = match new {
            Ok(new) => new,
            Err(error) => {
                return json!({"updated": false, "version": self.version(), "error": error});
            }
        };
        if let Some(problem) = check_spec_shape(&new) {
            return json!({"updated": false, "version": self.version(), "error": format!("скачанная спецификация отклонена: {problem}")});
        }
        if version_tuple(new["version"].as_str().unwrap_or_default())
            < version_tuple(self.version())
        {
            return json!({"updated": false, "version": self.version(), "error": format!("скачана более старая версия {}", new["version"].as_str().unwrap_or_default())});
        }
        let old = self.data.clone();
        let differences = diff_specs(&old, &new);
        let changed = old["version"] != new["version"] || !differences.is_empty();
        let mut changes = self.changes.clone();
        if changed && !differences.is_empty() {
            changes.insert(0,json!({"from":old["version"],"to":new["version"],"at":now()as i64,"items":differences}));
        }
        changes.truncate(50);
        let directory = settings.data_dir.clone();
        let content = new.to_string();
        let history = json!(changes).to_string();
        let persisted = tokio::task::spawn_blocking(move || {
            persist_cache(&directory, &content, &history).map_err(|error| error.to_string())
        })
        .await;
        let error = match persisted {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("specification cache worker interrupted".to_owned()),
        };
        if let Some(error) = error {
            return json!({"updated":false,"version":self.version(),"error":error});
        }
        self.data = new;
        self.changes = changes;
        self.source = "remote".to_owned();
        self.loaded_at = now();
        json!({"updated": changed, "version": self.version(), "changes": differences, "gaps": gaps(self.types())})
    }
}

struct PendingCache(PathBuf);

impl PendingCache {
    fn write(directory: &Path, name: &str, data: &[u8]) -> std::io::Result<Self> {
        let temporary = Self(directory.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4())));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary.0)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(temporary)
    }

    fn replace(&self, destination: &Path) -> std::io::Result<()> {
        fs::rename(&self.0, destination)
    }
}

impl Drop for PendingCache {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn persist_cache(directory: &Path, content: &str, history: &str) -> std::io::Result<()> {
    fs::create_dir_all(directory)?;
    let history_path = directory.join("spec_changes.json");
    let previous_history = match fs::read(&history_path) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let rollback = previous_history
        .as_deref()
        .map(|previous| PendingCache::write(directory, "spec_changes.rollback", previous))
        .transpose()?;
    let api = PendingCache::write(directory, "api.json", content.as_bytes())?;
    let changes = PendingCache::write(directory, "spec_changes.json", history.as_bytes())?;
    // The specification is the commit point: a failed history write must never
    // leave a newer api.json for the next process to load after reporting failure.
    changes.replace(&history_path)?;
    if let Err(error) = api.replace(&directory.join("api.json")) {
        let restored = match rollback {
            Some(previous) => previous.replace(&history_path),
            None => fs::remove_file(&history_path),
        };
        if let Err(restore_error) = restored {
            return Err(std::io::Error::new(
                error.kind(),
                format!("{error}; specification history restore failed: {restore_error}"),
            ));
        }
        return Err(error);
    }
    Ok(())
}
