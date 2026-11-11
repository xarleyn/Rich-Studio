use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::channels;
use crate::telegram::TelegramError;
use crate::telegram::chat::check_chat;
use crate::telegram::client::TelegramTransport;
use crate::telegram::uploads::Uploads;

#[derive(Debug)]
pub enum ChannelError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Store(StoreError),
    Telegram(TelegramError),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(message) | Self::Forbidden(message) | Self::NotFound(message) => {
                formatter.write_str(message)
            }
            Self::Store(error) => error.fmt(formatter),
            Self::Telegram(error) => error.fmt(formatter),
        }
    }
}
impl std::error::Error for ChannelError {}
impl From<StoreError> for ChannelError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<TelegramError> for ChannelError {
    fn from(error: TelegramError) -> Self {
        Self::Telegram(error)
    }
}

#[derive(Clone)]
pub struct ChannelService {
    context: AccountContext,
    transport: TelegramTransport,
    default_chat: String,
}

pub fn from_state(state: &crate::AppState) -> ChannelService {
    ChannelService::new(
        state.account.clone(),
        state.transport(),
        state.settings.default_chat.clone(),
    )
}

pub fn title_of(context: &AccountContext, chat: &Value) -> Result<Value, StoreError> {
    if chat.is_null() || chat.as_str() == Some("") {
        return Ok(Value::Null);
    }
    Ok(channels::get(context, chat)?
        .map(|channel| channel["title"].clone())
        .unwrap_or(Value::Null))
}
pub fn goes_to(
    context: &AccountContext,
    default_chat: &str,
    chats: &Value,
) -> Result<Value, StoreError> {
    let mut ids = Vec::new();
    for chat in chats.as_array().into_iter().flatten() {
        let chat = chat
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| chat.to_string());
        if !ids.contains(&chat) {
            ids.push(chat);
        }
    }
    let default = ids.is_empty();
    if default && !default_chat.is_empty() {
        ids.push(default_chat.to_owned());
    }
    ids.into_iter()
        .map(|chat| {
            Ok(json!({"chat_id":chat,"title":title_of(context,&json!(chat))?,"default":default}))
        })
        .collect::<Result<Vec<_>, StoreError>>()
        .map(Value::Array)
}

impl ChannelService {
    pub async fn run<T, F>(&self, action: F) -> Result<T, ChannelError>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Result<T, ChannelError> + Send + 'static,
    {
        let service = self.clone();
        tokio::task::spawn_blocking(move || action(service))
            .await
            .map_err(|_| {
                ChannelError::Store(StoreError::Migration("операция каналов прервана".into()))
            })?
    }
    pub fn new(
        context: AccountContext,
        transport: TelegramTransport,
        default_chat: String,
    ) -> Self {
        Self {
            context,
            transport,
            default_chat,
        }
    }

