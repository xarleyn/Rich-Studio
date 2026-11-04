use super::definition;
use crate::{
    AppState,
    logs::{self, ReadQuery},
    services::health,
};
use serde_json::{Value, json};

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "studio_status",
            "Что сейчас работает: токен, каналы, приём событий и настройки публикации.",
            true,
            false,
            false,
            json!({}),
        ),
        definition(
            "studio_health",
            "Проверки состояния студии, счётчики вызовов и последние предупреждения. Ничего не меняет.",
            true,
            false,
            false,
            json!({}),
        ),
        definition(
            "studio_logs",
            "Последние строки журнала JSON, новые в конце; без токенов и содержимого постов.",
            true,
            false,
            false,
            json!({"level":{"type":"string","default":"WARNING"},"limit":{"type":"integer","minimum":1,"maximum":500,"default":50},"since":{"type":["number","null"],"minimum":0,"maximum":4102444800_i64},"logger":{"type":"string","default":""},"text":{"type":"string","default":""}}),
        ),
    ]
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "studio_status" => {
            let context = &state.account;
            Ok(
                json!({"token_set":!state.settings.bot_token.is_empty(),"default_chat":if context.root&&!state.settings.default_chat.is_empty(){json!(state.settings.default_chat)}else{Value::Null},"preview_chat_set":if context.root{!state.settings.preview_chat.is_empty()}else{true},
                "updates":crate::services::updates::public_status(&state).await,"schedule_late_min":state.settings.schedule_late_min,"mcp_publish":state.settings.mcp_publish,"note":"отложенные посты отправляет сама студия: в это время она должна работать"}),
            )
        }
        "studio_health" => {
            let spec = state.spec.read().await.clone();
            let loops = state.loop_status.read().await.clone();
            tokio::task::spawn_blocking(move || {
                health::report(
                    &state.settings,
                    &state.account,
                    &spec,
                    &state.logs,
                    &state.metrics,
                    &loops,
                )
            })
            .await
            .map_err(|_| "health worker failed".to_owned())
        }
        "studio_logs" => {
            let limit = match arguments.get("limit") {
                None => 50,
                Some(value) => value.as_u64().ok_or("limit must be an integer")? as usize,
            };
            let since = match arguments.get("since") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_f64().ok_or("since must be a number")?),
            };
            logs::read_checked(
                state.logs.clone(),
                ReadQuery {
                    level: arguments["level"].as_str().unwrap_or("WARNING").to_owned(),
                    limit,
                    since,
                    logger: arguments["logger"].as_str().unwrap_or_default().to_owned(),
                    q: arguments["text"].as_str().unwrap_or_default().to_owned(),
                },
                500,
            )
            .await
            .map(Value::Array)
        }
        _ => Err(format!("unknown studio tool {name}")),
    }
}
