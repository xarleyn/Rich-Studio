//! SQLite online copies and allowlisted archive contents; import-session files never enter exports.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, backup::Backup};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{account_context::AccountContext, settings::Settings};

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct Export {
    directory: PathBuf,
    path: PathBuf,
}
impl Export {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for Export {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn copy_db(context: &AccountContext, destination: &Path) -> Result<(), Error> {
    let source = Connection::open_with_flags(
        context.database_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(Duration::from_secs(10))?;
    let mut target = Connection::open(destination)?;
    Backup::new(&source, &mut target)?.run_to_completion(128, Duration::from_millis(10), None)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn export_zip(settings: &Settings, context: &AccountContext) -> Result<Export, Error> {
    let directory = std::env::temp_dir().join(format!("rich-studio-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory)?;
    let export = Export {
        path: directory.join("export.zip"),
        directory,
    };
    private_directory(&export.directory)?;
    let contexts = if context.root {
        crate::services::accounts::all_contexts(settings)
            .map_err(|error| io::Error::other(error.to_string()))?
    } else {
        vec![context.clone()]
    };
    let mut archive = ZipWriter::new(File::create(&export.path)?);
    for account in contexts {
        if !account.database_path().is_file() {
            continue;
        }
        let prefix = account
            .account_id
            .map_or_else(String::new, |id| format!("accounts/{id}/"));
        let database = export
            .directory
            .join(format!("{}.db", account.account_id.unwrap_or(0)));
        copy_db(&account, &database)?;
        archive.start_file(
            format!("{prefix}studio.db"),
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        io::copy(&mut File::open(&database)?, &mut archive)?;
        if account.media_dir.exists() {
            let mut entries = fs::read_dir(&account.media_dir)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                // Never follow symlinks or recurse into user-controlled directories.
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| io::Error::other("media filename is not UTF-8"))?;
                archive.start_file(
                    format!("{prefix}media/{name}"),
                    SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
                )?;
                io::copy(&mut File::open(entry.path())?, &mut archive)?;
            }
        }
    }
    archive.finish()?.sync_all()?;
    Ok(export)
}

pub fn export_name() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "rich-studio-{}{:02}{:02}-{:02}{:02}.zip",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    )
}

pub fn daily_snapshot(
    settings: &Settings,
    context: &AccountContext,
) -> Result<Option<PathBuf>, Error> {
    if settings.backup_keep == 0 || !context.database_path().exists() {
        return Ok(None);
    }
    let folder = context
        .database_path()
        .parent()
        .ok_or_else(|| io::Error::other("database directory missing"))?
        .join("backups");
    private_directory(&folder)?;
    let now = time::OffsetDateTime::now_utc();
    let today = folder.join(format!(
        "studio-{}{:02}{:02}.db",
        now.year(),
        u8::from(now.month()),
        now.day()
    ));
    let made = if !today.exists() {
        let partial = folder.join(format!("studio-{}.part", uuid::Uuid::new_v4()));
        let copied = copy_db(context, &partial)
            .and_then(|()| fs::rename(&partial, &today).map_err(Into::into));
        if copied.is_err() {
            let _ = fs::remove_file(&partial);
        }
        copied?;
        Some(today)
    } else {
        None
    };
    let mut copies = fs::read_dir(&folder)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.len() == 18
                && name.starts_with("studio-")
                && name.ends_with(".db")
                && name.as_bytes()[7..15].iter().all(u8::is_ascii_digit)
                && entry.file_type().is_ok_and(|kind| kind.is_file()))
            .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    copies.sort();
    copies.reverse();
    for old in copies.into_iter().skip(settings.backup_keep as usize) {
        fs::remove_file(old)?;
    }
    Ok(made)
}
