use crate::{
    account_context::AccountContext,
    errors::StoreError,
    store::db::{self, Store},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub fn get(context: &AccountContext, key: &str) -> Result<Option<String>, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    Ok(db
        .query_row("select value from meta where key=?", [key], |row| {
            row.get(0)
        })
        .optional()?)
}
pub fn set(context: &AccountContext, key: &str, value: &str) -> Result<(), StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    db.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value", params![key,value])
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(())
}
pub fn take_lease(
    context: &AccountContext,
    name: &str,
    owner: &str,
    ttl: f64,
    now: f64,
) -> Result<bool, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    Ok(db.execute("insert into meta(key,value) values(?,?) on conflict(key) do update set value=excluded.value where json_extract(meta.value,'$.owner')=? or json_extract(meta.value,'$.until')<?",
        params![format!("lease:{name}"), json!({"owner":owner,"until":now+ttl}).to_string(),owner,now])
        .map_err(|e| StoreError::from_sqlite(e, context))? == 1)
}
pub fn lease_holder(context: &AccountContext, name: &str) -> Result<Option<String>, StoreError> {
    get(context, &format!("lease:{name}"))?
        .map(|raw| {
            serde_json::from_str::<Value>(&raw)
                .map(|v| v["owner"].as_str().map(str::to_owned))
                .map_err(|e| StoreError::Migration(e.to_string()))
        })
        .transpose()
        .map(Option::flatten)
}
pub fn drop_lease(context: &AccountContext, name: &str, owner: &str) -> Result<(), StoreError> {
    Store::connect(context)?
        .execute(
            "delete from meta where key=? and json_extract(value,'$.owner')=?",
            params![format!("lease:{name}"), owner],
        )
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(())
}
pub fn claim_webhook_update(
    context: &AccountContext,
    id: i64,
    now: i64,
) -> Result<bool, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    let tx = db.transaction()?;
    tx.execute(
        "delete from meta where key like 'webhook-update:%' and cast(value as integer)<?",
        [now - 7 * 86400],
    )?;
    let claimed = tx.execute(
        "insert or ignore into meta(key,value) values(?,?)",
        params![format!("webhook-update:{id}"), now.to_string()],
    )? == 1;
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    Ok(claimed)
}
