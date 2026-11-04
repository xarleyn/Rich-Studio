pub mod chat;
pub mod client;
pub mod uploads;

use std::fmt;

use serde_json::Value;

#[derive(Debug)]
pub enum TelegramError {
    Refused {
        description: String,
        code: Option<i64>,
        raw: Option<Value>,
    },
    RateLimited {
        description: String,
        retry_after: Option<u64>,
        raw: Option<Value>,
    },
    Unreachable(String),
    NoAnswer(String),
    Policy(String),
    MissingUpload(String),
}

impl fmt::Display for TelegramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { description, .. } | Self::RateLimited { description, .. } => {
                formatter.write_str(description)
            }
            Self::Unreachable(message)
            | Self::NoAnswer(message)
            | Self::Policy(message)
            | Self::MissingUpload(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TelegramError {}
pub mod import_client;
