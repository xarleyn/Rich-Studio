use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Settings {
    pub mode: String,
    pub public: bool,
    pub account_storage_mb: f64,
    pub account_database_mb: f64,
    pub data_dir: PathBuf,
    pub bot_token: String,
    pub api_key: String,
    pub session_days: f64,
    pub log_level: String,
    pub log_files: u64,
    pub allowed_hosts: HashSet<String>,
    pub forwarded_allow_ips: TrustedProxies,
    pub bind: SocketAddr,
    pub default_chat: String,
    pub preview_chat: String,
    pub updates_on: bool,
    pub public_url: String,
    pub spec_url: String,
    pub spec_refresh_hours: f64,
    pub spec_pinned: bool,
    pub max_upload_mb: f64,
    pub media_fetch_private: bool,
    pub api_base: String,
    pub raw_api: bool,
    pub bot_username: String,
    pub owner_telegram_id: i64,
    pub landing_url: String,
    pub mcp_publish: bool,
    pub background: bool,
    pub updates_mode: String,
    pub webhook_secret: String,
    pub import_api_id: i64,
    pub import_api_hash: String,
    pub import_proxy: String,
    pub schedule_late_min: f64,
    pub backup_keep: u64,
    pub log_max_mb: f64,
    pub media_unused_days: f64,
    pub versions_keep: u64,
    pub deadman_url: String,
    pub shutdown_grace: f64,
}

impl fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("mode", &self.mode)
            .field("public", &self.public)
            .field("data_dir", &self.data_dir)
            .field("bind", &self.bind)
            .field("bot_token", &"<redacted>")
            .field("api_key", &"<redacted>")
            .field("webhook_secret", &"<redacted>")
            .field("import_api_hash", &"<redacted>")
            .field("import_proxy", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsError {
    pub problems: Vec<String>,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "настройки не подходят:\n- {}",
            self.problems.join("\n- ")
        )
    }
}

impl std::error::Error for SettingsError {}

#[derive(Clone, Debug, Default)]
pub struct TrustedProxies {
    pub(crate) all: bool,
    networks: Vec<(IpAddr, u8)>,
}

impl std::str::FromStr for TrustedProxies {
    type Err = &'static str;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.trim() == "*" {
            return Ok(Self {
                all: true,
                networks: Vec::new(),
            });
        }
        let mut networks = Vec::new();
        for entry in raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (host, prefix) = entry
                .split_once('/')
                .map_or((entry, None), |(host, prefix)| (host, Some(prefix)));
            let address: IpAddr = host
                .parse()
                .map_err(|_| "нужны IP-адреса, CIDR или отдельное *")?;
            let bits = if address.is_ipv4() { 32 } else { 128 };
            let prefix = prefix
                .map_or(Ok(bits), str::parse::<u8>)
                .map_err(|_| "неверная длина префикса CIDR")?;
            if prefix > bits {
                return Err("неверная длина префикса CIDR");
            }
            networks.push((address, prefix));
        }
        Ok(Self {
            all: false,
            networks,
        })
    }
}

impl TrustedProxies {
    pub fn contains(&self, client: IpAddr) -> bool {
        self.all
            || self
                .networks
                .iter()
                .any(|(network, prefix)| match (network, client) {
                    (IpAddr::V4(network), IpAddr::V4(client)) => {
                        *prefix == 0
                            || (u32::from(*network) ^ u32::from(client)) >> (32 - prefix) == 0
                    }
                    (IpAddr::V6(network), IpAddr::V6(client)) => {
                        *prefix == 0
                            || (u128::from(*network) ^ u128::from(client)) >> (128 - prefix) == 0
                    }
                    _ => false,
                })
    }
}

impl Settings {
    pub fn from_env() -> Result<Self, SettingsError> {
        let values: BTreeMap<String, String> = env::vars().collect();
        Self::from_map(&values)
    }

