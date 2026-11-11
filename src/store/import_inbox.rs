//! Durable single-use forward receipts. These rows never contain import session keys.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;

use crate::{
    account_context::AccountContext,
    errors::ImportError,
    store::db::{self, Store, decode_row},
};

fn connect(context: &AccountContext) -> Result<Connection, ImportError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection)
}
fn map(error: rusqlite::Error, context: &AccountContext) -> ImportError {
    crate::errors::StoreError::from_sqlite(error, context).into()
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut record = decode_row(row)?;
    if let Some(Value::String(raw)) = record.get("result") {
        let index = row.as_ref().column_index("result")?;
        let value = serde_json::from_str(raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        record.insert("result".into(), value);
    }
    Ok(Value::Object(record))
}

pub fn create(
    context: &AccountContext,
    url: &str,
    sender: i64,
    now: i64,
) -> Result<Value, ImportError> {
    let token = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
    let mut connection = connect(context)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| map(e, context))?;
    transaction
        .execute("delete from telegram_import_inbox", [])
        .map_err(|e| map(e, context))?;
    transaction
        .execute(
            "insert into telegram_import_inbox values(?,?,?,?,'waiting',null,null)",
            params![token, url, sender, now + 600],
        )
        .map_err(|e| map(e, context))?;
    let result = transaction
        .query_row(
            "select * from telegram_import_inbox where id=?",
            [&token],
            row,
        )
        .map_err(|e| map(e, context))?;
    transaction.commit().map_err(|e| map(e, context))?;
    Ok(result)
}
pub fn get(context: &AccountContext, token: &str, now: i64) -> Result<Option<Value>, ImportError> {
    let mut value = connect(context)?
        .query_row(
            "select * from telegram_import_inbox where id=?",
            [token],
            row,
        )
        .optional()
        .map_err(|e| map(e, context))?;
    if let Some(record) = value.as_mut()
        && record["expires_at"].as_i64().is_some_and(|time| time < now)
        && ["waiting", "armed", "processing"].contains(&record["state"].as_str().unwrap_or(""))
    {
        record["state"] = Value::String("expired".into());
    }
    Ok(value)
}
pub fn arm(
    context: &AccountContext,
    token: &str,
    sender: i64,
    now: i64,
) -> Result<bool, ImportError> {
    Ok(connect(context)?.execute("update telegram_import_inbox set state='armed' where id=? and sender_id=? and expires_at>=? and state='waiting'",params![token,sender,now]).map_err(|e|map(e,context))?==1)
}
pub fn claim(
    context: &AccountContext,
    sender: i64,
    now: i64,
) -> Result<Option<Value>, ImportError> {
    connect(context)?.query_row("update telegram_import_inbox set state='processing' where sender_id=? and expires_at>=? and state='armed' returning *",params![sender,now],row).optional().map_err(|e|map(e,context))
}
pub fn finish(
    context: &AccountContext,
    token: &str,
    result: Option<&Value>,
    error: Option<&str>,
) -> Result<(), ImportError> {
    let result = result
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| ImportError::new(500, "cannot encode import result"))?;
    connect(context)?
        .execute(
            "update telegram_import_inbox set state=?,result=?,error=? where id=?",
            params![
                if error.is_some() { "failed" } else { "done" },
                result,
                error,
                token
            ],
        )
        .map_err(|e| map(e, context))?;
    Ok(())
}
