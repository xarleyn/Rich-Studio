use serde_json::{Value, json};

use crate::AppState;
use crate::errors::ServiceError;
use crate::services::idempotency::{Idempotency, fingerprint};
use crate::services::kinds;

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "widget_validate",
            "Проверить HTML-виджет до отправки: размеры (исходник и после zlib) и предупреждения о вырезаемых возможностях. Ничего не отправляет.",
            true,
            false,
            false,
            json!({
                "html":{"type":"string","description":"Исходник виджета: HTML со встроенными CSS и JS, до 32 768 байт"},
                "chat_data":{"type":"string","description":"Необязательно: данные виджета для чата"},
                "public_data":{"type":"string","description":"Необязательно: общие данные виджета"}
            }),
        ),
        definition(
            "send_widget",
            "Отправить HTML-виджет отдельным сообщением только после явного подтверждения пользователя. HTML после отправки неизменяем; каналы не входят в прямую отправку.",
            false,
            true,
            true,
            json!({
                "widget":{"type":"object","description":"Автономный виджет: {html, caption?, chat_data?, public_data?}; allow_queries=true недоступно"},
                "chat_id":{"type":["string","integer"]},
                "chat_ids":{"type":"array"},
                "at":{"type":["string","integer"]},
                "pin":{"type":"boolean"},
                "idempotency_key":{"type":"string"}
            }),
        ),
        definition(
            "edit_widget_data",
            "Изменить данные виджета (editMessageWidgetData): присутствующий ключ задаёт chat_data/public_data, пустая строка стирает, HTML не меняется.",
            false,
            true,
            true,
            json!({
                "post_id":{"type":"string","description":"Пост-виджет из list_posts"},
                "post_version":{"type":"integer","description":"version из get_post: устаревшая версия отклоняется"},
                "chat_data":{"type":"string","description":"До 1 024 байт"},
                "public_data":{"type":"string","description":"До 1 024 байт"}
            }),
        ),
    ]
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "widget_validate" => Ok(validate(&arguments)),
        "send_widget" => send(state, arguments)
            .await
            .map_err(|error| error.to_string()),
        "edit_widget_data" => edit(state, arguments)
            .await
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown widgets tool {name}")),
    }
}

fn validate(arguments: &Value) -> Value {
    let mut widget = json!({"html":arguments["html"]});
    for key in ["chat_data", "public_data"] {
        if let Some(value) = arguments.get(key) {
            widget[key] = value.clone();
        }
    }
    crate::services::widgets::report(&widget)
}

async fn send(state: AppState, arguments: Value) -> Result<Value, ServiceError> {
    let body = json!({
        "widget":arguments["widget"],
        "chat_id":arguments["chat_id"],
        "chat_ids":arguments["chat_ids"],
        "at":arguments["at"],
        "pin":arguments["pin"].as_bool().unwrap_or(false),
    });
    let key = arguments["idempotency_key"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auto:{}", fingerprint("widget", &body)));
    let action_body = body.clone();
    let service = crate::services::posts::from_state(&state).await;
    let actor = state.principal.actor("mcp");
    let action_actor = actor.clone();
    Idempotency::new(state.account.clone())
        .run(Some(&key), "widget", &body, &actor, || async move {
            service
                .send_kind(
                    kinds::get(Some(kinds::WIDGET))?,
                    &action_body["widget"],
                    &action_body["chat_id"],
                    &action_body["chat_ids"],
                    &action_body["at"],
                    action_body["pin"].as_bool().unwrap_or(false),
                    &action_actor,
                )
                .await
        })
        .await
}

async fn edit(state: AppState, arguments: Value) -> Result<Value, ServiceError> {
    let id = arguments["post_id"]
        .as_str()
        .ok_or_else(|| ServiceError::BadRequest("post_id: обязательная строка".to_owned()))?
        .to_owned();
    let mut body = json!({});
    for key in ["chat_data", "public_data"] {
        if let Some(value) = arguments.get(key) {
            body[key] = value.clone();
        }
    }
    let actor = state.principal.actor("mcp");
    let service = crate::services::editing::EditingService::new(
        state.account.clone(),
        state.transport(),
        &state.settings,
        state.spec.read().await.clone(),
    );
    service
        .widget_data(&id, &body, &actor, arguments["post_version"].as_i64())
        .await
}
