use serde_json::{Value, json};

use crate::AppState;
use crate::errors::ServiceError;
use crate::services::idempotency::{Idempotency, fingerprint};
use crate::services::kinds;
use crate::store::drafts;

use super::definition;

pub fn definitions() -> Vec<Value> {
    let post = json!({"post":{"type":"object"},"chat_id":{"type":["string","integer"]},"chat_ids":{"type":"array"},"draft_id":{"type":"string"},"version":{"type":"integer"},"idempotency_key":{"type":"string"},"dry_run":{"type":"boolean"}});
    let mut schedule = post.clone();
    schedule["at"] = json!({"type":["string","integer"]});
    vec![
        definition(
            "publish_post",
            "Опубликовать пост в канал только после явного подтверждения пользователя; dry_run ничего не отправляет.",
            false,
            true,
            true,
            post,
        ),
        definition(
            "schedule_post",
            "Отложить публикацию после явного подтверждения пользователя.",
            false,
            true,
            true,
            schedule,
        ),
        definition(
            "send_poll",
            "Опрос или викторина отдельным сообщением после явного подтверждения пользователя.",
            false,
            true,
            true,
            json!({"poll":{"type":"object"},"chat_id":{"type":["string","integer"]},"chat_ids":{"type":"array"},"at":{"type":["string","integer"]},"pin":{"type":"boolean"},"idempotency_key":{"type":"string"}}),
        ),
    ]
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "publish_post" => publish(state, arguments, false)
            .await
            .map_err(|error| error.to_string()),
        "schedule_post" => publish(state, arguments, true)
            .await
            .map_err(|error| error.to_string()),
        "send_poll" => poll(state, arguments)
            .await
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown publishing tool {name}")),
    }
}

async fn source(state: &AppState, arguments: &Value, publish: bool) -> Result<Value, ServiceError> {
    let post = &arguments["post"];
    let draft_id = arguments["draft_id"].as_str();
    if post.is_null() == draft_id.is_none() {
        return Err(ServiceError::BadRequest(
            "передайте post или draft_id, одно из двух".to_owned(),
        ));
    }
    if !post.is_null() {
        return crate::services::contracts::parse_post(post).map_err(ServiceError::BadRequest);
    }
    let draft_id = draft_id.unwrap_or_default().to_owned();
    let read_id = draft_id.clone();
    let draft = crate::services::blocking::run(&state.account, move |context| {
        drafts::get(context, &read_id)?
            .ok_or_else(|| ServiceError::NotFound(format!("черновика {read_id} нет")))
    })
    .await?;
    let version = arguments["version"].as_i64();
    if publish && version.is_none() {
        return Err(ServiceError::BadRequest("с draft_id передайте version из save_draft или get_draft: публикуется то, что видел пользователь".to_owned()));
    }
    if version.is_some_and(|version| draft["version"].as_i64() != Some(version)) {
        return Err(ServiceError::Conflict(format!(
            "черновик изменили (version {}, а не {}): покажите его пользователю заново через get_draft и preview_post",
            draft["version"],
            version.unwrap_or_default()
        )));
    }
    if publish && !draft["post_id"].is_null() {
        return Err(ServiceError::BadRequest(format!(
            "черновик {draft_id} — правка поста {}: замените его через edit_post, а не публикуйте новый пост",
            draft["post_id"]
        )));
    }
    crate::services::contracts::parse_post(&draft["payload"]).map_err(ServiceError::BadRequest)
}

async fn publish(state: AppState, arguments: Value, schedule: bool) -> Result<Value, ServiceError> {
    let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);
    let post = source(&state, &arguments, !dry_run).await?;
    let mut body = json!({"post":post,"chat_id":arguments["chat_id"],"chat_ids":arguments["chat_ids"],"draft_id":arguments["draft_id"],"dry_run":false});
    let scope = if schedule { "schedule" } else { "send" };
    if schedule {
        body["at"] = arguments["at"].clone();
    }
    if dry_run {
        let service = crate::services::posts::from_state(&state).await;
        return if schedule {
            service
                .run(move |service| {
                    service.schedule(
                        &body["post"],
                        &body["at"],
                        &body["chat_id"],
                        &body["chat_ids"],
                        body["draft_id"].as_str(),
                        "mcp",
                        true,
                        false,
                    )
                })
                .await
        } else {
            service
                .send(
                    &body["post"],
                    &body["chat_id"],
                    &body["chat_ids"],
                    body["draft_id"].as_str(),
                    "mcp",
                    true,
                    false,
                )
                .await
        };
    }
    let key = arguments["idempotency_key"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auto:{}", fingerprint(scope, &body)));
    let action_body = body.clone();
    let service = crate::services::posts::from_state(&state).await;
    let actor = state.principal.actor("mcp");
    let action_actor = actor.clone();
    Idempotency::new(state.account.clone())
        .run(Some(&key), scope, &body, &actor, || async move {
            if schedule {
                service
                    .run(move |service| {
                        service.schedule(
                            &action_body["post"],
                            &action_body["at"],
                            &action_body["chat_id"],
                            &action_body["chat_ids"],
                            action_body["draft_id"].as_str(),
                            &action_actor,
                            false,
                            false,
                        )
                    })
                    .await
            } else {
                service
                    .send(
                        &action_body["post"],
                        &action_body["chat_id"],
                        &action_body["chat_ids"],
                        action_body["draft_id"].as_str(),
                        &action_actor,
                        false,
                        false,
                    )
                    .await
            }
        })
        .await
}

async fn poll(state: AppState, arguments: Value) -> Result<Value, ServiceError> {
    let body = json!({"poll":arguments["poll"],"chat_id":arguments["chat_id"],"chat_ids":arguments["chat_ids"],"at":arguments["at"],"pin":arguments["pin"].as_bool().unwrap_or(false)});
    let key = arguments["idempotency_key"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auto:{}", fingerprint("poll", &body)));
    let action_body = body.clone();
    let service = crate::services::posts::from_state(&state).await;
    let actor = state.principal.actor("mcp");
    let action_actor = actor.clone();
    Idempotency::new(state.account.clone())
        .run(Some(&key), "poll", &body, &actor, || async move {
            service
                .send_kind(
                    kinds::get(Some(kinds::POLL))?,
                    &action_body["poll"],
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
