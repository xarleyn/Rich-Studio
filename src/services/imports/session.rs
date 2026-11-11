use crate::{account_context::AccountContext, errors::ImportError, settings::Settings};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

static ACTIVE: OnceLock<Mutex<HashSet<Option<i64>>>> = OnceLock::new();

pub struct ActiveImport(Option<i64>);
impl ActiveImport {
    pub fn acquire(uid: Option<i64>) -> Result<Self, ImportError> {
        let mut active = ACTIVE
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| ImportError::new(500, "Импорт недоступен."))?;
        if active.contains(&uid) || active.len() >= 4 {
            return Err(ImportError::new(
                429,
                "Импорт уже выполняется. Дождитесь завершения.",
            ));
        }
        active.insert(uid);
        Ok(Self(uid))
    }
}
impl Drop for ActiveImport {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.get_or_init(Default::default).lock() {
            active.remove(&self.0);
        }
    }
}

pub fn session_path(settings: &Settings, context: &AccountContext) -> PathBuf {
    context
        .account_id
        .map_or_else(
            || settings.data_dir.clone(),
            |uid| settings.account_dir(uid),
        )
        .join("telegram-import.session.txt")
}
pub fn status(settings: &Settings, context: &AccountContext) -> Value {
    json!({"mtproto_configured":settings.import_api_id!=0&&!settings.import_api_hash.is_empty(),"own_session":session_path(settings,context).is_file(),"bot_username":settings.bot_username,"forward_available":settings.updates_on&&!settings.bot_token.is_empty()&&(context.account_id.is_some()||(!settings.preview_chat.is_empty()&&settings.preview_chat.bytes().all(|ch|ch.is_ascii_digit())))})
}
