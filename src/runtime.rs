//! Filesystem resources for both source runs and relocatable installations.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::settings::{Settings, SettingsError};

/// Installed binaries use an adjacent `static` directory. Developers may run from the repository
/// root; explicit configuration always wins, including when it names a missing directory.
pub fn static_dir() -> PathBuf {
    if let Some(configured) = std::env::var_os("STUDIO_STATIC_DIR") {
        return PathBuf::from(configured);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let installed = parent.join("static");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from("app/static")
}

/// Startup check for `DATA_DIR`, matching the Python reference `main._data_dir_writable`.
///
/// Docker or an older image can leave a root-owned directory, or a root-owned file inside an
/// otherwise writable directory, that only fails at the first write. The directory is created
/// when missing, a probe file is written and removed, and every entry already inside must be
/// writable: a file by opening it for writing, a directory by accepting a probe file.
pub fn check_data_dir(settings: &Settings) -> Result<(), SettingsError> {
    check_data_dir_inner(settings).map_err(|error| SettingsError {
        problems: vec![data_dir_diagnostic(settings, &error, process_identity())],
    })
}

/// The message shown when the startup check fails. `identity` is the process uid/gid on Unix;
/// on Windows it is `None` and the advice names the folder security properties instead. The
/// Python reference derived the same branch from the presence of `os.getuid`/`os.getgid`.
pub fn data_dir_diagnostic(
    settings: &Settings,
    error: &io::Error,
    identity: Option<(u32, u32)>,
) -> String {
    let owner = match identity {
        Some((uid, _)) => uid.to_string(),
        None => current_user(),
    };
    let action = match identity {
        Some((uid, gid)) => format!(
            "если папку создал Docker или прежний образ от root: sudo chown -R {uid}:{gid} data"
        ),
        None => "разрешите этому пользователю запись в папку и её файлы в свойствах безопасности Windows"
            .to_owned(),
    };
    format!(
        "нет записи в DATA_DIR={} ({error}) для пользователя {owner}; {action}",
        settings.data_dir.display()
    )
}

fn check_data_dir_inner(settings: &Settings) -> io::Result<()> {
    fs::create_dir_all(&settings.data_dir)?;
    let probe = settings.data_dir.join(".write-test");
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)?;
    let mut directories = vec![settings.data_dir.clone()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let file_type = fs::symlink_metadata(&path)?.file_type();
            if file_type.is_symlink() && path.is_dir() {
                // A directory link may point back into the tree; files behind links are checked.
                continue;
            }
            if file_type.is_dir() {
                // A unique name: an existing file in the data folder must never be overwritten.
                let probe = path.join(format!(".write-test-{}", uuid::Uuid::new_v4()));
                if let Err(error) = fs::write(&probe, b"").and_then(|()| fs::remove_file(&probe)) {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{} не записывается", path.display()),
                    ));
                }
                directories.push(path);
            } else if !writable_file(&path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} не записывается", path.display()),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn writable_file(path: &Path) -> bool {
    // Python's os.access on Windows checks existence and the read-only flag, not ACLs; opening
    // for writing instead would report a database another process holds open.
    fs::metadata(path).is_ok_and(|metadata| !metadata.permissions().readonly())
}

#[cfg(not(windows))]
fn writable_file(path: &Path) -> bool {
    // Opening without O_TRUNC proves write access and, unlike mode bits, accounts for ACLs.
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => fs::OpenOptions::new().write(true).open(path).is_ok(),
        Ok(_) => true,
        Err(_) => false,
    }
}

#[cfg(unix)]
fn process_identity() -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let probe = std::env::temp_dir().join(format!(
        ".rich-studio-identity-{}-{stamp}",
        std::process::id()
    ));
    let metadata = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|file| file.metadata())
        .ok();
    let _ = fs::remove_file(&probe);
    metadata.map(|metadata| (metadata.uid(), metadata.gid()))
}

#[cfg(not(unix))]
fn process_identity() -> Option<(u32, u32)> {
    None
}

fn current_user() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_default()
}

/// Both Ctrl-C and container SIGTERM initiate the same graceful server drain.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
