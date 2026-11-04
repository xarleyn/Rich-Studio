use serde_json::{Value, json};

use crate::AppState;
use crate::errors::ServiceError;
use crate::services::editing::EditingService;
use crate::services::{blocking, listing};
use crate::store::drafts;

use super::definition;

pub fn definitions() -> Vec<Value> {
    let id = json!({"post_id":{"type":"string"}});
    vec![
        definition(
            "get_post",
            "Пост целиком: содержимое, статус, канал, версия и статистика.",
            true,
            false,
            false,
            id.clone(),
        ),
        definition(
            "post_history",
            "Прежние версии поста, новые сверху, и авторы правок.",
            true,
            false,
            false,
            id.clone(),
        ),
        definition(
            "cancel_scheduled_post",
            "Отменить отложенный или пропущенный пост после подтверждения пользователя.",
            false,
            true,
            true,
            id,
        ),
        definition(
            "list_posts",
            "Посты студии: первая строка, ссылка, статус и ошибки, нажатия и реакции.",
            true,
            false,
            false,
            json!({"limit":{"type":"integer","minimum":1,"maximum":100,"default":20},"status":{"type":"string"},"chat_id":{"type":"string"}}),
        ),
        definition(
            "edit_post",
            "Заменить пост после подтверждения пользователя. Передайте post_version из get_post; post или draft_id с version. Сохраняет историю.",
            false,
            true,
            true,
            json!({"post_id":{"type":"string"},"post":{"type":"object"},"draft_id":{"type":"string"},"version":{"type":"integer"},"post_version":{"type":"integer"}}),
        ),
        definition(
            "reschedule_post",
            "Перенести отложенный или пропущенный пост после подтверждения пользователя.",
            false,
            true,
            true,
            json!({"post_id":{"type":"string"},"at":{"type":"string"}}),
        ),
    ]
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    dispatch(state, name, arguments)
        .await
        .map_err(|error| error.to_string())
}

async fn dispatch(state: AppState, name: &str, arguments: Value) -> Result<Value, ServiceError> {
    let service = EditingService::new(
        state.account.clone(),
        state.transport(),
        &state.settings,
        state.spec.read().await.clone(),
    );
    if name == "list_posts" {
        let limit = arguments["limit"].as_i64().unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(ServiceError::BadRequest("limit: от 1 до 100".to_owned()));
        }
        return blocking::run(&state.account, move |context| {
            Ok(json!(listing::list_posts(
                context,
                limit,
                arguments["status"].as_str(),
                arguments["chat_id"].as_str()
            )?))
        })
        .await;
    }
    let id = arguments["post_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("post_id: обязательное поле".to_owned()))?
        .to_owned();
    match name {
        "get_post" => {
            let mut record = service.get(&id).await?;
            let chat = record["chat_id"].clone();
            record["chat_title"] = blocking::run(&state.account, move |context| {
                listing::channel_title(context, &chat)
            })
            .await?;
            record["editor_url"] = json!(format!("{}/?post={id}", state.settings.public_url));
            Ok(record)
        }
        "post_history" => service.versions(&id).await,
        "reschedule_post" => service.reschedule(&id, &arguments["at"]).await,
        "cancel_scheduled_post" => {
            let record = service.get(&id).await?;
            if !["scheduled", "missed"].contains(&record["status"].as_str().unwrap_or_default()) {
                return Err(ServiceError::BadRequest(format!(
                    "пост в статусе {}: отменить можно только отложенный или пропущенный",
                    record["status"]
                )));
            }
            service.delete(&id).await
        }
        "edit_post" => {
            let post = source(&state, &id, &arguments).await?;
            service
                .edit(
                    &id,
                    &post,
                    &state.principal.actor("mcp"),
                    arguments["post_version"].as_i64(),
                )
                .await
        }
        _ => Err(ServiceError::BadRequest(format!("unknown tool {name}"))),
    }
}

async fn source(state: &AppState, id: &str, args: &Value) -> Result<Value, ServiceError> {
    let post = &args["post"];
    let draft_id = args["draft_id"].as_str();
    if post.is_null() == draft_id.is_none() {
        return Err(ServiceError::BadRequest(
            "передайте post или draft_id, одно из двух".to_owned(),
        ));
    }
    if !post.is_null() {
        return crate::services::contracts::parse_post(post).map_err(ServiceError::BadRequest);
    }
    let draft_id = draft_id.unwrap_or_default().to_owned();
    let version = args["version"].as_i64().ok_or_else(|| {
        ServiceError::BadRequest(
            "с draft_id передайте version из save_draft или get_draft".to_owned(),
        )
    })?;
    let id = id.to_owned();
    blocking::run(&state.account, move |context| {
        let draft = drafts::get(context, &draft_id)?
            .ok_or_else(|| ServiceError::NotFound(format!("черновика {draft_id} нет")))?;
        if draft["version"].as_i64() != Some(version) {
            return Err(ServiceError::Conflict(
                "черновик изменили: прочитайте заново через get_draft".to_owned(),
            ));
        }
        if !draft["post_id"].is_null() && draft["post_id"] != id {
            let expected = draft["post_id"].as_str().unwrap_or_default();
            return Err(ServiceError::BadRequest(format!(
                "черновик {draft_id} — правка поста {expected}: замените его через edit_post с post_id {expected}, draft_id и version, а не публикуйте новый пост"
            )));
        }
        crate::services::contracts::parse_post(&draft["payload"]).map_err(ServiceError::BadRequest)
    })
    .await
}
