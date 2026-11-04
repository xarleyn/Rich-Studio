use serde_json::{Value, json};

use crate::AppState;
use crate::services::channels::from_state as service;

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "check_channel",
            "Проверить права бота и запомнить канал с названием.",
            false,
            false,
            true,
            json!({"chat_id":{"type":["string","integer"]}}),
        ),
        definition(
            "list_channels",
            "Каналы студии с названиями, темами форума и правами бота.",
            true,
            false,
            false,
            json!({}),
        ),
    ]
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "check_channel" => {
            let chat = arguments
                .get("chat_id")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| Value::String(state.settings.default_chat.clone()));
            service(&state)
                .check(chat)
                .await
                .map_err(|error| error.to_string())
        }
        "list_channels" => service(&state)
            .run(|service| service.list())
            .await
            .map(|channels| json!(channels))
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown channel tool {name}")),
    }
}
