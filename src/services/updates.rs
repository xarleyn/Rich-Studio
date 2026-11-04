//! Durable Telegram event ingestion, scoped to the databases owning each object.
mod callbacks;
mod dispatch;
mod ingest;
mod messages;
mod preview;
mod routing;
mod runtime;

pub use dispatch::{handle_update, poll_results};
pub use ingest::{poll_once, receive_webhook, register_webhook};
pub use preview::remember_preview;
pub use runtime::run;

use crate::{AppState, settings::Settings};
use serde::Serialize;
use serde_json::{Value, json};

pub const OFFSET_KEY: &str = "updates_offset";
pub const PREVIEW_KEY: &str = "preview_callbacks";
pub const COMMENT_WAIT: i64 = 86400;

// Каждый ключ обязан иметь ветку в dispatch.rs; этот же список уходит
// в getUpdates и setWebhook, поэтому он объявлен ровно один раз.
pub const ALLOWED_UPDATES: [&str; 8] = [
    "message",
    "edited_message",
    "callback_query",
    "message_reaction_count",
    "poll",
    "chat_member",
    "chat_join_request",
    "my_chat_member",
];
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub state: String,
    pub error: Option<String>,
    pub last_at: Option<i64>,
    pub handled: u64,
}
impl Status {
    pub fn new(settings: &Settings) -> Self {
        Self {
            enabled: settings.updates_on,
            state: if settings.updates_on {
                "starting"
            } else {
                "off"
            }
            .to_owned(),
            error: None,
            last_at: None,
            handled: 0,
        }
    }
}
pub async fn public_status(state: &AppState) -> Value {
    let mut value =
        serde_json::to_value(state.updates_status.read().await.clone()).unwrap_or(Value::Null);
    value["mode"] = json!(state.settings.updates_mode);
    if state.account.account_id.is_some() {
        value["error"] = Value::Null;
        value["last_at"] = Value::Null;
        value["handled"] = json!(0)
    }
    value
}
