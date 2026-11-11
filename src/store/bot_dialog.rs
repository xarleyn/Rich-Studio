//! Root contact preferences; every notification remains in its account database.
use crate::{
    account_context::AccountContext,
    errors::StoreError,
    settings::Settings,
    store::{
        auth,
        db::{self, Store, decode_row},
    },
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

pub fn contact(settings: &Settings, uid: i64) -> Result<Option<Value>, StoreError> {
    let connection = auth::database(settings)?;
    Ok(connection
        .query_row(
            "select * from bot_contacts where telegram_id=?",
            [uid],
            |row| decode_row(row).map(Value::Object),
        )
        .optional()?)
}
pub fn touch(
    settings: &Settings,
    uid: i64,
    language: &str,
    mode: Option<&str>,
    start: bool,
) -> Result<Value, StoreError> {
    let connection = auth::database(settings)?;
    connection.execute("insert into bot_contacts(telegram_id,language,mode,updated_at) values(?,?,?,?) on conflict(telegram_id) do update set language=excluded.language,revision=revision+case when ? is not null and mode!=? then 1 else 0 end,mode=coalesce(?,bot_contacts.mode),updated_at=excluded.updated_at",params![uid,language,mode.unwrap_or(if start {"all"}else{"off"}),auth::now(),mode,mode,mode])?;
    contact(settings, uid)?
        .ok_or_else(|| StoreError::Migration("contact missing after save".to_owned()))
}
pub fn disable(settings: &Settings, uid: i64) -> Result<(), StoreError> {
    auth::database(settings)?.execute("update bot_contacts set mode='off',revision=revision+1,updated_at=? where telegram_id=? and mode!='off'",params![auth::now(),uid])?;
    Ok(())
}
pub fn enqueue(
    context: &AccountContext,
    key: &str,
    recipient: i64,
    category: &str,
    body: &Value,
) -> Result<(), StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    connection.execute("insert or ignore into bot_outbox(event_key,recipient,category,body,created_at) values(?,?,?,?,?)",params![key,recipient,category,body.to_string(),auth::now()]).map_err(|e|StoreError::from_sqlite(e,context))?;
    Ok(())
}
pub fn due(context: &AccountContext, limit: i64) -> Result<Vec<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    connection.execute(
        "delete from bot_outbox where created_at<?",
        [auth::now() - 7 * 86400],
    )?;
    let mut statement=connection.prepare("select * from bot_outbox where state='pending' and retry_at<=? order by created_at,rowid limit ?")?;
    let values = statement
        .query_map(params![auth::now(), limit], |row| {
            decode_row(row).map(Value::Object)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    values
        .into_iter()
        .map(|mut value| {
            value["body"] = serde_json::from_str(value["body"].as_str().unwrap_or("{}"))
                .map_err(|e| StoreError::Migration(e.to_string()))?;
            Ok(value)
        })
        .collect()
}
pub fn claim(context: &AccountContext, key: &str) -> Result<bool, StoreError> {
    Ok(Store::connect(context)?.execute("update bot_outbox set state='attempted',attempts=attempts+1 where event_key=? and state='pending'",[key]).map_err(|e|StoreError::from_sqlite(e,context))?==1)
}
pub fn finish(
    context: &AccountContext,
    key: &str,
    state: &str,
    retry_at: i64,
) -> Result<(), StoreError> {
    Store::connect(context)?
        .execute(
            "update bot_outbox set state=?,retry_at=? where event_key=?",
            params![state, retry_at, key],
        )
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(())
}
pub fn expire_comments(
    context: &AccountContext,
    before: i64,
    error: &str,
) -> Result<Vec<String>, StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    let tx = connection.transaction()?;
    let ids = tx
        .prepare("select id from posts where comment_status='waiting' and sent_at<?")?
        .query_map([before], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    tx.execute("update posts set comment_status='failed',error=coalesce(error || '; ','') || ? where comment_status='waiting' and sent_at<?",params![error,before])?;
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(ids)
}

pub fn find_post(
    context: &AccountContext,
    chat: i64,
    message: i64,
) -> Result<Option<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection.query_row("select * from posts where chat_num=? and message_id=? order by created_at desc,rowid desc",params![chat,message],|row|decode_row(row).map(Value::Object)).optional()?)
}
pub fn find_poll(context: &AccountContext, poll: &str) -> Result<Option<Value>, StoreError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection
        .query_row("select * from posts where poll_id=?", [poll], |row| {
            decode_row(row).map(Value::Object)
        })
        .optional()?)
}
pub fn bump_click(context: &AccountContext, id: &str, data: &str) -> Result<(), StoreError> {
    // JSON object updates under an immediate transaction also support quotes/dots in callback data.
    let mut connection = Store::connect(context)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let raw: Option<String> = tx
        .query_row("select clicks from posts where id=?", [id], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    let mut clicks: Value = raw
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    clicks[data] = Value::from(clicks[data].as_i64().unwrap_or(0).saturating_add(1));
    tx.execute(
        "update posts set clicks=? where id=?",
        params![clicks.to_string(), id],
    )?;
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(())
}
pub fn reactions(context: &AccountContext, id: &str, reactions: &Value) -> Result<(), StoreError> {
    Store::connect(context)?.execute(
        "update posts set reactions=? where id=?",
        params![reactions.to_string(), id],
    )?;
    Ok(())
}
pub fn claim_comment(context: &AccountContext, id: &str) -> Result<bool, StoreError> {
    // Persistent claim survives cancellation and a failed commit after a Telegram send.
    Ok(Store::connect(context)?
        .execute(
            "update posts set comment_status='sending' where id=? and comment_status='waiting'",
            [id],
        )
        .map_err(|e| StoreError::from_sqlite(e, context))?
        == 1)
}
pub fn comment(
    context: &AccountContext,
    id: &str,
    message: Option<i64>,
    error: Option<&str>,
) -> Result<(), StoreError> {
    Store::connect(context)?.execute("update posts set comment_status=?,comment_message_id=?,error=case when ? is null then error else coalesce(error || '; ','') || ? end where id=?",params![if error.is_some(){"failed"}else{"sent"},message,error,error,id]).map_err(|e|StoreError::from_sqlite(e,context))?;
    Ok(())
}
pub fn recover_comments(context: &AccountContext) -> Result<(), StoreError> {
    Store::connect(context)?.execute("update posts set comment_status='failed',error=coalesce(error || '; ','') || 'Комментарий мог уйти: отправка прервалась. Проверьте обсуждение перед повтором.' where comment_status='sending'",[])?;
    Ok(())
}
