use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::errors::StoreError;
use crate::settings::Settings;
use crate::store::db::{self, Store, decode_row};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn hash_secret(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

pub fn database(settings: &Settings) -> Result<Connection, StoreError> {
    let mut connection = Store::connect_root(settings)?;
    db::migrate(&mut connection)?;
    Ok(connection)
}

pub fn add_token(
    settings: &Settings,
    name: &str,
    secret: &str,
    level: &str,
) -> Result<(), StoreError> {
    database(settings)?.execute(
        "insert into tokens(name,hash,level,created_at) values(?,?,?,?)",
        params![name, hash_secret(secret), level, now()],
    )?;
    Ok(())
}

pub fn list_tokens(settings: &Settings) -> Result<Vec<Value>, StoreError> {
    let connection = database(settings)?;
    let mut query = connection.prepare(
        "select name,level,created_at,last_used_at,revoked_at from tokens order by name",
    )?;
    Ok(query
        .query_map([], |row| decode_row(row).map(Value::Object))?
        .collect::<Result<_, _>>()?)
}

pub fn revoke_token(settings: &Settings, name: &str) -> Result<bool, StoreError> {
    let mut connection = database(settings)?;
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "update tokens set revoked_at=? where name=? and revoked_at is null",
        params![now(), name],
    )?;
    transaction.execute("delete from sessions where token_name=?", [name])?;
    transaction.commit()?;
    Ok(changed == 1)
}

pub fn any_tokens(settings: &Settings) -> Result<bool, StoreError> {
    Ok(database(settings)?
        .query_row("select exists(select 1 from tokens)", [], |row| row.get(0))?)
}

pub fn token_by_secret(settings: &Settings, secret: &str) -> Result<Option<Value>, StoreError> {
    let connection = database(settings)?;
    let token = connection
        .query_row(
            "select name,level,last_used_at from tokens where hash=? and revoked_at is null",
            [hash_secret(secret)],
            |row| decode_row(row).map(Value::Object),
        )
        .optional()?;
    if let Some(token) = &token
        && token["last_used_at"].as_i64().unwrap_or(0) < now() - 60
    {
        connection.execute(
            "update tokens set last_used_at=? where name=?",
            params![now(), token["name"].as_str()],
        )?;
    }
    Ok(token)
}

pub fn add_session(
    settings: &Settings,
    secret: &str,
    token_name: Option<&str>,
    ttl: i64,
    key_hash: Option<&str>,
    telegram_id: Option<i64>,
) -> Result<(), StoreError> {
    let mut connection = database(settings)?;
    let transaction = connection.transaction()?;
    transaction.execute("delete from sessions where expires_at<?", [now()])?;
    transaction.execute("insert into sessions(hash,token_name,key_hash,telegram_id,created_at,expires_at) values(?,?,?,?,?,?)", params![hash_secret(secret),token_name,key_hash,telegram_id,now(),now().saturating_add(ttl)])?;
    transaction.commit()?;
    Ok(())
}

pub fn session_by_secret(settings: &Settings, secret: &str) -> Result<Option<Value>, StoreError> {
    Ok(database(settings)?.query_row("select s.token_name,s.key_hash,s.telegram_id,t.level from sessions s left join tokens t on t.name=s.token_name where s.hash=? and s.expires_at>=? and (s.token_name is null or (t.name is not null and t.revoked_at is null))", params![hash_secret(secret),now()], |row| decode_row(row).map(Value::Object)).optional()?)
}

pub fn logout(settings: &Settings, secret: &str) -> Result<(), StoreError> {
    let mut connection = database(settings)?;
    let transaction = connection.transaction()?;
    transaction.execute("delete from sessions where hash=?", [hash_secret(secret)])?;
    transaction.execute(
        "delete from account_sessions where hash=?",
        [hash_secret(secret)],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn save_account(settings: &Settings, user: &Value) -> Result<Value, StoreError> {
    let uid = user["id"]
        .as_i64()
        .ok_or_else(|| StoreError::Migration("missing account identity".to_owned()))?;
    let connection = database(settings)?;
    connection.execute("insert into accounts values(?,?,?,?,?,?) on conflict(telegram_id) do update set first_name=excluded.first_name,last_name=excluded.last_name,username=excluded.username,updated_at=excluded.updated_at", params![uid,user["first_name"].as_str(),user["last_name"].as_str(),user["username"].as_str(),now(),now()])?;
    account(settings, uid)?.ok_or_else(|| StoreError::Migration("account disappeared".to_owned()))
}

pub fn account(settings: &Settings, uid: i64) -> Result<Option<Value>, StoreError> {
    Ok(database(settings)?
        .query_row("select * from accounts where telegram_id=?", [uid], |row| {
            decode_row(row).map(Value::Object)
        })
        .optional()?)
}

pub fn account_ids(settings: &Settings) -> Result<Vec<i64>, StoreError> {
    let connection = database(settings)?;
    let mut query = connection.prepare("select telegram_id from accounts order by telegram_id")?;
    Ok(query
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

pub fn challenge(settings: &Settings, secret: &str, ttl: i64) -> Result<(), StoreError> {
    let mut connection = database(settings)?;
    let transaction = connection.transaction()?;
    transaction.execute("delete from login_states where expires_at<?", [now()])?;
    transaction.execute(
        "insert into login_states values(?,?)",
        params![hash_secret(secret), now().saturating_add(ttl)],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn consume_challenge(settings: &Settings, secret: &str) -> Result<bool, StoreError> {
    Ok(database(settings)?.execute(
        "delete from login_states where hash=? and expires_at>=?",
        params![hash_secret(secret), now()],
    )? == 1)
}

pub fn add_account_session(
    settings: &Settings,
    secret: &str,
    uid: i64,
    ttl: i64,
) -> Result<(), StoreError> {
    let mut connection = database(settings)?;
    let transaction = connection.transaction()?;
    transaction.execute("delete from account_sessions where expires_at<?", [now()])?;
    transaction.execute(
        "insert into account_sessions values(?,?,?)",
        params![hash_secret(secret), uid, now().saturating_add(ttl)],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn account_session(settings: &Settings, secret: &str) -> Result<Option<Value>, StoreError> {
    Ok(database(settings)?.query_row("select a.* from account_sessions s join accounts a on a.telegram_id=s.telegram_id where s.hash=? and s.expires_at>=?", params![hash_secret(secret),now()], |row| decode_row(row).map(Value::Object)).optional()?)
}

pub fn take_budget(
    settings: &Settings,
    key: &str,
    limit: i64,
    seconds: i64,
) -> Result<bool, StoreError> {
    let mut connection = database(settings)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute("delete from public_limits where expires_at<=?", [now()])?;
    let hits: i64 = transaction.query_row("insert into public_limits values(?,?,1) on conflict(key) do update set hits=hits+1 returning hits", params![hash_secret(key),now().saturating_add(seconds)], |row| row.get(0))?;
    transaction.commit()?;
    Ok(hits <= limit)
}
