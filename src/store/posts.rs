use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::Value;
use uuid::Uuid;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{Store, decode_row};

pub struct NewPost<'a> {
    pub chat_id: &'a Value,
    pub payload: &'a Value,
    pub draft_id: Option<&'a str>,
    pub status: &'a str,
    pub message_id: Option<i64>,
    pub scheduled_at: Option<i64>,
    pub error: Option<&'a str>,
    pub kind: &'a str,
    pub actor: &'a str,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn chat_text(chat_id: &Value) -> String {
    match chat_id {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn insert(
    connection: &Connection,
    context: &AccountContext,
    new: &NewPost<'_>,
) -> Result<String, StoreError> {
    let id = Uuid::new_v4().simple().to_string();
    let id = id[..12].to_owned();
    let payload = serde_json::to_string(new.payload)
        .map_err(|error| StoreError::Migration(error.to_string()))?;
    connection.execute(
        "insert into posts(id,draft_id,chat_id,message_id,payload,status,scheduled_at,sent_at,error,created_at,kind,actor) values(?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            id,
            new.draft_id,
            chat_text(new.chat_id),
            new.message_id,
            payload,
            new.status,
            new.scheduled_at,
            if new.status == "sent" { Some(now()) } else { None },
            new.error,
            now(),
            new.kind,
            new.actor,
        ],
    ).map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(id)
}

pub fn add(context: &AccountContext, new: NewPost<'_>) -> Result<Value, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let id = insert(&connection, context, &new)?;
    get(context, &id)?.ok_or_else(|| StoreError::Migration("inserted post is missing".to_owned()))
}

pub fn add_scheduled(
    context: &AccountContext,
    chats: &[Value],
    payload: &Value,
    at: i64,
    draft_id: Option<&str>,
    kind: &str,
    actor: &str,
) -> Result<Vec<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let transaction = connection
        .transaction()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    let mut ids = Vec::new();
    for chat in chats {
        ids.push(insert(
            &transaction,
            context,
            &NewPost {
                chat_id: chat,
                payload,
                draft_id,
                status: "scheduled",
                message_id: None,
                scheduled_at: Some(at),
                error: None,
                kind,
                actor,
            },
        )?);
    }
    transaction
        .commit()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    ids.into_iter()
        .map(|id| {
            get(context, &id)?
                .ok_or_else(|| StoreError::Migration("scheduled post is missing".to_owned()))
        })
        .collect()
}

pub fn get(context: &AccountContext, id: &str) -> Result<Option<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    connection
        .query_row("select * from posts where id=?", [id], |row| {
            decode_row(row).map(Value::Object)
        })
        .optional()
        .map_err(Into::into)
}

pub fn list(context: &AccountContext) -> Result<Vec<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    let mut statement = connection.prepare(
        "select * from posts order by coalesce(scheduled_at,sent_at,created_at) desc,rowid desc",
    )?;
    Ok(statement
        .query_map([], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn mark_sent(context: &AccountContext, id: &str, message_id: i64) -> Result<Value, StoreError> {
    Store::connect(context)?.execute(
        "update posts set status='sent',message_id=?,sent_at=?,error=null,retry_at=null where id=?",
        params![message_id, now(), id],
    ).map_err(|error| StoreError::from_sqlite(error, context))?;
    get(context, id)?.ok_or_else(|| StoreError::Migration("sent post is missing".to_owned()))
}

pub fn mark_failed(context: &AccountContext, id: &str, error: &str) -> Result<(), StoreError> {
    Store::connect(context)?
        .execute(
            "update posts set status='failed',error=? where id=?",
            params![error, id],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn update_delivery(
    context: &AccountContext,
    id: &str,
    chat_num: Option<i64>,
    poll_id: Option<&str>,
    comment_status: Option<&str>,
) -> Result<(), StoreError> {
    Store::connect(context)?.execute(
        "update posts set chat_num=?,poll_id=coalesce(?,poll_id),comment_status=coalesce(?,comment_status) where id=?",
        params![chat_num, poll_id, comment_status, id],
    ).map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn set_pinned(context: &AccountContext, id: &str, pinned: bool) -> Result<(), StoreError> {
    Store::connect(context)?
        .execute(
            "update posts set pinned=? where id=?",
            params![i64::from(pinned), id],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn drop_refused(
    context: &AccountContext,
    chat_id: &Value,
    draft_id: &str,
    keep_with: &[&str],
) -> Result<usize, StoreError> {
    let connection = Store::connect(context)?;
    let mut statement = connection
        .prepare("select id,error from posts where status='failed' and draft_id=? and chat_id=?")?;
    let rows = statement
        .query_map(params![draft_id, chat_text(chat_id)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let ids = rows
        .into_iter()
        .filter(|(_, error)| {
            let error = error.as_deref().unwrap_or_default();
            !keep_with.iter().any(|needle| error.contains(needle))
        })
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    let count = ids.len();
    for id in ids {
        connection
            .execute("delete from posts where id=?", [id])
            .map_err(|error| StoreError::from_sqlite(error, context))?;
    }
    Ok(count)
}

pub fn delete(context: &AccountContext, id: &str) -> Result<(), StoreError> {
    let connection = Store::connect(context)?;
    connection
        .execute("delete from post_versions where post_id=?", [id])
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    connection
        .execute("delete from posts where id=?", [id])
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

use rusqlite::OptionalExtension;