    pub fn from_map(values: &BTreeMap<String, String>) -> Result<Self, SettingsError> {
        let mut problems = Vec::new();
        let mode = value(values, "STUDIO_MODE", "local").to_lowercase();
        if !["local", "server"].contains(&mode.as_str()) {
            problems.push(format!("STUDIO_MODE={mode:?}: бывает local или server"));
        }
        let public = flag(values, "STUDIO_PUBLIC", "off");
        if public && mode != "server" {
            problems.push("STUDIO_PUBLIC=on требует STUDIO_MODE=server и HTTPS".to_owned());
        }
        let account_storage_mb = number(values, "ACCOUNT_STORAGE_MB", 500.0, false, &mut problems);
        let account_database_mb = number(values, "ACCOUNT_DATABASE_MB", 50.0, false, &mut problems);
        if !(1.0..=1_048_576.0).contains(&account_database_mb) {
            problems.push("ACCOUNT_DATABASE_MB: нужно от 1 до 1048576 МБ".to_owned());
        }
        let data_dir = PathBuf::from(value(values, "DATA_DIR", "/data"));
        let bot_token = secret(values, "TG_BOT_TOKEN", &mut problems);
        let api_key = secret(values, "STUDIO_API_KEY", &mut problems);
        let session_days = number(values, "SESSION_DAYS", 7.0, false, &mut problems);
        let log_level = value(values, "LOG_LEVEL", "INFO").to_uppercase();
        if !["DEBUG", "INFO", "WARNING", "ERROR"].contains(&log_level.as_str()) {
            problems.push(format!(
                "LOG_LEVEL={log_level:?}: бывает DEBUG, INFO, WARNING, ERROR"
            ));
        }
        let log_files = number(values, "LOG_FILES", 3.0, true, &mut problems) as u64;
        let forwarded_allow_ips = value(values, "FORWARDED_ALLOW_IPS", "127.0.0.1")
            .parse()
            .unwrap_or_else(|error| {
                problems.push(format!("FORWARDED_ALLOW_IPS: {error}"));
                TrustedProxies::default()
            });
        let bind = match value(values, "STUDIO_BIND", "127.0.0.1:7424").parse() {
            Ok(address) => address,
            Err(_) => {
                problems.push("STUDIO_BIND: нужен адрес IP:порт".to_owned());
                SocketAddr::from(([127, 0, 0, 1], 7424))
            }
        };
        let default_chat = value(values, "TG_DEFAULT_CHAT", "").to_owned();
        let preview_chat = value(values, "TG_PREVIEW_CHAT", "").to_owned();
        let updates_on = flag(values, "TG_UPDATES", "on");
        let public_url = value(values, "STUDIO_URL", "http://localhost:7424")
            .trim_end_matches('/')
            .to_owned();
        let spec_url = value(
            values,
            "SPEC_URL",
            "https://raw.githubusercontent.com/PaulSonOfLars/telegram-bot-api-spec/main/api.json",
        )
        .to_owned();
        let spec_refresh_hours = number(values, "SPEC_REFRESH_HOURS", 12.0, false, &mut problems);
        let spec_pinned = flag(
            values,
            "SPEC_PIN",
            if mode == "server" { "on" } else { "off" },
        );
        let own_server = !value(values, "TG_API_BASE", "").is_empty();
        let max_upload_mb = number(
            values,
            "MAX_UPLOAD_MB",
            if own_server { 2000.0 } else { 50.0 },
            false,
            &mut problems,
        );
        let media_fetch_private = flag(values, "MEDIA_FETCH_PRIVATE", "off");
        let api_base = value(values, "TG_API_BASE", "https://api.telegram.org")
            .trim_end_matches('/')
            .to_owned();
        let raw_api = flag(values, "RAW_API", "on");
        let bot_username = value(values, "TG_BOT_USERNAME", "YourBot")
            .trim_start_matches('@')
            .to_owned();
        let owner_telegram_id = number(values, "TG_OWNER_ID", 0.0, true, &mut problems) as i64;
        if !(0..(1_i64 << 52)).contains(&owner_telegram_id) {
            problems.push(
                "TG_OWNER_ID: нужен Telegram ID пользователя (положительное число меньше 2^52)"
                    .to_owned(),
            );
        }
        if public
            && (bot_username.is_empty()
                || !bot_username
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_'))
        {
            problems.push("TG_BOT_USERNAME: нужен username бота без @".to_owned());
        }
        let landing_url = value(values, "STUDIO_LANDING_URL", "")
            .trim_end_matches('/')
            .to_owned();
        let mcp_publish = flag(values, "MCP_PUBLISH", "on");
        let background = flag(values, "STUDIO_BACKGROUND", "on");
        let updates_mode = value(values, "TG_UPDATES_MODE", "polling").to_lowercase();
        let webhook_secret = secret(values, "TG_WEBHOOK_SECRET", &mut problems);
        if !["polling", "webhook"].contains(&updates_mode.as_str()) {
            problems.push("TG_UPDATES_MODE: бывает polling или webhook".to_owned());
        }
        let import_api_id = number(values, "TG_IMPORT_API_ID", 0.0, true, &mut problems) as i64;
        let import_api_hash = secret(values, "TG_IMPORT_API_HASH", &mut problems);
        let import_proxy = secret(values, "TG_IMPORT_PROXY_URL", &mut problems);
        if (import_api_id > 0) == import_api_hash.is_empty() {
            problems.push("TG_IMPORT_API_ID и TG_IMPORT_API_HASH задаются вместе".to_owned());
        }
        if !import_proxy.is_empty()
            && url::Url::parse(&import_proxy).ok().is_none_or(|url| {
                !["http", "socks5"].contains(&url.scheme())
                    || url.host_str().is_none()
                    || !["", "/"].contains(&url.path())
                    || url.query().is_some()
                    || url.fragment().is_some()
            })
        {
            problems.push(
                "TG_IMPORT_PROXY_URL: нужен http:// или socks5:// адрес прокси без пути".to_owned(),
            );
        }
        let schedule_late_min = number(values, "SCHEDULE_LATE_MIN", 60.0, false, &mut problems);
        let backup_keep = number(values, "BACKUP_KEEP", 7.0, true, &mut problems) as u64;
        let log_max_mb = number(values, "LOG_MAX_MB", 5.0, false, &mut problems);
        let media_unused_days = number(values, "MEDIA_UNUSED_DAYS", 0.0, false, &mut problems);
        let versions_keep = number(values, "VERSIONS_KEEP", 0.0, true, &mut problems) as u64;
        let deadman_url = value(values, "DEADMAN_URL", "")
            .trim_end_matches('/')
            .to_owned();
        if !deadman_url.is_empty()
            && !deadman_url.starts_with("http://")
            && !deadman_url.starts_with("https://")
        {
            problems.push("DEADMAN_URL: нужен адрес http(s)://".to_owned());
        }
        let shutdown_grace = number(values, "SHUTDOWN_GRACE", 30.0, false, &mut problems);
        let mut allowed_hosts = HashSet::from([
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "[::1]".to_owned(),
            "::1".to_owned(),
        ]);
        allowed_hosts.extend(
            value(values, "ALLOWED_HOSTS", "")
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_lowercase),
        );
        let public_address = url::Url::parse(&public_url).ok();
        if mode == "server" {
            if public_address.as_ref().is_none_or(|url| {
                url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
            }) {
                problems.push("STUDIO_MODE=server: STUDIO_URL должен быть https://… (адрес, который отдаёт Caddy)".to_owned());
            } else if let Some(url) = &public_address {
                allowed_hosts.insert(
                    url[url::Position::BeforeHost..url::Position::AfterPort].to_lowercase(),
                );
            }
            if bot_token.is_empty() {
                problems.push(
                    "STUDIO_MODE=server: не задан TG_BOT_TOKEN (или TG_BOT_TOKEN_FILE)".to_owned(),
                );
            }
            if !landing_url.is_empty() {
                match url::Url::parse(&landing_url) {
                    Ok(url)
                        if url.scheme() == "https"
                            && url.host_str().is_some()
                            && url.username().is_empty()
                            && url.password().is_none()
                            && url.path() == "/"
                            && url.query().is_none()
                            && url.fragment().is_none() =>
                    {
                        let host =
                            url[url::Position::BeforeHost..url::Position::AfterPort].to_lowercase();
                        if public_address.as_ref().is_some_and(|app| {
                            app[url::Position::BeforeHost..url::Position::AfterPort] == host
                        }) {
                            problems.push(
                                "STUDIO_LANDING_URL и STUDIO_URL должны быть разными доменами"
                                    .to_owned(),
                            );
                        } else {
                            allowed_hosts.insert(host);
                        }
                    }
                    _ => problems.push(
                        "STUDIO_LANDING_URL: нужен отдельный https://домен без пути".to_owned(),
                    ),
                }
            }
        }
        if updates_on && updates_mode == "webhook" {
            if webhook_secret.is_empty()
                || webhook_secret.len() > 256
                || !webhook_secret.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == '_' || character == '-'
                })
            {
                problems.push(
                    "TG_WEBHOOK_SECRET: нужен секрет из 1–256 букв ASCII, цифр, _ или -".to_owned(),
                );
            }
            if public_address.as_ref().is_none_or(|url| {
                url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || url.path() != "/"
                    || ![443, 80, 88, 8443].contains(&url.port().unwrap_or(443))
            }) {
                problems.push("STUDIO_URL: для вебхука нужен публичный HTTPS-адрес без пути, порт 443, 80, 88 или 8443".to_owned());
            } else if let Some(url) = &public_address {
                allowed_hosts.insert(
                    url[url::Position::BeforeHost..url::Position::AfterPort].to_lowercase(),
                );
            }
        }
        if !problems.is_empty() {
            return Err(SettingsError { problems });
        }
        Ok(Self {
            mode,
            public,
            account_storage_mb,
            account_database_mb,
            data_dir,
            bot_token,
            api_key,
            session_days,
            log_level,
            log_files,
            allowed_hosts,
            forwarded_allow_ips,
            bind,
            default_chat,
            preview_chat,
            updates_on,
            public_url,
            spec_url,
            spec_refresh_hours,
            spec_pinned,
            max_upload_mb,
            media_fetch_private,
            api_base,
            raw_api,
            bot_username,
            owner_telegram_id,
            landing_url,
            mcp_publish,
            background,
            updates_mode,
            webhook_secret,
            import_api_id,
            import_api_hash,
            import_proxy,
            schedule_late_min,
            backup_keep,
            log_max_mb,
            media_unused_days,
            versions_keep,
            deadman_url,
            shutdown_grace,
        })
    }

    pub fn root_database_path(&self) -> PathBuf {
        self.data_dir.join("studio.db")
    }

    pub fn account_dir(&self, account_id: i64) -> PathBuf {
        self.data_dir.join("accounts").join(account_id.to_string())
    }

    pub fn media_dir(&self, account_id: Option<i64>) -> PathBuf {
        match account_id {
            Some(account_id) => self.account_dir(account_id).join("media"),
            None => self.data_dir.join("media"),
        }
    }

    pub fn database_path(&self, account_id: Option<i64>) -> PathBuf {
        match account_id {
            Some(account_id) => self.account_dir(account_id).join("studio.db"),
            None => self.root_database_path(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn is_landing_host(&self, host: &str) -> bool {
        self.public
            && url::Url::parse(&self.landing_url).ok().is_some_and(|url| {
                url[url::Position::BeforeHost..url::Position::AfterPort].eq_ignore_ascii_case(host)
            })
    }
}

fn value<'a>(values: &'a BTreeMap<String, String>, name: &str, default: &'a str) -> &'a str {
    values.get(name).map_or(default, |value| value.trim())
}

fn flag(values: &BTreeMap<String, String>, name: &str, default: &str) -> bool {
    !["off", "0", "false", "no"].contains(&value(values, name, default).to_lowercase().as_str())
}

fn number(
    values: &BTreeMap<String, String>,
    name: &str,
    default: f64,
    whole: bool,
    problems: &mut Vec<String>,
) -> f64 {
    let raw = value(values, name, "");
    if raw.is_empty() {
        return default;
    }
    let parsed = if whole {
        raw.parse::<u64>().map(|value| value as f64).map_err(|_| ())
    } else {
        raw.parse::<f64>().map_err(|_| ())
    };
    match parsed {
        Ok(number) if number >= 0.0 && number.is_finite() => number,
        Ok(_) => {
            problems.push(format!("{name}={raw}: не может быть меньше 0"));
            default
        }
        Err(()) => {
            problems.push(format!(
                "{name}={raw:?}: нужно {}число",
                if whole { "целое " } else { "" }
            ));
            default
        }
    }
}

fn secret(values: &BTreeMap<String, String>, name: &str, problems: &mut Vec<String>) -> String {
    let file_name = format!("{name}_FILE");
    let path = value(values, &file_name, "");
    if path.is_empty() {
        return value(values, name, "").to_owned();
    }
    if !value(values, name, "").is_empty() {
        problems.push(format!("заданы и {name}, и {file_name}: оставьте одно"));
        return String::new();
    }
    match fs::read_to_string(path) {
        Ok(secret) => secret.trim().to_owned(),
        Err(error) => {
            problems.push(format!(
                "{file_name}: не читается файл {path} ({})",
                error.kind()
            ));
            String::new()
        }
    }
}
