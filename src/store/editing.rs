use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{Store, decode_row};

fn database(context: &AccountContext) -> Result<rusqlite::Connection, StoreError> {
    let mut connection = Store::connect(context)?;
    crate::store::db::migrate(&mut connection)?;
    Ok(connection)
}

pub fn claim_version(
    context: &AccountContext,
    id: &str,
    expected: Option<i64>,
) -> Result<Option<i64>, StoreError> {
    database(context)?.query_row("update posts set version=version+1 where id=? and (? is null or version=?) returning version",params![id,expected,expected],|row| row.get(0)).optional().map_err(|error| StoreError::from_sqlite(error,context))
}

pub fn release_version(context: &AccountContext, id: &str, taken: i64) -> Result<(), StoreError> {
    database(context)?
        .execute(
            "update posts set version=version-1 where id=? and version=?",
            params![id, taken],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn versions(context: &AccountContext, id: &str) -> Result<Vec<Value>, StoreError> {
    let connection = database(context)?;
    let mut query = connection.prepare(
        "select id,payload,replaced_at,actor from post_versions where post_id=? order by id desc",
    )?;
    Ok(query
        .query_map([id], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<_, _>>()?)
}

fn replace(
    context: &AccountContext,
    id: &str,
    new: &Value,
    old: &Value,
    actor: &str,
    taken: i64,
    waiting: bool,
) -> Result<bool, StoreError> {
    let mut connection = database(context)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let sql = if waiting {
        "update posts set payload=? where id=? and version=? and status in ('scheduled','missed')"
    } else {
        "update posts set payload=? where id=? and version=? and status='sent'"
    };
    let changed = transaction
        .execute(sql, params![new.to_string(), id, taken])
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    if changed != 1 {
        return Ok(false);
    }
    transaction
        .execute(
            "insert into post_versions(post_id,payload,replaced_at,actor) values(?,?,?,?)",
            params![id, old.to_string(), crate::store::auth::now(), actor],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    transaction
        .commit()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(true)
}

pub fn replace_scheduled(
    context: &AccountContext,
    id: &str,
    new: &Value,
    old: &Value,
    actor: &str,
    taken: i64,
) -> Result<bool, StoreError> {
    replace(context, id, new, old, actor, taken, true)
}
pub fn replace_sent(
    context: &AccountContext,
    id: &str,
    new: &Value,
    old: &Value,
    actor: &str,
    taken: i64,
) -> Result<bool, StoreError> {
    replace(context, id, new, old, actor, taken, false)
}

pub fn find_sent(
    context: &AccountContext,
    chat: &Value,
    message_id: i64,
) -> Result<Option<Value>, StoreError> {
    let chat = chat
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| chat.to_string());
    Ok(database(context)?.query_row("select * from posts where message_id=? and status='sent' and (chat_id=? or chat_num=?) order by created_at desc",params![message_id,chat,chat],|row| decode_row(row).map(Value::Object)).optional()?)
}

pub fn reschedule(context: &AccountContext, id: &str, at: i64) -> Result<bool, StoreError> {
    Ok(database(context)?.execute("update posts set status='scheduled',scheduled_at=?,retry_at=null,error=null where id=? and status in ('scheduled','missed')",params![at,id]).map_err(|error| StoreError::from_sqlite(error,context))? == 1)
}

pub fn cancel(context: &AccountContext, id: &str) -> Result<bool, StoreError> {
    Ok(database(context)?
        .execute(
            "update posts set status='cancelled' where id=? and status in ('scheduled','missed')",
            [id],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?
        == 1)
}

pub fn deleted(context: &AccountContext, id: &str) -> Result<(), StoreError> {
    database(context)?
        .execute("update posts set status='deleted' where id=?", [id])
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn poll_results(context: &AccountContext, id: &str, result: &Value) -> Result<(), StoreError> {
    database(context)?
        .execute(
            "update posts set poll_results=? where id=?",
            params![result.to_string(), id],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn remove(context: &AccountContext, id: &str) -> Result<(), StoreError> {
    let mut connection = database(context)?;
    let transaction = connection.transaction()?;
    transaction.execute("delete from post_versions where post_id=?", [id])?;
    transaction.execute("delete from posts where id=? and status='failed'", [id])?;
    transaction.commit()?;
    Ok(())
}

pub fn list(
    context: &AccountContext,
    limit: i64,
    status: Option<&str>,
    chat: Option<&str>,
) -> Result<Vec<Value>, StoreError> {
    let connection = database(context)?;
    let mut query = connection.prepare("select id,draft_id,chat_id,message_id,status,scheduled_at,sent_at,error,created_at,kind,actor,version,pinned,comment_status,reactions,clicks,poll_results,payload,json_extract(payload,'$.poll.question') as poll_question from posts where (? is null or status=?) and (? is null or chat_id=? or chat_num=?) order by coalesce(scheduled_at,sent_at,created_at) desc,rowid desc limit ?")?;
    Ok(query
        .query_map(params![status, status, chat, chat, chat, limit], |row| {
            decode_row(row).map(Value::Object)
        })?
        .collect::<Result<_, _>>()?)
}
