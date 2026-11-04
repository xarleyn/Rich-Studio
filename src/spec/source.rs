use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use super::diff::{TRACK_METHODS, TRACK_PREFIXES, diff_specs};
use super::{check_spec_shape, enum_values, type_const, version_tuple};
use crate::settings::Settings;

const BUNDLED: &str = include_str!("../../app/bundled/api.json");
const LABELS: &str = include_str!("../../app/static/js/labels.js");
const RENDER: &str = include_str!("../../app/static/render.js");

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

    pub fn summary(&self) -> Value {
        let mut output_types = Map::new();
        for (name, definition) in self.types().as_object().into_iter().flatten() {
            if !TRACK_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
                && ![
                    "Location",
                    "LoginUrl",
                    "SwitchInlineQueryChosenChat",
                    "WebAppInfo",
                    "User",
                    "RichBlockTableCell",
                    "RichBlockCaption",
                ]
                .contains(&name.as_str())
            {
                continue;
            }
            let fields = summary_fields(definition, true);
            let constant = type_const(self.types(), name);
            output_types.insert(
                name.clone(),
                json!({
                    "description": description(definition),
                    "fields": fields,
                    "subtypes": definition.get("subtypes").cloned().unwrap_or_else(|| json!([])),
                    "const": constant.map(|(_, value)| value),
                    "href": definition.get("href").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        let mut methods = Map::new();
        for name in TRACK_METHODS {
            if let Some(definition) = self.methods().get(name) {
                methods.insert(
                    (*name).to_owned(),
                    json!({
                        "description": description(definition),
                        "fields": summary_fields(definition, false),
                        "href": definition.get("href").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
        }
        json!({
            "version": self.version(),
            "release_date": self.data.get("release_date").cloned().unwrap_or(Value::Null),
            "changelog": self.data.get("changelog").cloned().unwrap_or(Value::Null),
            "source": self.source,
            "loaded_at": self.loaded_at as i64,
            "types": output_types,
            "methods": methods,
            "changes": self.changes.iter().take(10).cloned().collect::<Vec<_>>(),
            "gaps": gaps(self.types()),
            "last_check": self.last_check,
        })
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

    async fn download(&mut self, settings: &Settings) -> Value {
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

fn persist_cache(directory: &Path, content: &str, history: &str) -> std::io::Result<()> {
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

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn description(definition: &Value) -> String {
    definition
        .get("description")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary_fields(definition: &Value, enums: bool) -> Vec<Value> {
    definition
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|field| {
            let mut output = json!({
                "name": field.get("name").cloned().unwrap_or(Value::Null),
                "types": field.get("types").cloned().unwrap_or_else(|| json!([])),
                "required": field.get("required").and_then(Value::as_bool).unwrap_or(false),
                "description": field.get("description").cloned().unwrap_or_else(|| json!("")),
            });
            if enums {
                output["enum"] = field
                    .get("description")
                    .and_then(Value::as_str)
                    .and_then(enum_values)
                    .map_or(Value::Null, |values| json!(values));
            }
            output
        })
        .collect()
}

fn consts(types: &Value, base: &str) -> Vec<(String, String)> {
    types
        .get(base)
        .and_then(|definition| definition.get("subtypes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|subtype| {
            let name = subtype.as_str()?;
            let (_, constant) = type_const(types, name)?;
            Some((constant, name.to_owned()))
        })
        .collect()
}

fn object_keys(source: &str, variable: &str) -> Vec<String> {
    let pattern = Regex::new(&format!(r"\b{variable}\s*=\s*\{{([\s\S]*?)\}};"))
        .unwrap_or_else(|error| panic!("JS regex: {error}"));
    let Some(body) = pattern
        .captures(source)
        .and_then(|found| found.get(1))
        .map(|body| body.as_str())
    else {
        return Vec::new();
    };
    Regex::new(r"(?:^|[,{\s])([a-z_]+)\s*:")
        .unwrap_or_else(|error| panic!("JS key regex: {error}"))
        .captures_iter(body)
        .map(|key| key[1].to_owned())
        .collect()
}

fn function_cases(source: &str, function: &str, inline: bool) -> Vec<String> {
    let Some(start) = source.find(&format!("function {function}(")) else {
        return Vec::new();
    };
    let tail = &source[start..];
    let end = tail[1..]
        .find("\n  function ")
        .map_or(tail.len(), |end| end + 1);
    let body = &tail[..end];
    let pattern = if inline {
        r"'([a-z_]+)'"
    } else {
        r"case '([a-z_]+)'"
    };
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("JS case regex: {error}"))
        .captures_iter(body)
        .map(|key| key[1].to_owned())
        .collect()
}

fn gaps(types: &Value) -> Vec<Value> {
    let labels = LABELS;
    let render = RENDER;
    let mut output = Vec::new();
    for (part, file, base, kind) in [
        ("labels", "js/labels.js", "InputRichBlock", "block"),
        ("labels", "js/labels.js", "RichText", "inline"),
        ("preview", "render.js", "InputRichBlock", "block"),
        ("preview", "render.js", "RichText", "inline"),
    ] {
        let (table, function, wraps) = match (part, base) {
            ("labels", "InputRichBlock") => ("BLOCK_LABELS", "", false),
            ("labels", _) => ("RT_LABELS", "", false),
            (_, "InputRichBlock") => ("", "renderBlock", false),
            _ => ("", "renderRT", true),
        };
        let source = if part == "labels" { labels } else { render };
        let mut known = if table.is_empty() {
            function_cases(source, function, kind == "inline")
        } else {
            object_keys(source, table)
        };
        if wraps {
            known.extend(object_keys(source, "WRAP"));
        }
        let where_ = if table.is_empty() { function } else { table };
        for (constant, type_name) in consts(types, base) {
            if !known.contains(&constant) {
                output.push(json!({"part": part, "file": file, "where": where_, "kind": kind, "name": constant, "type": type_name}));
            }
        }
    }
    output.sort_by(|left, right| {
        format!(
            "{}{}{}",
            left["part"].as_str().unwrap_or_default(),
            left["kind"].as_str().unwrap_or_default(),
            left["name"].as_str().unwrap_or_default()
        )
        .cmp(&format!(
            "{}{}{}",
            right["part"].as_str().unwrap_or_default(),
            right["kind"].as_str().unwrap_or_default(),
            right["name"].as_str().unwrap_or_default()
        ))
    });
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::TempDir;

    struct Server {
        url: String,
        stopping: Arc<AtomicBool>,
        task: Option<std::thread::JoinHandle<()>>,
    }
    impl Server {
        fn new(body: &Value) -> Result<Self, Box<dyn std::error::Error>> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}/spec", listener.local_addr()?);
            listener.set_nonblocking(true)?;
            let stopping = Arc::new(AtomicBool::new(false));
            let flag = stopping.clone();
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let task = std::thread::spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                            let mut buffer = [0; 4096];
                            let mut request = Vec::new();
                            while request.len() < 16_384
                                && !request.windows(4).any(|part| part == b"\r\n\r\n")
                            {
                                match stream.read(&mut buffer) {
                                    Ok(0) | Err(_) => break,
                                    Ok(length) => request.extend_from_slice(&buffer[..length]),
                                }
                            }
                            if !request.windows(4).any(|part| part == b"\r\n\r\n") {
                                continue;
                            }
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                url,
                stopping,
                task: Some(task),
            })
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Release);
            if let Some(task) = self.task.take() {
                let _ = task.join();
            }
        }
    }
    fn fixture(version: &str) -> Value {
        json!({"version":version,"types":{"InputRichBlock":{"subtypes":[]}},"methods":{"sendRichMessage":{"fields":[]}}})
    }
    fn config(
        directory: &TempDir,
        server: &Server,
    ) -> Result<Settings, Box<dyn std::error::Error>> {
        Ok(Settings::from_map(&std::collections::BTreeMap::from([
            (
                "DATA_DIR".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ),
            ("SPEC_URL".to_owned(), server.url.clone()),
        ]))?)
    }

    #[tokio::test]
    async fn history_write_failure_preserves_old_cache_bytes_and_restart_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let server = Server::new(&fixture("Bot API 99.0"))?;
        let settings = config(&directory, &server)?;
        let old = serde_json::to_vec_pretty(&fixture("Bot API 98.0"))?;
        fs::write(directory.path().join("api.json"), &old)?;
        fs::create_dir(directory.path().join("spec_changes.json"))?;
        let mut spec = Spec::load_initial(&settings);
        let report = spec.refresh(&settings, true).await;
        assert_eq!(report["updated"], false);
        assert!(report["error"].is_string());
        assert_eq!(spec.version(), "Bot API 98.0");
        assert_eq!(fs::read(directory.path().join("api.json"))?, old);
        assert_eq!(Spec::load_initial(&settings).version(), "Bot API 98.0");
        assert!(directory.path().join("spec_changes.json").is_dir());
        assert!(
            !fs::read_dir(directory.path())?
                .filter_map(Result::ok)
                .any(|file| file.file_name().to_string_lossy().ends_with(".tmp"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_refresh_returns_current_empty_diff_and_retains_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let mut new = fixture("Bot API 99.0");
        new["methods"]["sendRichMessage"]["fields"] =
            json!([{"name":"new_option","types":["Boolean"],"required":false}]);
        let server = Server::new(&new)?;
        let settings = config(&directory, &server)?;
        fs::write(
            directory.path().join("api.json"),
            serde_json::to_vec(&fixture("Bot API 98.0"))?,
        )?;
        let mut spec = Spec::load_initial(&settings);
        let first = spec.refresh(&settings, true).await;
        assert_eq!(first["updated"], true);
        assert!(!first["changes"].as_array().ok_or("changes")?.is_empty());
        let history = spec.changes.clone();
        let second = spec.refresh(&settings, true).await;
        assert_eq!(second["updated"], false);
        assert_eq!(second["changes"], json!([]), "second refresh: {second}");
        assert_eq!(spec.changes, history);
        assert_eq!(Spec::load_initial(&settings).data, new);
        Ok(())
    }

    #[test]
    fn failed_cache_commit_restores_previous_history() -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let history = b"[ { \"from\": \"previous\" } ]\n";
        fs::write(directory.path().join("spec_changes.json"), history)?;
        fs::create_dir(directory.path().join("api.json"))?;
        assert!(persist_cache(directory.path(), "{}", "[]").is_err());
        assert_eq!(
            fs::read(directory.path().join("spec_changes.json"))?,
            history
        );
        assert!(directory.path().join("api.json").is_dir());
        assert!(
            !fs::read_dir(directory.path())?
                .filter_map(Result::ok)
                .any(|file| file.file_name().to_string_lossy().ends_with(".tmp"))
        );
        Ok(())
    }
}

#[test]
fn gaps_report_a_new_rich_block_and_the_bundled_types_are_complete() {
    let bundled = serde_json::from_str::<Value>(BUNDLED).expect("bundled spec");
    let mut types = bundled["types"].clone();
    assert!(consts(&types, "InputRichBlock").len() >= 20);
    assert!(consts(&types, "RichText").len() >= 20);
    types["InputRichBlockHologram"] = json!({
        "name":"InputRichBlockHologram",
        "fields":[{
            "name":"type",
            "types":["String"],
            "required":true,
            "description":"Type of the block, always “hologram”"
        }]
    });
    types["InputRichBlock"]["subtypes"]
        .as_array_mut()
        .expect("block subtypes")
        .push(json!("InputRichBlockHologram"));
    let found = gaps(&types)
        .into_iter()
        .map(|gap| {
            (
                gap["part"].as_str().unwrap_or_default().to_owned(),
                gap["name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(found.contains(&("labels".to_owned(), "hologram".to_owned())));
    assert!(found.contains(&("preview".to_owned(), "hologram".to_owned())));
    assert!(gaps(&bundled["types"]).is_empty());
}
