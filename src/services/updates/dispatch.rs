use super::{
    callbacks, messages,
    routing::{find_message, transport, update_chat},
};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{accounts, blocking, bot_dialog, import_forward, invite_links, suggested_posts},
    store::{bot_dialog as db, channels, editing},
};
use serde_json::{Value, json};

pub fn poll_results(poll: &Value) -> Value {
    json!({"total":poll.get("total_voter_count").unwrap_or(&json!(0)),"is_closed":poll.get("is_closed").unwrap_or(&json!(false)),"options":poll["options"].as_array().map(|options|options.iter().map(|option|json!({"text":option["text"],"voter_count":option.get("voter_count").unwrap_or(&json!(0))})).collect::<Vec<_>>()).unwrap_or_default()})
}

pub async fn handle_update(
    state: &AppState,
    update: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    let spec = state.spec.read().await.clone();
    if import_forward::receive(&state.settings, update, &spec)
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?
    {
        let _ = bot_dialog::import_reply(state, update).await;
        return Ok(Some("import"));
    }
    if !callbacks::content_callback(state, update).await?
        && bot_dialog::handle(state, update).await?
    {
        return Ok(Some("bot_command"));
    }
    let root = AccountContext::root(&state.settings);
    if !state.settings.public {
        return handle_scoped(state, &root, update).await;
    }
    let settings = state.settings.clone();
    let contexts = blocking::run(&root, move |_| {
        accounts::all_contexts(&settings).map_err(|e| ServiceError::Internal(e.to_string()))
    })
    .await?;
    if update.get("my_chat_member").is_some() {
        return messages::connect(state, update, &contexts).await;
    }
    let mut handled = None;
    for context in contexts {
        let update_copy = update.clone();
        let target =
            blocking::run(&context, move |context| update_chat(context, &update_copy)).await;
        let target = match target {
            Ok(Some(target)) => target,
            Ok(None) => continue,
            Err(error) => {
                state.logs.event(
                    "ERROR",
                    "rich-studio.updates",
                    &error.to_string(),
                    json!({"event":"update.storage_failed","account":context.account_id}),
                );
                continue;
            }
        };
        if context.account_id.is_some()
            && update["message"]["is_automatic_forward"].as_bool() != Some(true)
            && let Err(error) = transport(state)
                .check_account_member(&context, &target)
                .await
        {
            state.logs.event(
                "WARNING",
                "rich-studio.updates",
                &error.to_string(),
                json!({"event":"update.account_refused","account":context.account_id}),
            );
            continue;
        }
        match handle_scoped(state, &context, update).await {
            Ok(result) => {
                handled = result.or(handled);
                if update["callback_query"].is_object() {
                    return Ok(handled);
                }
            }
            Err(error) => state.logs.event(
                "ERROR",
                "rich-studio.updates",
                &error.to_string(),
                json!({"event":"update.account_failed","account":context.account_id}),
            ),
        }
    }
    if handled.is_some() {
        Ok(handled)
    } else {
        handle_scoped(state, &root, update).await
    }
}
async fn handle_scoped(
    state: &AppState,
    context: &AccountContext,
    update: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    if let Some(callback) = update.get("callback_query") {
        callbacks::answer(state, context, callback).await?;
        return Ok(Some("callback"));
    }
    if let Some(message) = update
        .get("message")
        .or_else(|| update.get("edited_message"))
        && suggested_posts::on_message(context, message).await?
    {
        return Ok(Some("suggested_post"));
    }
    if let Some(message) = update.get("message") {
        if suggested_posts::on_service(context, message).await? {
            return Ok(Some("suggested_post_outcome"));
        }
        if let Some(topic) = [
            message.get("forum_topic_created"),
            message.get("forum_topic_edited"),
        ]
        .into_iter()
        .flatten()
        .find(|topic| topic.as_object().is_some_and(|topic| !topic.is_empty()))
        {
            if let (Some(name), Some(id)) = (
                topic["name"].as_str().filter(|s| !s.is_empty()),
                message["message_thread_id"].as_i64(),
            ) {
                let chat = message["chat"]["id"].clone();
                let name = name.to_owned();
                blocking::run(context, move |context| {
                    if let Some(channel) = channels::get(context, &chat)? {
                        let mut topics = channel["topics"].as_array().cloned().unwrap_or_default();
                        let updated = json!({"id":id,"name":name});
                        if let Some(topic) = topics
                            .iter_mut()
                            .find(|topic| topic["id"].as_i64() == Some(id))
                        {
                            *topic = updated
                        } else {
                            topics.push(updated)
                        }
                        channels::set_topics(
                            context,
                            channel["chat_id"].as_str().unwrap_or(""),
                            &json!(topics),
                        )?;
                    }
                    Ok(())
                })
                .await?;
                return Ok(Some("topic"));
            }
            return Ok(None);
        }
        if message["is_automatic_forward"].as_bool() == Some(true) {
            return messages::comment(state, context, message).await;
        }
    }
    if let Some(reaction) = update.get("message_reaction_count") {
        let reaction = reaction.clone();
        blocking::run(context, move |context| {
            if let Some(post) = find_message(context, &reaction)? {
                let mut values = serde_json::Map::new();
                for item in reaction["reactions"].as_array().into_iter().flatten() {
                    let kind = &item["type"];
                    let key = kind["emoji"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            kind["custom_emoji_id"]
                                .as_str()
                                .map(|id| format!("custom:{id}"))
                        })
                        .unwrap_or_else(|| kind["type"].as_str().unwrap_or("?").to_owned());
                    values.insert(key, item.get("total_count").cloned().unwrap_or(json!(0)));
                }
                db::reactions(
                    context,
                    post["id"].as_str().unwrap_or(""),
                    &Value::Object(values),
                )?;
            }
            Ok(())
        })
        .await?;
        return Ok(Some("reactions"));
    }
    if let Some(poll) = update.get("poll") {
        let poll = poll.clone();
        blocking::run(context, move |context| {
            if let Some(post) = db::find_poll(context, poll["id"].as_str().unwrap_or(""))? {
                editing::poll_results(
                    context,
                    post["id"].as_str().unwrap_or(""),
                    &poll_results(&poll),
                )?;
            }
            Ok(())
        })
        .await?;
        return Ok(Some("poll"));
    }
    if let Some(member) = update.get("chat_member") {
        return Ok(invite_links::on_member(context, member)
            .await?
            .then_some("join"));
    }
    if let Some(join) = update.get("chat_join_request") {
        return Ok(invite_links::on_join_request(context, join)
            .await?
            .then_some("join_request"));
    }
    Ok(None)
}
