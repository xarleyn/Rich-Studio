//! Structured, redacted logs written by a bounded worker outside Tokio threads.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

use crate::{metrics::Metrics, settings::Settings};

tokio::task_local! { static REQUEST_ID: String; }
const LEVELS: &[&str] = &["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"];

#[derive(Deserialize)]
pub struct ReadQuery {
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub since: Option<f64>,
    #[serde(default)]
    pub logger: String,
    #[serde(default)]
    pub q: String,
}
fn default_level() -> String {
    "INFO".to_owned()
}
fn default_limit() -> usize {
    100
}

pub async fn read_checked(
    logger: Arc<Logger>,
    query: ReadQuery,
    max: usize,
) -> Result<Vec<Value>, String> {
    if !LEVELS.contains(&query.level.as_str())
        || !(1..=max).contains(&query.limit)
        || query
            .since
            .is_some_and(|since| !since.is_finite() || !(0.0..=4102444800.0).contains(&since))
    {
        return Err("неверные параметры журнала".to_owned());
    }
    tokio::task::spawn_blocking(move || {
        logger.read(
            &query.level,
            query.limit,
            query.since,
            &query.logger,
            &query.q,
        )
    })
    .await
    .map_err(|_| "log worker failed".to_owned())?
    .map_err(|_| "журнал недоступен".to_owned())
}

pub async fn scope<F: std::future::Future>(id: String, future: F) -> F::Output {
    REQUEST_ID.scope(id, future).await
}
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}
pub fn incoming_id(id: Option<&str>, traceparent: Option<&str>) -> String {
    if let Some(id) = id.filter(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
    }) {
        return id.to_owned();
    }
    if let Some(trace) = traceparent {
        let parts: Vec<&str> = trace.trim().split('-').collect();
        if parts.len() >= 4
            && parts[0].len() == 2
            && parts[0] != "ff"
            && (parts[0] != "00" || parts.len() == 4)
            && parts[1].len() == 32
            && parts[2].len() == 16
            && parts[3].len() == 2
            && parts.iter().take(4).all(|part| {
                part.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            && parts[1].bytes().any(|c| c != b'0')
            && parts[2].bytes().any(|c| c != b'0')
        {
            return parts[1].to_owned();
        }
    }
    new_id("")
}

pub fn timestamp(now: time::OffsetDateTime) -> String {
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

pub fn scrub(settings: &Settings, text: &str) -> String {
    let mut text = text.to_owned();
    for secret in [
        &settings.bot_token,
        &settings.api_key,
        &settings.import_api_hash,
        &settings.import_proxy,
        &settings.webhook_secret,
        &settings.deadman_url,
    ] {
        if !secret.is_empty() {
            text = text.replace(secret, "<secret>");
        }
    }
    static PATTERNS: OnceLock<Result<Vec<Regex>, regex::Error>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"/bot[0-9]{1,12}(?::|%3[Aa])[A-Za-z0-9_-]+",
            r"\b[0-9]{5,12}:[A-Za-z0-9_-]{30,}",
            r#"(?i)Bearer\s+[^\s\"'<>]+"#,
            r"\brsu?_[A-Za-z0-9_-]{20,}",
        ]
        .into_iter()
        .map(Regex::new)
        .collect()
    });
    let Ok(patterns) = patterns else {
        return "<redacted>".to_owned();
    };
    for pattern in patterns {
        text = pattern.replace_all(&text, "<secret>").into_owned();
    }
    text
}

