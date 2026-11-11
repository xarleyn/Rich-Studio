use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde_json::Value;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{Store, decode_row};

pub const TTL: i64 = 24 * 3600;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn claim(
    context: &AccountContext,
    key: &str,
    scope: &str,
    fingerprint: &str,
) -> Result<Option<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let now = now();
    connection
        .execute(
            "delete from idempotency where created_at < ? and status='done'",
            [now - TTL],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    let inserted = connection.execute(
        "insert or ignore into idempotency(key,scope,fingerprint,status,created_at) values(?,?,?,'pending',?)",
        params![key, scope, fingerprint, now],
    ).map_err(|error| StoreError::from_sqlite(error, context))?;
    if inserted == 1 {
        return Ok(None);
    }
    let existing = connection.query_row("select * from idempotency where key=?", [key], |row| {
        decode_row(row).map(Value::Object)
    })?;
    let mut existing = existing.as_object().cloned().unwrap_or_default();
    if let Some(response) = existing
        .get("response")
        .and_then(Value::as_str)
        .filter(|response| !response.is_empty())
    {
        let decoded = serde_json::from_str(response).map_err(|error| {
            StoreError::Migration(format!("invalid idempotency response: {error}"))
        })?;
        existing.insert("response".to_owned(), decoded);
    }
    Ok(Some(Value::Object(existing)))
}

pub fn finish(
    context: &AccountContext,
    key: &str,
    status: &str,
    response: &Value,
) -> Result<(), StoreError> {
    let connection = Store::connect(context)?;
    let response = serde_json::to_string(response)
        .map_err(|error| StoreError::Migration(error.to_string()))?;
    connection
        .execute(
            "update idempotency set status=?,response=? where key=?",
            params![status, response, key],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn release(context: &AccountContext, key: &str) -> Result<(), StoreError> {
    Store::connect(context)?
        .execute("delete from idempotency where key=?", [key])
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn purge(context: &AccountContext, at: Option<i64>) -> Result<u64, StoreError> {
    let at = at.unwrap_or_else(now);
    let deleted = Store::connect(context)?
        .execute(
            "delete from idempotency where created_at < ? and status='done'",
            [at - TTL],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(deleted as u64)
}
