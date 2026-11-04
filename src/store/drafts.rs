use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::db::{Store, decode_row};
use crate::account_context::AccountContext;
use crate::errors::StoreError;

const MAX_DRAFT_BYTES: usize = 1024 * 1024;
const MAX_DRAFTS: i64 = 1000;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn list(context: &AccountContext) -> Result<Vec<Value>, StoreError> {
    let connection = Store::connect(context)?;
    let mut statement = connection.prepare(
        "select id, title, updated_at, post_id, version, chat_ids from drafts order by updated_at desc",
    )?;
    let rows = statement
        .query_map([], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get(context: &AccountContext, id: &str) -> Result<Option<Value>, StoreError> {
    let connection = Store::connect(context)?;
    connection
        .query_row("select * from drafts where id=?", [id], |row| {
            decode_row(row).map(Value::Object)
        })
        .optional()
        .map_err(Into::into)
}

pub fn save(
    context: &AccountContext,
    id: Option<&str>,
    title: &str,
    payload: &Value,
    version: Option<i64>,
    post_id: Option<&str>,
    chat_ids: Option<&[String]>,
) -> Result<Option<Value>, StoreError> {
    let id = id.map_or_else(
        || Uuid::new_v4().simple().to_string()[..12].to_owned(),
        str::to_owned,
    );
    let data = super::templates::encode(payload)
        .map_err(|error| StoreError::Migration(format!("cannot encode draft: {error}")))?;
    let chats = chat_ids
        .filter(|chats| !chats.is_empty())
        .map(|chats| {
            super::templates::encode(&serde_json::json!(chats))
                .map_err(|error| StoreError::Migration(error.to_string()))
        })
        .transpose()?;
    if !context.root
        && [
            id.as_str(),
            title,
            &data,
            post_id.unwrap_or_default(),
            chats.as_deref().unwrap_or_default(),
        ]
        .iter()
        .map(|value| value.len())
        .sum::<usize>()
            > MAX_DRAFT_BYTES
    {
        return Err(StoreError::Quota(
            "Черновик вместе с названием и параметрами больше 1 МБ.".to_owned(),
        ));
    }
    let mut connection = Store::connect(context)?;
    let transaction = connection.transaction()?;
    if !context.root {
        let exists: Option<i64> = transaction
            .query_row("select 1 from drafts where id=?", [&id], |row| row.get(0))
            .optional()?;
        let count: i64 =
            transaction.query_row("select count(*) from drafts", [], |row| row.get(0))?;
        if exists.is_none() && count >= MAX_DRAFTS {
            return Err(StoreError::Quota(
                "Лимит черновиков учётки исчерпан. Удалите ненужные черновики.".to_owned(),
            ));
        }
    }
    let now = now();
    if let Some(version) = version {
        let updated = transaction.execute(
            "update drafts set title=?, payload=?, updated_at=?, post_id=?, chat_ids=?, version=version+1 where id=? and version=?",
            params![title, data, now, post_id, chats, id, version],
        );
        match updated {
            Ok(1) => {}
            Ok(_) => return Ok(None),
            Err(error) => return Err(StoreError::from_sqlite(error, context)),
        }
    } else {
        let result = transaction.execute(
            "insert into drafts(id,title,payload,created_at,updated_at,post_id,chat_ids) values(?,?,?,?,?,?,?)
             on conflict(id) do update set title=excluded.title, payload=excluded.payload,
             updated_at=excluded.updated_at, post_id=excluded.post_id, chat_ids=excluded.chat_ids,
             version=drafts.version+1",
            params![id, title, data, now, now, post_id, chats],
        );
        if let Err(error) = result {
            return Err(StoreError::from_sqlite(error, context));
        }
    }
    let record = transaction
        .query_row("select * from drafts where id=?", [&id], |row| {
            decode_row(row).map(Value::Object)
        })
        .optional()?;
    transaction
        .commit()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(record)
}

pub fn delete(context: &AccountContext, id: &str) -> Result<(), StoreError> {
    Store::connect(context)?.execute("delete from drafts where id=?", [id])?;
    Ok(())
}

pub fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
