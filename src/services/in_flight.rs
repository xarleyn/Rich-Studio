//! Process-local sends excluded from lease recovery. The mutex only covers finite DB work.
use crate::{account_context::AccountContext, errors::ServiceError, store::scheduler};
use serde_json::Value;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
type Key = (PathBuf, String);
fn registry() -> &'static Mutex<HashSet<Key>> {
    static SENDS: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();
    SENDS.get_or_init(|| Mutex::new(HashSet::new()))
}
pub struct SendGuard(Key);
pub fn contains(context: &AccountContext, id: &str) -> bool {
    registry().lock().map_or(true, |sends| {
        sends.contains(&(context.database_path().to_path_buf(), id.to_owned()))
    })
}
impl Drop for SendGuard {
    fn drop(&mut self) {
        // If poisoned, keeping the id is safer than falsely treating a live send as abandoned.
        if let Ok(mut sends) = registry().lock() {
            sends.remove(&self.0);
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
    sends.insert(key.clone());
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
    sends.insert(key.clone());
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
    let keep = sends
        .iter()
        .filter(|(db, _)| db == path)
        .map(|(_, id)| id.clone())
        .collect::<Vec<_>>();
    Ok(scheduler::recover_ids(context, &keep)?)
}
