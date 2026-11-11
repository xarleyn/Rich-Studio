use crate::{
    account_context::AccountContext,
    errors::StoreError,
    store::db::{self, Store, decode_row},
};
use rusqlite::params;
use serde_json::Value;

// Also appears in rows recovered by the Python runtime.
pub const INTERRUPTED_SEND: &str =
    "сервис остановился во время отправки; проверьте канал, пост мог уйти";

pub fn due(context: &AccountContext, now: i64) -> Result<Vec<Value>, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    let mut query = db.prepare("select * from posts where status='scheduled' and coalesce(retry_at,scheduled_at)<=? order by scheduled_at,created_at")?;
    Ok(query
        .query_map([now], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<_, _>>()?)
}
pub fn claim(context: &AccountContext, id: &str, at: i64) -> Result<bool, StoreError> {
    Ok(Store::connect(context)?.execute("update posts set status='sending' where id=? and status='scheduled' and coalesce(retry_at,scheduled_at)<=?", params![id,at])
        .map_err(|e| StoreError::from_sqlite(e,context))? == 1)
}
pub fn missed(context: &AccountContext, id: &str, error: &str) -> Result<bool, StoreError> {
    Ok(Store::connect(context)?
        .execute(
            "update posts set status='missed',error=? where id=? and status='sending'",
            params![error, id],
        )
        .map_err(|e| StoreError::from_sqlite(e, context))?
        == 1)
}
pub fn retry(context: &AccountContext, id: &str, at: i64, error: &str) -> Result<(), StoreError> {
    Store::connect(context)?.execute("update posts set status='scheduled',retry_at=?,error=? where id=? and status='sending'",params![at,error,id])
        .map_err(|e| StoreError::from_sqlite(e,context))?;
    Ok(())
}
pub fn recover(context: &AccountContext, keep: &[String]) -> Result<usize, StoreError> {
    recover_ids(context, keep).map(|ids| ids.len())
}
pub fn recover_ids(context: &AccountContext, keep: &[String]) -> Result<Vec<String>, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    let mut recovered = Vec::new();
    let tx = db.transaction()?;
    let ids = {
        let mut query = tx.prepare("select id from posts where status='sending'")?;
        query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for id in ids {
        if !keep.contains(&id) {
            let changed = tx.execute(
                "update posts set status='failed',error=? where id=? and status='sending'",
                params![INTERRUPTED_SEND, id],
            )?;
            if changed == 1 {
                recovered.push(id);
            }
        }
    }
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(recovered)
}
