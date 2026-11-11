use std::fmt;

use rusqlite::ErrorCode;

use crate::account_context::AccountContext;
use crate::telegram::TelegramError;
use serde_json::Value;

#[derive(Debug)]
pub enum ServiceError {
    BadRequest(String),
    Unprocessable(String),
    Invalid(Value),
    Conflict(String),
    NotFound(String),
    Upstream(Value),
    MayHaveGone(Value),
    Internal(String),
    Store(StoreError),
    Telegram(TelegramError),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(message)
            | Self::Unprocessable(message)
            | Self::Conflict(message)
            | Self::NotFound(message)
            | Self::Internal(message) => formatter.write_str(message),
            Self::Invalid(detail) | Self::Upstream(detail) | Self::MayHaveGone(detail) => {
                write!(formatter, "{detail}")
            }
            Self::Store(error) => error.fmt(formatter),
            Self::Telegram(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<StoreError> for ServiceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<TelegramError> for ServiceError {
    fn from(error: TelegramError) -> Self {
        Self::Telegram(error)
    }
}

#[derive(Debug)]
pub enum StoreError {
    StorageFull,
    Quota(String),
    Migration(String),
    Sqlite(rusqlite::Error),
}

impl StoreError {
    pub fn from_sqlite(error: rusqlite::Error, context: &AccountContext) -> Self {
        if !context.root
            && matches!(
                &error,
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == ErrorCode::DiskFull
            )
        {
            Self::StorageFull
        } else {
            Self::Sqlite(error)
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageFull => write!(
                formatter,
                "Лимит базы учётки исчерпан. Удалите ненужные черновики. \
                 Если выполнялась публикация, проверьте канал перед повтором: сообщение могло уйти."
            ),
            Self::Migration(message) | Self::Quota(message) => formatter.write_str(message),
            Self::Sqlite(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::StorageFull | Self::Migration(_) | Self::Quota(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub type MigrationError = StoreError;

#[derive(Debug)]
pub struct ImportError {
    pub status: u16,
    pub detail: Value,
}
impl ImportError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            detail: Value::String(message.into()),
        }
    }
}
impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Value::String(text) => formatter.write_str(text),
            other => write!(formatter, "{other}"),
        }
    }
}
impl std::error::Error for ImportError {}
impl From<StoreError> for ImportError {
    fn from(error: StoreError) -> Self {
        Self::new(
            if matches!(error, StoreError::StorageFull | StoreError::Quota(_)) {
                413
            } else {
                500
            },
            error.to_string(),
        )
    }
}
