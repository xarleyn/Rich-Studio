use std::fmt;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::Store;

const INLINE: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "mp4", "mov", "webm", "mp3", "m4a", "ogg", "oga", "opus",
    "wav", "flac",
];
const INTERNAL_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".home.arpa",
    ".corp",
    ".private",
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
            Self::TooLarge => formatter.write_str("файл больше разрешённого MAX_UPLOAD_MB"),
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

struct FetchFile {
    path: PathBuf,
    target: Option<PathBuf>,
    file: Option<fs::File>,
    keep: bool,
}
impl Drop for FetchFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.keep {
            let _ = fs::remove_file(&self.path);
            if let Some(target) = &self.target {
                let _ = fs::remove_file(target);
            }
        }
    }
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

    pub fn upload(&self, filename: &str, data: &[u8]) -> Result<Value, MediaError> {
        if data.len() > self.max_upload_bytes {
            return Err(MediaError::TooLarge);
        }
        self.check_capacity(data.len())?;
        fs::create_dir_all(&self.context.media_dir)?;
        let name = self.free_name(filename);
        fs::write(self.context.media_dir.join(&name), data)?;
        Ok(describe(&name))
    }

    pub fn download(&self, name: &str) -> Result<Vec<u8>, MediaError> {
        Ok(fs::read(self.path_of(name)?)?)
    }

    pub fn path_of(&self, name: &str) -> Result<PathBuf, MediaError> {
        let path = Path::new(name);
        if name.is_empty()
            || path.is_absolute()
            || path.components().count() != 1
            || name == "."
            || name == ".."
        {
            return Err(MediaError::NotFound);
        }
        let path = self.context.media_dir.join(name);
        if !path.is_file() {
            return Err(MediaError::NotFound);
        }
        Ok(path)
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

    pub fn used_by(&self, name: &str) -> Result<Vec<String>, MediaError> {
        let reference = format!("upload:{name}");
        let mut connection = Store::connect(&self.context)?;
        crate::store::db::migrate(&mut connection)?;
        let mut output = Vec::new();
        let mut drafts = connection.prepare("select title,payload from drafts")?;
        let rows = drafts.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (title, payload) = row?;
            if payload.contains(&reference) {
                output.push(format!(
                    "черновик «{}»",
                    if title.is_empty() {
                        "Без названия"
                    } else {
                        &title
                    }
                ));
            }
        }
        drop(drafts);
        let mut posts = connection.prepare(
            "select status,chat_id,payload from posts where status in ('scheduled','missed')",
        )?;
        let rows = posts.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (status, chat_id, payload) = row?;
            if payload.contains(&reference) {
                output.push(format!(
                    "{} пост в {chat_id}",
                    if status == "scheduled" {
                        "отложенный"
                    } else {
                        "пропущенный"
                    }
                ));
            }
        }
        drop(posts);
        let mut templates =
            connection.prepare("select title,payload,fields from post_templates")?;
        let rows = templates.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (title, payload, fields) = row?;
            if payload.contains(&reference) || fields.contains(&reference) {
                output.push(format!("шаблон «{title}»"));
            }
        }
        Ok(output)
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

    pub async fn fetch(
        &self,
        url: &str,
        filename: Option<&str>,
        public_only: bool,
        allow_private: bool,
    ) -> Result<Value, MediaError> {
        let mut url = Url::parse(url)
            .map_err(|_| MediaError::BadRequest("url: нужна ссылка http(s)".to_owned()))?;
        let suggested = filename
            .map(str::to_owned)
            .or_else(|| {
                url.path_segments()
                    .and_then(Iterator::last)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "file".to_owned());
        let (name, part) = self
            .run(move |service| {
                let name = service.free_name(&suggested);
                fs::create_dir_all(&service.context.media_dir)?;
                let path = service
                    .context
                    .media_dir
                    .join(format!(".{name}.{}.part", Uuid::new_v4().simple()));
                let mut part = FetchFile {
                    path,
                    target: None,
                    file: None,
                    keep: false,
                };
                part.file = Some(
                    fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&part.path)?,
                );
                Ok((name, part))
            })
            .await?;
        let mut part = self
            .fetch_to_part(&mut url, part, public_only, allow_private)
            .await?;
        self.run(move |service| {
            if let Some(mut file) = part.file.take() {
                file.flush()?;
            }
            let final_name = service.free_name(&name);
            let target = service.context.media_dir.join(&final_name);
            part.target = Some(target.clone());
            fs::rename(&part.path, target)?;
            part.keep = true;
            drop(part);
            Ok(describe(&final_name))
        })
        .await
    }

    async fn fetch_to_part(
        &self,
        url: &mut Url,
        mut part: FetchFile,
        public_only: bool,
        allow_private: bool,
    ) -> Result<FetchFile, MediaError> {
        for _ in 0..=5 {
            if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() {
                return Err(MediaError::BadRequest(
                    "url: нужна ссылка http(s)".to_owned(),
                ));
            }
            let host = url.host_str().unwrap_or_default().to_owned();
            if internal_name(&host) {
                return Err(refused(&host));
            }
            let port = url.port_or_known_default().unwrap_or(80);
            let proxied = system_proxy(url.scheme(), &host);
            let addresses = match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(addresses) => addresses.collect::<Vec<_>>(),
                Err(_) if proxied => Vec::new(),
                Err(_) => {
                    return Err(MediaError::Fetch(format!(
                        "не удалось скачать: адрес {host} не найден"
                    )));
                }
            };
            let checked = if addresses.is_empty() {
                None
            } else if public_only || !allow_private || self.context.account_id.is_some() {
                Some(public_address(
                    &addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>(),
                    &host,
                )?)
            } else {
                None
            };
            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("RichStudio/0.1.0 (media fetch)");
            if let Some(address) = checked {
                builder = builder.resolve(&host, SocketAddr::new(address, port));
            }
            let client = builder
                .build()
                .map_err(|_| MediaError::Fetch("не удалось настроить скачивание".into()))?;
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|_| MediaError::Fetch("не удалось скачать: сеть недоступна".into()))?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        MediaError::Fetch("не удалось скачать: пустой redirect".to_owned())
                    })?;
                *url = url
                    .join(location)
                    .map_err(|_| MediaError::BadRequest("url: неверный redirect".to_owned()))?;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err(MediaError::Fetch(format!(
                    "не удалось скачать: HTTP {}",
                    response.status()
                )));
            }
            let mut response = response;
            let mut size = 0usize;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| MediaError::Fetch("не удалось скачать: передача прервана".into()))?
            {
                size += chunk.len();
                if size > self.max_upload_bytes {
                    return Err(MediaError::TooLarge);
                }
                part = self
                    .run(move |service| {
                        service.check_capacity(chunk.len())?;
                        part.file
                            .as_mut()
                            .ok_or_else(|| MediaError::Fetch("файл закрыт".into()))?
                            .write_all(&chunk)?;
                        Ok(part)
                    })
                    .await?;
            }
            return Ok(part);
        }
        Err(MediaError::Fetch(
            "не удалось скачать: больше 5 перенаправлений".to_owned(),
        ))
    }

    fn free_name(&self, filename: &str) -> String {
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
        let mut name = base.clone();
        let mut index = 1;
        while self.context.media_dir.join(&name).exists() {
            let path = Path::new(&base);
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let extension = path
                .extension()
                .map(|value| format!(".{}", value.to_string_lossy()))
                .unwrap_or_default();
            name = format!("{stem}-{index}{extension}");
            index += 1;
        }
        name
    }

    fn check_capacity(&self, additional: usize) -> Result<(), MediaError> {
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

    fn upload_refs(&self) -> Result<std::collections::HashSet<String>, MediaError> {
        let mut connection = Store::connect(&self.context)?;
        crate::store::db::migrate(&mut connection)?;
        let pattern = Regex::new(r#""upload:([^"\\]+)""#)
            .unwrap_or_else(|error| panic!("upload regex: {error}"));
        let mut refs = std::collections::HashSet::new();
        for query in [
            "select payload from drafts",
            "select payload from post_templates",
            "select fields from post_templates",
            "select payload from posts where status in ('scheduled','sending','missed','sent')",
        ] {
            let mut statement = connection.prepare(query)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                for capture in pattern.captures_iter(&row?) {
                    refs.insert(capture[1].to_owned());
                }
            }
        }
        Ok(refs)
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

pub fn internal_name(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return false;
    }
    let name = host.to_lowercase();
    let name = name.trim_end_matches('.');
    !name.contains('.')
        || INTERNAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

pub(crate) fn system_proxy(scheme: &str, host: &str) -> bool {
    let variable = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .unwrap_or_default()
    };
    let no_proxy = variable("NO_PROXY");
    let host = host.trim_end_matches('.').to_lowercase();
    let bypassed = no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let entry = entry
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_lowercase();
            entry == "*" || host == entry || host.ends_with(&format!(".{entry}"))
        });
    !bypassed
        && (!variable(if scheme == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        })
        .is_empty()
            || !variable("ALL_PROXY").is_empty())
}

