use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::errors::StoreError;
use crate::settings::Settings;
use crate::store::auth::{database, hash_secret, now};
use crate::store::db::decode_row;

pub fn list_for(settings: &Settings, uid: i64) -> Result<Vec<Value>, StoreError> {
    let connection = database(settings)?;
    let mut query = connection.prepare("select id,name,level,created_at,expires_at,last_used_at,revoked_at from account_tokens where telegram_id=? order by created_at desc,id")?;
    Ok(query
        .query_map([uid], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<_, _>>()?)
}

pub struct NewToken<'a> {
    pub uid: i64,
    pub id: &'a str,
    pub name: &'a str,
    pub secret: &'a str,
    pub level: &'a str,
    pub expires: Option<i64>,
}

pub fn add(settings: &Settings, token: NewToken<'_>, limit: i64) -> Result<bool, StoreError> {
    let mut connection = database(settings)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let active: i64 = transaction.query_row("select count(*) from account_tokens where telegram_id=? and revoked_at is null and (expires_at is null or expires_at>?)", params![token.uid,now()], |row| row.get(0))?;
    if active >= limit {
        return Ok(false);
    }
    transaction.execute("delete from account_tokens where id in (select id from account_tokens where telegram_id=? and (revoked_at is not null or expires_at<=?) order by created_at desc,id limit -1 offset 100)", params![token.uid,now()])?;
    transaction.execute("insert into account_tokens(id,telegram_id,name,hash,level,created_at,expires_at) values(?,?,?,?,?,?,?)", params![token.id,token.uid,token.name,hash_secret(token.secret),token.level,now(),token.expires])?;
    transaction.commit()?;
    Ok(true)
}

pub fn revoke(settings: &Settings, uid: i64, id: &str) -> Result<bool, StoreError> {
    Ok(database(settings)?.execute(
        "update account_tokens set revoked_at=coalesce(revoked_at,?) where id=? and telegram_id=?",
        params![now(), id, uid],
    )? == 1)
}

pub fn by_secret(settings: &Settings, secret: &str) -> Result<Option<Value>, StoreError> {
    let connection = database(settings)?;
    let token = connection.query_row("select t.id,t.name,t.level,t.telegram_id,t.last_used_at from account_tokens t join accounts a on a.telegram_id=t.telegram_id where t.hash=? and t.revoked_at is null and (t.expires_at is null or t.expires_at>?)", params![hash_secret(secret),now()], |row| decode_row(row).map(Value::Object)).optional()?;
    if let Some(token) = &token
        && token["last_used_at"].as_i64().unwrap_or(0) < now() - 60
    {
        connection.execute(
            "update account_tokens set last_used_at=? where id=?",
            params![now(), token["id"].as_str()],
        )?;
    }
    Ok(token)
}
