mod download;
mod summary;

#[cfg(test)]
mod tests;

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{check_spec_shape, version_tuple};
use crate::settings::Settings;

const BUNDLED: &str = include_str!("../../app/bundled/api.json");

#[derive(Clone, Debug)]
pub struct Spec {
    pub data: Value,
    pub source: String,
    pub loaded_at: f64,
    pub changes: Vec<Value>,
    pub last_check: Option<Value>,
}

impl Spec {
    pub fn load_initial(settings: &Settings) -> Self {
        let mut best = serde_json::from_str::<Value>(BUNDLED)
            .ok()
            .filter(|data| check_spec_shape(data).is_none())
            .map(|data| (data, "bundled".to_owned(), 0.0));
        for path in (!settings.spec_pinned)
            .then(|| settings.data_dir.join("api.json"))
            .into_iter()
        {
            let Ok(raw) = fs::read(&path) else { continue };
            let Ok(data) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            if check_spec_shape(&data).is_some() {
                continue;
            }
            let newer = best.as_ref().is_none_or(|(current, _, _)| {
                version_tuple(data["version"].as_str().unwrap_or_default())
                    >= version_tuple(current["version"].as_str().unwrap_or_default())
            });
            if newer {
                let loaded_at = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0.0, |duration| duration.as_secs_f64());
                best = Some((data, "cache".to_owned(), loaded_at));
            }
        }
        let (data, source, loaded_at) = best.unwrap_or_else(|| {
            (
                json!({"version": "?", "types": {}, "methods": {}}),
                "missing".to_owned(),
                0.0,
            )
        });
        let changes = fs::read(settings.data_dir.join("spec_changes.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Self {
            data,
            source,
            loaded_at,
            changes,
            last_check: None,
        }
    }

    pub fn version(&self) -> &str {
        self.data
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("?")
    }

    pub fn types(&self) -> &Value {
        self.data.get("types").unwrap_or(&Value::Null)
    }

    pub fn methods(&self) -> &Value {
        self.data.get("methods").unwrap_or(&Value::Null)
    }

    pub fn definition(&self, name: &str) -> Option<Value> {
        self.types()
            .get(name)
            .or_else(|| self.methods().get(name))
            .cloned()
    }

    pub async fn refresh(&mut self, settings: &Settings, force: bool) -> Value {
        if settings.spec_pinned {
            return json!({
                "updated": false,
                "version": self.version(),
                "pinned": true,
                "error": "спецификация закреплена (SPEC_PIN): новая версия приходит с обновлением образа"
            });
        }
        if !force && self.source == "remote" {
            let age = now() - self.loaded_at;
            if age < settings.spec_refresh_hours * 3600.0 {
                return json!({"updated": false, "version": self.version(), "reason": "fresh"});
            }
        }
        let report = self.download(settings).await;
        self.last_check = Some(
            json!({"at": now() as i64, "error": report.get("error").cloned().unwrap_or(Value::Null)}),
        );
        report
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