fn sensitive(key: &str) -> bool {
    [
        "authorization",
        "cookie",
        "token",
        "secret",
        "password",
        "session",
        "payload",
        "post",
        "body",
        "config",
    ]
    .contains(&key.to_ascii_lowercase().as_str())
}
pub fn scrub_all(settings: &Settings, value: &Value) -> Value {
    match value {
        Value::String(value) => json!(scrub(settings, value)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| scrub_all(settings, value))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if sensitive(key) {
                            json!("<redacted>")
                        } else {
                            scrub_all(settings, value)
                        },
                    )
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

enum Command {
    Event(Value),
    Flush(mpsc::Sender<()>),
    Stop,
}
struct Inner {
    settings: Settings,
    file: Option<PathBuf>,
    sender: Option<mpsc::SyncSender<Command>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
    write_errors: Arc<AtomicU64>,
    metrics: Arc<Metrics>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Command::Stop);
        }
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}
#[derive(Clone)]
pub struct Logger(Arc<Inner>);
impl std::fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Logger")
            .field("file", &self.0.file)
            .finish_non_exhaustive()
    }
}

impl Logger {
    pub fn new(settings: &Settings, metrics: Arc<Metrics>) -> Self {
        let file = (settings.log_max_mb > 0.0)
            .then(|| settings.data_dir.join("logs/studio.log"))
            .filter(|path| prepare(path).is_ok());
        let (sender, receiver) = mpsc::sync_channel(1024);
        let worker_settings = settings.clone();
        let worker_file = file.clone();
        let write_errors = Arc::new(AtomicU64::new(u64::from(
            settings.log_max_mb > 0.0 && file.is_none(),
        )));
        let worker_errors = write_errors.clone();
        let worker = std::thread::Builder::new()
            .name("studio-log".to_owned())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        Command::Event(event) => {
                            eprintln!(
                                "{} {}: {}",
                                event["level"].as_str().unwrap_or_default(),
                                event["logger"].as_str().unwrap_or_default(),
                                event["msg"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .replace(['\r', '\n'], " ")
                            );
                            if let Some(path) = &worker_file
                                && write_event(&worker_settings, path, &event).is_err()
                            {
                                worker_errors.fetch_add(1, Ordering::Relaxed);
                                eprintln!("WARNING rich-studio.logs: journal write failed");
                            }
                        }
                        Command::Flush(done) => {
                            let _ = done.send(());
                        }
                        Command::Stop => break,
                    }
                }
            })
            .ok();
        let sender = worker.as_ref().map(|_| sender);
        Self(Arc::new(Inner {
            settings: settings.clone(),
            file,
            sender,
            worker: Mutex::new(worker),
            dropped: AtomicU64::new(0),
            write_errors,
            metrics,
        }))
    }
    pub fn event(&self, level: &str, logger: &str, message: &str, fields: Value) {
        let level = if level == "WARN" { "WARNING" } else { level };
        if rank(level) < rank(&self.0.settings.log_level) {
            return;
        }
        let mut event = json!({"ts":timestamp(time::OffsetDateTime::now_utc()),"level":level,"logger":logger,"msg":message});
        if let Some(fields) = fields.as_object() {
            for (key, value) in fields {
                if !["ts", "level", "logger", "msg"].contains(&key.as_str()) {
                    event[key] = value.clone();
                }
            }
        }
        if let Ok(id) = REQUEST_ID.try_with(Clone::clone) {
            event["request_id"] = json!(id);
        }
        let event = scrub_all(&self.0.settings, &event);
        if rank(level) >= rank("WARNING") {
            self.0.metrics.problem(&event);
        }
        if let Some(sender) = &self.0.sender {
            if sender.try_send(Command::Event(event)).is_err() {
                self.0.dropped.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            eprintln!("{event}");
        }
    }
    pub fn install_tracing(&self) -> Result<(), String> {
        tracing_subscriber::registry()
            .with(LogLayer(self.clone()))
            .try_init()
            .map_err(|_| "tracing subscriber already installed".to_owned())
    }
    pub fn current_file(&self) -> Option<&Path> {
        self.0.file.as_deref()
    }
    pub fn dropped(&self) -> u64 {
        self.0.dropped.load(Ordering::Relaxed)
    }
    pub fn write_errors(&self) -> u64 {
        self.0.write_errors.load(Ordering::Relaxed)
    }
    pub fn flush(&self) -> io::Result<()> {
        if let Some(sender) = &self.0.sender {
            let (done, wait) = mpsc::channel();
            sender
                .send(Command::Flush(done))
                .map_err(|_| io::Error::other("log worker stopped"))?;
            wait.recv_timeout(Duration::from_secs(5))
                .map_err(|_| io::Error::other("log flush timeout"))?;
        }
        Ok(())
    }
    fn files(&self) -> Vec<PathBuf> {
        let Some(file) = &self.0.file else {
            return vec![];
        };
        let mut paths: Vec<PathBuf> = (1..=self.0.settings.log_files.max(1))
            .rev()
            .map(|copy| file.with_file_name(format!("studio.log.{copy}")))
            .collect();
        paths.push(file.clone());
        paths
    }
    pub fn disk_bytes(&self) -> u64 {
        self.files()
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum()
    }
    pub fn read(
        &self,
        level: &str,
        limit: usize,
        since: Option<f64>,
        logger: &str,
        text: &str,
    ) -> io::Result<Vec<Value>> {
        self.flush()?;
        let since = match since {
            Some(value) if !value.is_finite() || !(0.0..=4102444800.0).contains(&value) => {
                return Ok(vec![]);
            }
            Some(value) => time::OffsetDateTime::from_unix_timestamp(value as i64)
                .ok()
                .map(timestamp),
            None => None,
        };
        let mut found = VecDeque::new();
        let query = text.to_lowercase();
        for path in self.files() {
            let file = match fs::File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for raw in io::BufReader::new(file).lines() {
                let raw = raw?;
                let Ok(line) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                if rank(line["level"].as_str().unwrap_or_default()) < rank(level)
                    || !line["logger"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with(logger)
                    || since.as_ref().is_some_and(|since| {
                        line["ts"].as_str().unwrap_or_default() < since.as_str()
                    })
                    || (!query.is_empty() && !raw.to_lowercase().contains(&query))
                {
                    continue;
                }
                if found.len() >= limit.clamp(1, 1000) {
                    found.pop_front();
                }
                found.push_back(scrub_all(&self.0.settings, &line));
            }
        }
        Ok(found.into_iter().collect())
    }
}
fn rank(level: &str) -> usize {
    LEVELS.iter().position(|known| *known == level).unwrap_or(1)
}
fn prepare(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    open(path).map(|_| ())
}
fn open(path: &Path) -> io::Result<fs::File> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::other("log path must not be a symlink"));
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}
fn write_event(settings: &Settings, path: &Path, event: &Value) -> io::Result<()> {
    let line = serde_json::to_vec(event)?;
    let max = (settings.log_max_mb * 1024.0 * 1024.0) as u64;
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() + line.len() as u64 + 1 > max) {
        for index in (1..=settings.log_files.max(1)).rev() {
            let source = if index == 1 {
                path.to_path_buf()
            } else {
                path.with_file_name(format!("studio.log.{}", index - 1))
            };
            let dest = path.with_file_name(format!("studio.log.{index}"));
            match fs::remove_file(&dest) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match fs::rename(source, dest) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    let mut file = open(path)?;
    file.write_all(&line)?;
    file.write_all(b"\n")?;
    file.flush()
}

struct LogLayer(Logger);
impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        if !metadata.target().starts_with("rich_studio")
            && !metadata.target().starts_with("rich-studio")
        {
            return;
        }
        let mut visitor = Fields(serde_json::Map::new());
        event.record(&mut visitor);
        let message = visitor
            .0
            .remove("message")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.0.event(
            metadata.level().as_str(),
            metadata.target(),
            &message,
            Value::Object(visitor.0),
        );
    }
}
struct Fields(serde_json::Map<String, Value>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_owned(),
            if sensitive(field.name()) {
                json!("<redacted>")
            } else {
                json!(format!("{value:?}"))
            },
        );
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), json!(value));
    }
}
