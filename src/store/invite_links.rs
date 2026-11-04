use crate::{
    account_context::AccountContext,
    errors::StoreError,
    store::db::{self, Store, decode_row},
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
pub fn get(context: &AccountContext, link: &str) -> Result<Option<Value>, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    Ok(db
        .query_row(
            "select * from invite_links where invite_link=?",
            [link],
            |r| decode_row(r).map(Value::Object),
        )
        .optional()?)
}
pub fn listing(context: &AccountContext, chat: Option<&str>) -> Result<Vec<Value>, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    let mut q=db.prepare("select * from invite_links where ? is null or chat_id=? order by created_at desc,rowid desc")?;
    Ok(
        q.query_map(params![chat, chat], |r| decode_row(r).map(Value::Object))?
            .collect::<Result<_, _>>()?,
    )
}
pub fn save(
    context: &AccountContext,
    link: &str,
    chat: &str,
    actor: &str,
    record: &Value,
) -> Result<Value, StoreError> {
    let mut db = Store::connect(context)?;
    db::migrate(&mut db)?;
    db.execute("insert into invite_links(invite_link,chat_id,actor,created_at,name,kind,expire_date,member_limit,creates_join_request,subscription_period,subscription_price,is_revoked) values(?,?,?,?,?,?,?,?,?,?,?,?) on conflict(invite_link) do update set name=excluded.name,kind=excluded.kind,expire_date=excluded.expire_date,member_limit=excluded.member_limit,creates_join_request=excluded.creates_join_request,subscription_period=excluded.subscription_period,subscription_price=excluded.subscription_price,is_revoked=excluded.is_revoked",params![
        link,chat,actor,crate::store::auth::now(),record["name"].as_str(),if record["subscription_price"].as_i64().is_some_and(|v|v>0){"subscription"}else{"invite"},record["expire_date"].as_i64(),record["member_limit"].as_i64(),i64::from(record["creates_join_request"].as_bool().unwrap_or(false)),record["subscription_period"].as_i64(),record["subscription_price"].as_i64(),i64::from(record["is_revoked"].as_bool().unwrap_or(false))
    ]).map_err(|e|StoreError::from_sqlite(e,context))?;
    get(context, link)?.ok_or_else(|| StoreError::Migration("saved invite missing".to_owned()))
}
pub fn count(context: &AccountContext, link: &str, join: bool) -> Result<bool, StoreError> {
    let sql = if join {
        "update invite_links set joined=joined+1,last_join_at=?2 where invite_link=?1"
    } else {
        "update invite_links set requests=requests+1 where invite_link=?1 and ?2>=0"
    };
    Ok(Store::connect(context)?
        .execute(sql, params![link, crate::store::auth::now()])
        .map_err(|e| StoreError::from_sqlite(e, context))?
        == 1)
}
