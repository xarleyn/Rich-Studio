//! Durable Telegram event ingestion, scoped to the databases owning each object.
use super::{accounts, blocking, bot_dialog, bot_notifications, invite_links, suggested_posts};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    settings::Settings,
    store::{auth, bot_dialog as db, channels, editing, invite_links as link_store, meta, posts},
    telegram::{TelegramError, client::TelegramTransport, uploads::Uploads},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const OFFSET_KEY: &str = "updates_offset";
pub const PREVIEW_KEY: &str = "preview_callbacks";
pub const COMMENT_WAIT: i64 = 86400;
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
// Deserialize legacy JSON object insertion order without changing serde_json semantics globally.
struct Ordered(Vec<(String, Value)>);
impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("preview callback object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Ordered, M::Error> {
                let mut rows = Vec::new();
                while let Some(row) = map.next_entry()? {
                    rows.push(row);
                }
                Ok(Ordered(rows))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
pub async fn remember_preview(
    context: &AccountContext,
    chat: &Value,
    message: i64,
    callbacks: &Value,
) -> Result<(), ServiceError> {
    let key = format!(
        "{}:{message}",
        chat.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| chat.to_string())
    );
    let callbacks = callbacks.clone();
    blocking::run(context, move |context| {
        let raw = meta::get(context, PREVIEW_KEY)?.unwrap_or_else(|| "{}".to_owned());
        let mut rows = serde_json::from_str::<Ordered>(&raw)
            .map_err(|e| ServiceError::Internal(e.to_string()))?
            .0;
        if let Some(row) = rows.iter_mut().find(|(saved, _)| *saved == key) {
            row.1 = callbacks;
        } else {
            rows.push((key, callbacks));
        }
        let keep = rows.len().saturating_sub(20);
        rows.drain(..keep);
        // Explicit serialization preserves the legacy object order for the next append.
        let fields = rows
            .into_iter()
            .map(|(key, value)| format!("{}:{value}", json!(key)))
            .collect::<Vec<_>>()
            .join(",");
        meta::set(context, PREVIEW_KEY, &format!("{{{fields}}}"))?;
        Ok(())
    })
    .await
}
fn preview(context: &AccountContext, chat: i64, message: i64) -> Result<Value, ServiceError> {
    let saved: Value =
        serde_json::from_str(&meta::get(context, PREVIEW_KEY)?.unwrap_or_else(|| "{}".to_owned()))
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
    Ok(saved
        .get(format!("{chat}:{message}"))
        .cloned()
        .unwrap_or_else(|| json!({})))
}
fn has_callbacks(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}
pub fn poll_results(poll: &Value) -> Value {
    json!({"total":poll.get("total_voter_count").unwrap_or(&json!(0)),"is_closed":poll.get("is_closed").unwrap_or(&json!(false)),"options":poll["options"].as_array().map(|options|options.iter().map(|option|json!({"text":option["text"],"voter_count":option.get("voter_count").unwrap_or(&json!(0))})).collect::<Vec<_>>()).unwrap_or_default()})
}
fn find_message(context: &AccountContext, message: &Value) -> Result<Option<Value>, ServiceError> {
    match (
        message["chat"]["id"].as_i64(),
        message["message_id"].as_i64(),
    ) {
        (Some(chat), Some(mid)) => Ok(db::find_post(context, chat, mid)?),
        _ => Ok(None),
    }
}
fn update_chat(context: &AccountContext, update: &Value) -> Result<Option<Value>, ServiceError> {
    if let Some(callback) = update.get("callback_query") {
        let message = &callback["message"];
        if let (Some(chat), Some(mid)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
        ) && (db::find_post(context, chat, mid)?.is_some()
            || has_callbacks(&preview(context, chat, mid)?))
        {
            return Ok(Some(json!(chat)));
        }
        return Ok(None);
    }
    if let Some(poll) = update.get("poll") {
        return Ok(
            db::find_poll(context, poll["id"].as_str().unwrap_or(""))?.map(|post| {
                if post["chat_num"].is_null() {
                    post["chat_id"].clone()
                } else {
                    post["chat_num"].clone()
                }
            }),
        );
    }
    if let Some(reaction) = update.get("message_reaction_count") {
        return Ok(find_message(context, reaction)?.map(|_| reaction["chat"]["id"].clone()));
    }
    let event = [
        "message",
        "edited_message",
        "chat_member",
        "chat_join_request",
    ]
    .into_iter()
    .find_map(|key| update.get(key))
    .unwrap_or(&Value::Null);
    if event["is_automatic_forward"].as_bool() == Some(true) {
        let origin = &event["forward_origin"];
        return Ok(find_message(context, origin)?.map(|_| origin["chat"]["id"].clone()));
    }
    if let Some(link) = event["invite_link"]["invite_link"].as_str() {
        return Ok(link_store::get(context, link)?.map(|link| link["chat_id"].clone()));
    }
    let chat = &event["chat"]["id"];
    Ok(
        if !chat.is_null() && channels::get(context, chat)?.is_some() {
            Some(chat.clone())
        } else {
            None
        },
    )
}
async fn content_callback(state: &AppState, update: &Value) -> Result<bool, ServiceError> {
    if !update["callback_query"].is_object() {
        return Ok(false);
    }
    let Some(uid) = bot_dialog::private_sender(update) else {
        return Ok(false);
    };
    let Some(mid) = update["callback_query"]["message"]["message_id"].as_i64() else {
        return Ok(false);
    };
    let settings = state.settings.clone();
    blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        let context = if settings.public && auth::account(&settings, uid)?.is_some() {
            bot_dialog::context(&settings, uid)
        } else if settings.preview_chat == uid.to_string() {
            AccountContext::root(&settings)
        } else {
            return Ok(false);
        };
        Ok(db::find_post(&context, uid, mid)?.is_some()
            || has_callbacks(&preview(&context, uid, mid)?))
    })
    .await
}
pub async fn handle_update(
    state: &AppState,
    update: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    let spec = state.spec.read().await.clone();
    if super::import_forward::receive(&state.settings, update, &spec)
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?
    {
        let _ = bot_dialog::import_reply(state, update).await;
        return Ok(Some("import"));
    }
    if !content_callback(state, update).await? && bot_dialog::handle(state, update).await? {
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
    if let Some(member) = update.get("my_chat_member") {
        let uid = member["from"]["id"].as_i64();
        if let Some(context) = contexts
            .iter()
            .find(|context| context.account_id.is_some() && context.account_id == uid)
            && ["administrator", "creator"]
                .contains(&member["new_chat_member"]["status"].as_str().unwrap_or(""))
            && !member["chat"]["id"].is_null()
        {
            super::channels::ChannelService::new(context.clone(), transport(state), String::new())
                .check(member["chat"]["id"].clone())
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?;
            return Ok(Some("channel_connected"));
        }
        return Ok(None);
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
fn transport(state: &AppState) -> TelegramTransport {
    state.transport()
}
async fn handle_scoped(
    state: &AppState,
    context: &AccountContext,
    update: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    if let Some(callback) = update.get("callback_query") {
        let callback = callback.clone();
        let callback_copy = callback.clone();
        let mut params = blocking::run(context, move |context| {
            let message = &callback_copy["message"];
            let data = callback_copy["data"].as_str().unwrap_or("");
            let callbacks = if let Some(post) = find_message(context, message)? {
                db::bump_click(context, post["id"].as_str().unwrap_or(""), data)?;
                post["payload"]["callbacks"].clone()
            } else if let (Some(chat), Some(mid)) = (
                message["chat"]["id"].as_i64(),
                message["message_id"].as_i64(),
            ) {
                preview(context, chat, mid)?
            } else {
                Value::Null
            };
            let answer = &callbacks[data];
            let mut params = json!({});
            if answer.is_string() {
                params["text"] = answer.clone();
            } else if let Some(answer) = answer.as_object() {
                for key in ["text", "show_alert", "url", "cache_time"] {
                    if let Some(value) = answer.get(key) {
                        params[key] = value.clone();
                    }
                }
            }
            Ok(params)
        })
        .await?;
        params["callback_query_id"] = callback["id"].clone();
        if let Err(error) = transport(state)
            .call(context, "answerCallbackQuery", params, Uploads::default())
            .await
            && !error.to_string().contains("query is too old")
        {
            return Err(error.into());
        }
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
            return comment(state, context, message).await;
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
async fn comment(
    state: &AppState,
    context: &AccountContext,
    message: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    let origin = &message["forward_origin"];
    if origin["type"] != "channel" {
        return Ok(None);
    }
    let origin_copy = origin.clone();
    let post = blocking::run(context, move |context| {
        let Some(post) = find_message(context, &origin_copy)? else {
            return Ok(None);
        };
        if post["comment_status"] != "waiting"
            || !db::claim_comment(context, post["id"].as_str().unwrap_or(""))?
        {
            return Ok(None);
        }
        Ok(Some(post))
    })
    .await?;
    let Some(post) = post else { return Ok(None) };
    let result = transport(state)
        .send_linked_comment(
            context,
            &origin["chat"]["id"],
            &message["chat"]["id"],
            message["message_id"].as_i64().unwrap_or(0),
            post["payload"]["comment"].clone(),
        )
        .await;
    let (mid,error)=match result {
        Ok(result) if result["message_id"].as_i64().is_some()=>(result["message_id"].as_i64(),None),
        Ok(_)=>(None,Some("первый комментарий: Telegram вернул неполный ответ. Он мог уйти, проверьте обсуждение".to_owned())),
        Err(TelegramError::NoAnswer(error))=>(None,Some(format!("первый комментарий: {error}. Он мог уйти, проверьте обсуждение"))),
        Err(error)=>(None,Some(format!("первый комментарий: {error}")))
    };
    let id = post["id"].as_str().unwrap_or("").to_owned();
    let error_copy = error.clone();
    let saved = blocking::run(context, move |context| {
        db::comment(context, &id, mid, error_copy.as_deref())?;
        Ok(posts::get(context, &id)?)
    })
    .await?;
    if error.is_some()
        && let Some(post) = saved
    {
        let _ =
            bot_notifications::record_event(context, &state.settings, &post, "comment", None).await;
    }
    Ok(Some("comment"))
}
async fn processed(state: &AppState, update: &Value) {
    let id = format!("update-{}", update["update_id"]);
    match crate::logs::scope(id, handle_update(state, update)).await {
        Ok(Some(_)) => {
            let mut status = state.updates_status.write().await;
            status.handled = status.handled.saturating_add(1);
            status.last_at = Some(auth::now());
        }
        Err(error) => {
            state.logs.event("ERROR","rich-studio.updates",&error.to_string(),json!({"event":"update.failed","request_id":format!("update-{}",update["update_id"])}));
            state.updates_status.write().await.error =
                Some(crate::logs::scrub(&state.settings, &error.to_string()));
        }
        _ => {}
    }
}
pub async fn receive_webhook(state: &AppState, update: &Value) -> Result<(), ServiceError> {
    let id = update["update_id"]
        .as_i64()
        .filter(|id| *id >= 0)
        .ok_or_else(|| {
            ServiceError::BadRequest(
                "update_id должен быть неотрицательным целым числом".to_owned(),
            )
        })?;
    if blocking::run(&AccountContext::root(&state.settings), move |context| {
        Ok(meta::claim_webhook_update(context, id, auth::now())?)
    })
    .await?
    {
        processed(state, update).await;
    }
    Ok(())
}
pub async fn poll_once(state: &AppState) -> Result<(), ServiceError> {
    let root = AccountContext::root(&state.settings);
    let mut offset = blocking::run(&root, |context| {
        Ok(meta::get(context, OFFSET_KEY)?.and_then(|text| text.parse::<i64>().ok()))
    })
    .await?;
    let updates = transport(state)
        .call(
            &root,
            "getUpdates",
            json!({"offset":offset,"timeout":25,"allowed_updates":ALLOWED_UPDATES}),
            Uploads::default(),
        )
        .await?;
    let updates = updates.as_array().ok_or_else(|| {
        ServiceError::Internal("Telegram вернул неверный список событий".to_owned())
    })?;
    {
        let mut status = state.updates_status.write().await;
        status.state = "ok".to_owned();
        status.error = None;
    }
    for update in updates {
        let Some(id) = update["update_id"]
            .as_i64()
            .filter(|id| *id >= 0 && *id < i64::MAX)
        else {
            continue;
        };
        if offset.is_some_and(|offset| id < offset) {
            continue;
        }
        offset = Some(id + 1);
        blocking::run(&root, move |context| {
            Ok(meta::set(context, OFFSET_KEY, &(id + 1).to_string())?)
        })
        .await?;
        processed(state, update).await;
    }
    Ok(())
}
pub async fn register_webhook(state: &AppState) -> Result<(), ServiceError> {
    transport(state).call(&AccountContext::root(&state.settings),"setWebhook",json!({"url":format!("{}/api/telegram/webhook",state.settings.public_url),"secret_token":state.settings.webhook_secret,"allowed_updates":ALLOWED_UPDATES,"max_connections":1}),Uploads::default()).await?;
    Ok(())
}
async fn expire(state: &AppState) -> Result<(), ServiceError> {
    let settings = state.settings.clone();
    let contexts = blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        accounts::all_contexts(&settings).map_err(|e| ServiceError::Internal(e.to_string()))
    })
    .await?;
    for context in contexts {
        bot_notifications::expire_comments(
            &context,
            &state.settings,
            auth::now() - COMMENT_WAIT,
            "первый комментарий: пересылка в обсуждение не пришла за сутки",
        )
        .await?;
    }
    Ok(())
}
async fn pause(stopping: &AtomicBool, seconds: u64) {
    for _ in 0..seconds {
        if stopping.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
async fn incoming(state: &AppState, stopping: &AtomicBool) {
    let mut registered = false;
    while !stopping.load(Ordering::Acquire) {
        let status = state.updates_status.read().await.clone();
        state.metrics.beat("updates", 60.0, status.error.as_deref());
        if !state.settings.updates_on {
            pause(stopping, 1).await;
            continue;
        }
        if state.settings.bot_token.is_empty() {
            state.updates_status.write().await.state = "no_token".to_owned();
            pause(stopping, 30).await;
            continue;
        }
        let result = if state.settings.updates_mode == "webhook" {
            if !registered {
                match register_webhook(state).await {
                    Ok(()) => {
                        registered = true;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            }
        } else {
            poll_once(state).await
        };
        let result = match result {
            Ok(()) => expire(state).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                let mut status = state.updates_status.write().await;
                status.state = "ok".to_owned();
                status.error = None;
                drop(status);
                if state.settings.updates_mode == "webhook" {
                    pause(stopping, 60).await;
                }
            }
            Err(error) => {
                let conflict = matches!(
                    error,
                    ServiceError::Telegram(TelegramError::Refused {
                        code: Some(409),
                        ..
                    })
                );
                let mut status = state.updates_status.write().await;
                status.state = if conflict { "conflict" } else { "error" }.to_owned();
                status.error = Some(crate::logs::scrub(&state.settings, &error.to_string()));
                drop(status);
                pause(stopping, if conflict { 60 } else { 10 }).await;
            }
        }
    }
}
async fn reports(state: &AppState, stopping: &AtomicBool) {
    let mut registered = false;
    while !stopping.load(Ordering::Acquire) {
        let mut error = None;
        if !state.settings.bot_token.is_empty() {
            if !registered {
                match bot_dialog::register_commands(state).await {
                    Ok(()) => registered = true,
                    Err(failure) => {
                        state.logs.event(
                            "ERROR",
                            "rich-studio.bot",
                            &failure.to_string(),
                            json!({"event":"bot.menu_failed"}),
                        );
                        error = Some(crate::logs::scrub(&state.settings, &failure.to_string()));
                    }
                }
            }
            if let Err(failure) = bot_notifications::deliver(state).await {
                state.logs.event(
                    "ERROR",
                    "rich-studio.bot",
                    &failure.to_string(),
                    json!({"event":"bot.delivery_failed"}),
                );
                error = Some(crate::logs::scrub(&state.settings, &failure.to_string()));
            }
        }
        state
            .metrics
            .beat("bot_notifications", 10.0, error.as_deref());
        pause(stopping, 10).await;
    }
}
/// Child futures stay owned by the lease worker; aborting it cannot orphan any sender.
pub async fn run(state: AppState, stopping: Arc<AtomicBool>) -> Result<(), ServiceError> {
    tokio::join!(incoming(&state, &stopping), reports(&state, &stopping));
    Ok(())
}
