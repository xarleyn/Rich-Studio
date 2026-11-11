use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use crate::settings::Settings;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Read,
    Owner,
    Admin,
    Publish,
    Drafts,
}

impl Role {
    pub fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Drafts => 1,
            Self::Publish => 2,
            Self::Admin | Self::Owner => 3,
        }
    }

    pub fn level(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Drafts => "draft",
            Self::Publish => "publish",
            Self::Admin | Self::Owner => "admin",
        }
    }

    pub fn parse(level: &str) -> Option<Self> {
        match level {
            "read" => Some(Self::Read),
            "draft" => Some(Self::Drafts),
            "publish" => Some(Self::Publish),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportPolicy {
    Owner,
    Account(i64),
}

#[derive(Clone, Debug)]
pub struct AccountContext {
    pub account_id: Option<i64>,
    pub role: Role,
    pub root: bool,
    pub media_dir: PathBuf,
    pub transport_policy: TransportPolicy,
    database_path: PathBuf,
    pub(crate) account_database_mb: f64,
}

impl AccountContext {
    pub fn root(settings: &Settings) -> Self {
        Self {
            account_id: None,
            role: Role::Owner,
            root: true,
            media_dir: settings.media_dir(None),
            transport_policy: TransportPolicy::Owner,
            database_path: settings.root_database_path(),
            account_database_mb: settings.account_database_mb,
        }
    }

    pub fn account(settings: &Settings, account_id: i64, role: Role) -> Self {
        Self {
            account_id: Some(account_id),
            role,
            root: false,
            media_dir: settings.media_dir(Some(account_id)),
            transport_policy: TransportPolicy::Account(account_id),
            database_path: settings.database_path(Some(account_id)),
            account_database_mb: settings.account_database_mb,
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
}

/// Downloads, Telegram attachments and exports accept regular files in the account's media folder.
pub(crate) fn media_file_path(directory: &Path, name: &str) -> Option<PathBuf> {
    let leaf = Path::new(name);
    if leaf.file_name() != Some(leaf.as_os_str()) {
        return None;
    }
    let path = directory.join(leaf);
    fs::symlink_metadata(&path).ok()?.is_file().then_some(path)
}

/// Recheck at open time: an attachment may be queued before its bytes are read.
pub(crate) fn open_media_file(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(io::ErrorKind::NotFound.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err(io::ErrorKind::NotFound.into());
        }
    }
    Ok(file)
}

pub(crate) fn read_media_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_media_file(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}
