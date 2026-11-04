use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

fn sql_value(value: &Value) -> Result<SqlValue, StoreError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| value.as_f64().map(SqlValue::Real))
            .ok_or_else(|| {
                StoreError::Migration("JSON number cannot be stored in SQLite".to_owned())
            }),
        Value::String(value) => Ok(SqlValue::Text(value.clone())),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value)
            .map(SqlValue::Text)
            .map_err(|error| StoreError::Migration(error.to_string())),
    }
}

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{Store, decode_row};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn list(context: &AccountContext) -> Result<Vec<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let mut statement =
        connection.prepare("select * from channels order by coalesce(title,chat_id)")?;
    Ok(statement
        .query_map([], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn get(context: &AccountContext, chat_id: &Value) -> Result<Option<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let text = chat_id
        .as_str()
        .map(str::to_owned)
        .or_else(|| chat_id.as_i64().map(|value| value.to_string()))
        .unwrap_or_default();
    connection.query_row(
        "select * from channels where chat_id=? or chat_num=? or (username is not null and '@' || username=?)",
        params![text, sql_value(chat_id)?, text],
        |row| decode_row(row).map(Value::Object),
    ).optional().map_err(Into::into)
}

pub fn save(context: &AccountContext, channel: &Value) -> Result<Value, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let chat_id = channel["chat_id"].as_str().unwrap_or_default();
    connection.execute(
        "insert into channels(chat_id,chat_num,title,username,type,is_forum,topics,can_post,rights,checked_at,created_at)
         values(?,?,?,?,?,?,?,?,?,?,?)
         on conflict(chat_id) do update set chat_num=excluded.chat_num,title=excluded.title,username=excluded.username,
         type=excluded.type,is_forum=excluded.is_forum,topics=excluded.topics,can_post=excluded.can_post,
         rights=excluded.rights,checked_at=excluded.checked_at",
        params![
            chat_id,
            sql_value(&channel["chat_num"])?,
            sql_value(&channel["title"])?,
            sql_value(&channel["username"])?,
            sql_value(&channel["type"])?,
            channel["is_forum"].as_i64().unwrap_or(0),
            sql_value(&channel["topics"])?,
            sql_value(&channel["can_post"])?,
            sql_value(&channel["rights"])?,
            sql_value(&channel["checked_at"])?,
            now(),
        ],
    )?;
    get(context, &Value::String(chat_id.to_owned()))?
        .ok_or_else(|| StoreError::Migration("channel save returned no row".to_owned()))
}

pub fn set_topics(
    context: &AccountContext,
    chat_id: &str,
    topics: &Value,
) -> Result<Value, StoreError> {
    let connection = Store::connect(context)?;
    connection.execute(
        "update channels set topics=? where chat_id=?",
        params![sql_value(topics)?, chat_id],
    )?;
    get(context, &Value::String(chat_id.to_owned()))?
        .ok_or_else(|| StoreError::Migration("channel not found".to_owned()))
}

pub fn delete(context: &AccountContext, chat_id: &str) -> Result<(), StoreError> {
    Store::connect(context)?.execute("delete from channels where chat_id=?", [chat_id])?;
    Ok(())
}