pub fn public_address(addresses: &[IpAddr], host: &str) -> Result<IpAddr, MediaError> {
    if addresses.is_empty() {
        return Err(MediaError::Fetch(format!(
            "не удалось скачать: адрес {host} не найден"
        )));
    }
    for address in addresses {
        let address = match address {
            IpAddr::V6(address) => address
                .to_ipv4_mapped()
                .map_or(IpAddr::V6(*address), IpAddr::V4),
            address => *address,
        };
        let non_public = match address {
            IpAddr::V4(address) => {
                let octets = address.octets();
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_broadcast()
                    || address.is_documentation()
                    || address.is_unspecified()
                    || address.is_multicast()
                    || octets[0] == 0
                    || octets[0] >= 240
                    || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                    || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                    || (octets[0] == 198 && (18..=19).contains(&octets[1]))
            }
            IpAddr::V6(address) => {
                address.is_unspecified()
                    || address.is_loopback()
                    || address.is_multicast()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
                    || (address.segments()[0] & 0xffc0) == 0xfe80
                    || address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8
            }
        };
        if non_public {
            return Err(refused(host));
        }
    }
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .copied()
        .or_else(|| addresses.first().copied())
        .ok_or_else(|| MediaError::Fetch(format!("не удалось скачать: адрес {host} не найден")))
}

fn refused(host: &str) -> MediaError {
    MediaError::BadRequest(format!(
        "url: {host} — локальный или внутренний адрес, студия туда не ходит (свой сервер в локальной сети: MEDIA_FETCH_PRIVATE=1)"
    ))
}
