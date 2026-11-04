use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags, Row};
use serde_json::{Map, Number, Value};

use crate::account_context::AccountContext;
use crate::errors::{MigrationError, StoreError};
use crate::settings::Settings;

pub const MIGRATIONS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/app/store/migrations");
const MIGRATIONS: &[&str] = &[
    include_str!("../../app/store/migrations/0001_indexes.sql"),
    include_str!("../../app/store/migrations/0002_meta.sql"),
    include_str!("../../app/store/migrations/0003_idempotency.sql"),
    include_str!("../../app/store/migrations/0004_draft_version.sql"),
    include_str!("../../app/store/migrations/0005_post_versions.sql"),
    include_str!("../../app/store/migrations/0006_channels.sql"),
    include_str!("../../app/store/migrations/0007_tokens_sessions.sql"),
    include_str!("../../app/store/migrations/0008_invite_links.sql"),
    include_str!("../../app/store/migrations/0009_suggested_posts.sql"),
    include_str!("../../app/store/migrations/0010_post_kinds.sql"),
    include_str!("../../app/store/migrations/0011_accounts.sql"),
    include_str!("../../app/store/migrations/0012_public_limits.sql"),
    include_str!("../../app/store/migrations/0013_telegram_import.sql"),
    include_str!("../../app/store/migrations/0014_account_tokens.sql"),
    include_str!("../../app/store/migrations/0015_post_templates.sql"),
    include_str!("../../app/store/migrations/0016_bot_dialog.sql"),
    include_str!("../../app/store/migrations/0017_owner_sessions.sql"),
];

/// Immutable SQL travels with the executable; no source checkout is needed at runtime.
pub fn embedded_migrations() -> Vec<String> {
    MIGRATIONS.iter().map(|sql| (*sql).to_owned()).collect()
}
pub const SCHEMA: &str = "
create table if not exists drafts(
  id text primary key, title text, payload text not null,
  created_at integer, updated_at integer);
create table if not exists posts(
  id text primary key, draft_id text, chat_id text not null, message_id integer,
  payload text not null, status text not null, scheduled_at integer,
  sent_at integer, error text, created_at integer);
";
pub const EXTRA_COLUMNS: &[(&str, &str)] = &[
    ("kind", "text default 'rich'"),
    ("chat_num", "integer"),
    ("pinned", "integer default 0"),
    ("comment_status", "text"),
    ("comment_message_id", "integer"),
    ("reactions", "text"),
    ("clicks", "text"),
    ("poll_id", "text"),
    ("poll_results", "text"),
    ("actor", "text"),
    ("retry_at", "integer"),
    ("version", "integer not null default 0"),
];
const JSON_FIELDS: &[&str] = &[
    "payload",
    "reactions",
    "clicks",
    "poll_results",
    "chat_ids",
    "topics",
    "rights",
];

pub struct Store;

impl Store {
    pub fn connect(context: &AccountContext) -> Result<Connection, StoreError> {
        let path = context.database_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                StoreError::Migration(format!(
                    "cannot create database directory {parent:?}: {error}"
                ))
            })?;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&connection, context)?;
        Ok(connection)
    }

    pub fn connect_root(settings: &Settings) -> Result<Connection, StoreError> {
        Self::connect(&AccountContext::root(settings))
    }
}

fn configure(connection: &Connection, context: &AccountContext) -> Result<(), StoreError> {
    connection.busy_timeout(Duration::from_secs(10))?;
    connection.execute_batch("pragma foreign_keys = on; pragma synchronous = FULL;")?;
    if !context.root {
        let page_size: i64 = connection.query_row("pragma page_size", [], |row| row.get(0))?;
        let pages = ((context.account_database_mb * 1024.0 * 1024.0) as i64 / page_size).max(1);
        connection.execute_batch(&format!(
            "pragma max_page_count = {pages}; pragma journal_size_limit = 1048576;"
        ))?;
    }
    Ok(())
}

pub fn load_migrations(folder: impl AsRef<Path>) -> Result<Vec<String>, MigrationError> {
    let mut files = Vec::new();
    for entry in fs::read_dir(folder).map_err(|error| StoreError::Migration(error.to_string()))? {
        let path = entry
            .map_err(|error| StoreError::Migration(error.to_string()))?
            .path();
        if path.extension().is_some_and(|extension| extension == "sql") {
            files.push(path);
        }
    }
    files.sort();
    let mut numbers = Vec::with_capacity(files.len());
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StoreError::Migration(format!("invalid migration filename: {path:?}"))
            })?;
        let number = name
            .split_once('_')
            .and_then(|(number, _)| number.parse::<usize>().ok())
            .ok_or_else(|| StoreError::Migration(format!("invalid migration filename: {name}")))?;
        numbers.push(number);
    }
    let expected: Vec<usize> = (1..=files.len()).collect();
    if numbers != expected {
        return Err(StoreError::Migration(format!(
            "migration numbers must be 1..{} without gaps or repeats: {numbers:?}",
            files.len()
        )));
    }
    files
        .iter()
        .map(|path| {
            fs::read_to_string(path).map_err(|error| StoreError::Migration(error.to_string()))
        })
        .collect()
}

pub fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    let migrations = embedded_migrations();
    migrate_steps(connection, &migrations)
}

pub fn migrate_steps(
    connection: &mut Connection,
    migrations: &[String],
) -> Result<(), MigrationError> {
    connection.execute_batch("pragma journal_mode = WAL;")?;
    connection.execute_batch(SCHEMA)?;
    let existing: Vec<String> = connection
        .prepare("pragma table_info(posts)")?
        .query_map([], |row| row.get(1))?
        .collect::<Result<_, _>>()?;
    for (column, declaration) in EXTRA_COLUMNS {
        if !existing.iter().any(|existing| existing == column) {
            connection.execute_batch(&format!(
                "alter table posts add column {column} {declaration}"
            ))?;
        }
    }
    let version: i64 = connection.query_row("pragma user_version", [], |row| row.get(0))?;
    let version = usize::try_from(version).map_err(|_| {
        StoreError::Migration("database schema version cannot be negative".to_owned())
    })?;
    if version > migrations.len() {
        return Err(StoreError::Migration(format!(
            "database schema version {version} is newer than supported {}",
            migrations.len()
        )));
    }
    for (index, step) in migrations.iter().enumerate().skip(version) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(step)?;
        transaction.execute_batch(&format!("pragma user_version = {};", index + 1))?;
        transaction.commit()?;
    }
    Ok(())
}

pub fn decode_row(row: &Row<'_>) -> Result<Map<String, Value>, rusqlite::Error> {
    let mut output = Map::new();
    for (index, name) in row.as_ref().column_names().enumerate() {
        let value = match row.get_ref(index)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(value) => Value::Number(value.into()),
            ValueRef::Real(value) => Number::from_f64(value).map_or(Value::Null, Value::Number),
            ValueRef::Text(value) => {
                let text = std::str::from_utf8(value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        index,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                if JSON_FIELDS.contains(&name) && !text.is_empty() {
                    serde_json::from_str(text).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            index,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?
                } else {
                    Value::String(text.to_owned())
                }
            }
            ValueRef::Blob(value) => Value::Array(
                value
                    .iter()
                    .map(|byte| Value::Number(u64::from(*byte).into()))
                    .collect(),
            ),
        };
        output.insert(name.to_owned(), value);
    }
    Ok(output)
}

pub fn is_storage_full(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::DiskFull
    )
}
