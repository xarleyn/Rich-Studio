use std::fmt;

use regex::Regex;
use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db;
use crate::store::drafts;

#[derive(Clone, Debug)]
pub struct SaveDraft {
    pub id: Option<String>,
    pub title: String,
    pub post: Value,
    pub version: Option<i64>,
    pub post_id: Option<String>,
    pub chat_ids: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum DraftError {
    NotFound(String),
    Conflict { current: Option<Value> },
    Invalid(String),
    Store(StoreError),
}

impl fmt::Display for DraftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "черновика {id} нет"),
            Self::Conflict { .. } => {
                formatter.write_str("черновик изменили в другой вкладке или агентом")
            }
            Self::Invalid(message) => formatter.write_str(message),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DraftError {}

impl From<StoreError> for DraftError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone)]
pub struct DraftService {
    context: AccountContext,
}

impl DraftService {
    pub fn new(context: AccountContext) -> Self {
        Self { context }
    }

    pub fn migrate(&self) -> Result<(), DraftError> {
        db::migrate(&mut db::Store::connect(&self.context)?)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Value>, DraftError> {
        Ok(drafts::list(&self.context)?)
    }

    pub fn get(&self, id: &str) -> Result<Value, DraftError> {
        drafts::get(&self.context, id)?.ok_or_else(|| DraftError::NotFound(id.to_owned()))
    }

    pub fn save(&self, input: SaveDraft) -> Result<Value, DraftError> {
        let old = match input.id.as_deref() {
            Some(id) => drafts::get(&self.context, id)?,
            None => None,
        };
        let chats = match input.chat_ids {
            Some(chats) => Some(normalize_chats(&chats)?),
            None => old
                .as_ref()
                .and_then(|draft| draft.get("chat_ids"))
                .and_then(Value::as_array)
                .map(|chats| {
                    chats
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                }),
        };
        let result = drafts::save(
            &self.context,
            input.id.as_deref(),
            &input.title,
            &input.post,
            input.version,
            input.post_id.as_deref(),
            chats.as_deref(),
        )?;
        result.ok_or_else(|| {
            let current = input
                .id
                .as_deref()
                .and_then(|id| drafts::get(&self.context, id).ok().flatten());
            DraftError::Conflict { current }
        })
    }

    pub fn delete(&self, id: &str) -> Result<(), DraftError> {
        drafts::delete(&self.context, id)?;
        Ok(())
    }
}

pub fn normalize_chats(chat_ids: &[String]) -> Result<Vec<String>, DraftError> {
    let valid = Regex::new(r"@\w+|-?\d+").unwrap_or_else(|error| panic!("chat regex: {error}"));
    let mut output: Vec<String> = Vec::new();
    for raw in chat_ids {
        let chat = raw.trim();
        if chat.is_empty() {
            return Err(DraftError::Invalid(
                "в списке каналов пустой элемент: уберите его или впишите канал".to_owned(),
            ));
        }
        if !valid.is_match(chat) || valid.find(chat).map(|found| found.as_str()) != Some(chat) {
            return Err(DraftError::Invalid(format!(
                "канал {chat:?}: нужен @username или числовой id (-100…)"
            )));
        }
        if !output
            .iter()
            .any(|existing| existing.to_lowercase() == chat.to_lowercase())
        {
            output.push(chat.to_owned());
        }
    }
    Ok(output)
}

pub fn draft_links(public_url: &str, draft_id: &str) -> Value {
    json!({
        "editor_url": format!("{public_url}/?draft={draft_id}"),
        "preview_url": format!("{public_url}/render/draft/{draft_id}")
    })
}

pub async fn run<T, F>(context: AccountContext, action: F) -> Result<T, DraftError>
where
    T: Send + 'static,
    F: FnOnce(&DraftService) -> Result<T, DraftError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let service = DraftService::new(context);
        service.migrate()?;
        action(&service)
    })
    .await
    .map_err(|_| DraftError::Store(StoreError::Migration("операция черновиков прервана".into())))?
}