    pub async fn check(&self, chat_id: Value) -> Result<Value, ChannelError> {
        let key = key(&chat_id)?;
        let info = check_chat(&self.transport, &self.context, key.clone()).await?;
        let mut saved_key = key
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| key.to_string());
        if self.context.account_id.is_some() {
            let chat_type = info["chat"]["type"].as_str().unwrap_or_default();
            if !["channel", "supergroup", "group"].contains(&chat_type) {
                return Err(ChannelError::Forbidden(
                    "Привяжите канал или группу, где вы администратор.".to_owned(),
                ));
            }
            let uid = self.context.account_id.unwrap_or_default();
            let member = self
                .transport
                .call(
                    &self.context,
                    "getChatMember",
                    json!({"chat_id":info["chat"]["id"],"user_id":uid}),
                    Uploads::default(),
                )
                .await?;
            let status = member["status"].as_str().unwrap_or_default();
            let member_allowed = status == "creator"
                || status == "administrator"
                    && (chat_type != "channel"
                        || member["can_post_messages"].as_bool() == Some(true));
            if !member_allowed
                || info["can_post"].as_bool() != Some(true)
                || !["administrator", "creator"]
                    .contains(&info["status"].as_str().unwrap_or_default())
            {
                return Err(ChannelError::Forbidden(
                    "Добавьте бота администратором с правом публикации сообщений.".to_owned(),
                ));
            }
            saved_key = info["chat"]["id"].to_string();
        }
        let context = self.context.clone();
        let saved_info = info.clone();
        let saved=tokio::task::spawn_blocking(move||{
        let info=saved_info;
        let existing = channels::get(&context, &Value::String(saved_key.clone()))?;
        let chat_id = existing
            .as_ref()
            .and_then(|channel| channel["chat_id"].as_str().map(str::to_owned))
            .unwrap_or(saved_key);
        let title = ["title", "username"]
            .into_iter()
            .find_map(|field| info["chat"][field].as_str().filter(|value| !value.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| info["chat"]["id"].to_string());
        let channel = json!({
            "chat_id":chat_id,
            "chat_num":info["chat"]["id"],
            "title":title,
            "username":info["chat"]["username"],
            "type":info["chat"]["type"],
            "is_forum":if info["chat"]["is_forum"].as_bool() == Some(true) { 1 } else { 0 },
            "topics":existing.as_ref().map(|channel| channel["topics"].clone()).unwrap_or(Value::Null),
            "can_post":if info["can_post"].as_bool() == Some(true) { 1 } else { 0 },
            "rights":json!({"status":info["status"]}).as_object().cloned().unwrap_or_default().into_iter().chain(info["rights"].as_object().cloned().unwrap_or_default()).collect::<serde_json::Map<_,_>>(),
            "checked_at":SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs() as i64),
        });
        channels::save(&context, &channel)
        }).await.map_err(|_|ChannelError::Store(StoreError::Migration("операция каналов прервана".into())))??;
        Ok(
            json!({"bot":info["bot"],"chat":info["chat"],"status":info["status"],"rights":info["rights"],"can_post":info["can_post"],"channel":saved}),
        )
    }

    pub fn list(&self) -> Result<Vec<Value>, ChannelError> {
        let mut output = channels::list(&self.context)?;
        if !self.default_chat.is_empty()
            && !output
                .iter()
                .any(|channel| same(channel, &self.default_chat))
        {
            output.insert(0, json!({"chat_id":self.default_chat,"title":null,"topics":null,"is_forum":0,"checked_at":null}));
        }
        for channel in &mut output {
            channel["is_default"] =
                json!(!self.default_chat.is_empty() && same(channel, &self.default_chat));
            channel["is_forum"] = json!(channel["is_forum"].as_i64().unwrap_or(0) != 0);
            if !channel["topics"].is_array() {
                channel["topics"] = json!([]);
            }
        }
        Ok(output)
    }

    pub fn set_topics(&self, chat_id: &str, topics: Vec<Value>) -> Result<Value, ChannelError> {
        let channel = channels::get(&self.context, &Value::String(chat_id.to_owned()))?
            .ok_or_else(|| {
                ChannelError::NotFound(format!(
                    "канала {chat_id} нет в списке: сначала проверьте его"
                ))
            })?;
        let mut clean = Vec::new();
        for topic in topics {
            let id = topic
                .get("id")
                .and_then(Value::as_i64)
                .filter(|id| *id >= 1)
                .ok_or_else(|| {
                    ChannelError::BadRequest(format!(
                        "id темы — номер темы (message_thread_id), а не {}",
                        topic["id"]
                    ))
                })?;
            let name = topic
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            clean.push(json!({"id":id,"name":if name.is_empty() { format!("Тема {id}") } else { name.to_owned() }}));
        }
        Ok(channels::set_topics(
            &self.context,
            channel["chat_id"].as_str().unwrap_or_default(),
            &Value::Array(clean),
        )?)
    }

    pub fn remove(&self, chat_id: &str) -> Result<(), ChannelError> {
        if let Some(channel) = channels::get(&self.context, &Value::String(chat_id.to_owned()))? {
            channels::delete(
                &self.context,
                channel["chat_id"].as_str().unwrap_or_default(),
            )?;
        }
        Ok(())
    }
}

fn key(chat_id: &Value) -> Result<Value, ChannelError> {
    match chat_id {
        Value::Number(number) => Ok(Value::Number(number.clone())),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Err(ChannelError::BadRequest(
                    "укажите @канал или числовой id".to_owned(),
                ));
            }
            if text
                .trim_start_matches('-')
                .chars()
                .all(|character| character.is_ascii_digit())
            {
                Ok(text
                    .parse::<i64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| Value::String(text.to_owned())))
            } else {
                Ok(Value::String(text.to_owned()))
            }
        }
        _ => Err(ChannelError::BadRequest(
            "укажите @канал или числовой id".to_owned(),
        )),
    }
}

fn same(channel: &Value, chat_id: &str) -> bool {
    channel["chat_id"].as_str() == Some(chat_id)
        || chat_id
            .parse::<i64>()
            .ok()
            .is_some_and(|id| channel["chat_num"].as_i64() == Some(id))
        || channel["username"]
            .as_str()
            .is_some_and(|username| format!("@{username}") == chat_id)
}
