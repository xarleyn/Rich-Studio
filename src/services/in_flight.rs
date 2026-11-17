//! Process-local sends excluded from lease recovery. The mutex only covers finite DB work.
use crate::{account_context::AccountContext, errors::ServiceError, store::scheduler};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
type Key = (PathBuf, String);
fn registry() -> &'static Mutex<HashMap<Key, usize>> {
    static SENDS: OnceLock<Mutex<HashMap<Key, usize>>> = OnceLock::new();
    SENDS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub struct SendGuard(Key);
pub fn contains(context: &AccountContext, id: &str) -> bool {
    registry().lock().map_or(true, |sends| {
        sends.contains_key(&(context.database_path().to_path_buf(), id.to_owned()))
    })
}
impl Drop for SendGuard {
    fn drop(&mut self) {
        // If poisoned, keeping the id is safer than falsely treating a live send as abandoned.
        if let Ok(mut sends) = registry().lock()
            && let Some(owners) = sends.get_mut(&self.0)
        {
            *owners = owners.saturating_sub(1);
            if *owners == 0 {
                sends.remove(&self.0);
            }
        }
    }
}
pub fn record(
    context: &AccountContext,
    action: impl FnOnce() -> Result<Value, ServiceError>,
) -> Result<(Value, SendGuard), ServiceError> {
    let mut sends = registry()
        .lock()
        .map_err(|_| ServiceError::Internal("send registry unavailable".to_owned()))?;
    let record = action()?;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| ServiceError::Internal("post id missing".to_owned()))?
        .to_owned();
    let key = (context.database_path().to_path_buf(), id);
    *sends.entry(key.clone()).or_default() += 1;
    Ok((record, SendGuard(key)))
}
pub fn claim(
    context: &AccountContext,
    id: &str,
    at: i64,
) -> Result<Option<SendGuard>, ServiceError> {
    let mut sends = registry()
        .lock()
        .map_err(|_| ServiceError::Internal("send registry unavailable".to_owned()))?;
    if !scheduler::claim(context, id, at)? {
        return Ok(None);
    }
    let key = (context.database_path().to_path_buf(), id.to_owned());
    *sends.entry(key.clone()).or_default() += 1;
    Ok(Some(SendGuard(key)))
}
pub fn recover(context: &AccountContext) -> Result<usize, ServiceError> {
    recover_ids(context).map(|ids| ids.len())
}
pub fn recover_ids(context: &AccountContext) -> Result<Vec<String>, ServiceError> {
    let sends = registry()
        .lock()
        .map_err(|_| ServiceError::Internal("send registry unavailable".to_owned()))?;
    let path = context.database_path();
    let mut keep = sends
        .keys()
        .filter(|(db, _)| db == path)
        .map(|(_, id)| id.clone())
        .collect::<Vec<_>>();
    keep.extend(super::publisher::unrecorded_ids(context));
    Ok(scheduler::recover_ids(context, &keep)?)
}

fn comments() -> &'static Mutex<HashMap<Key, usize>> {
    static COMMENTS: OnceLock<Mutex<HashMap<Key, usize>>> = OnceLock::new();
    COMMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct CommentGuard {
    key: Key,
    _send: SendGuard,
}

pub fn comment(context: &AccountContext, id: &str) -> Result<CommentGuard, ServiceError> {
    let key = (context.database_path().to_owned(), id.to_owned());
    let mut sends = registry()
        .lock()
        .map_err(|_| ServiceError::Internal("send registry unavailable".to_owned()))?;
    let mut active = comments()
        .lock()
        .map_err(|_| ServiceError::Internal("comment registry unavailable".to_owned()))?;
    *sends.entry(key.clone()).or_default() += 1;
    *active.entry(key.clone()).or_default() += 1;
    Ok(CommentGuard {
        key: key.clone(),
        _send: SendGuard(key),
    })
}

pub fn comment_contains(context: &AccountContext, id: &str) -> bool {
    comments().lock().map_or(true, |active| {
        active.contains_key(&(context.database_path().to_owned(), id.to_owned()))
    })
}

impl Drop for CommentGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = comments().lock()
            && let Some(owners) = active.get_mut(&self.key)
        {
            *owners = owners.saturating_sub(1);
            if *owners == 0 {
                active.remove(&self.key);
            }
        }
    }
}
