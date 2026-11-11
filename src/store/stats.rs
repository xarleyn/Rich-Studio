use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{Store, decode_row};

pub fn post_counts(context: &AccountContext) -> Result<Value, StoreError> {
    let connection = Store::connect(context)?;
    let mut query = connection.prepare("select status,count(*) from posts group by status")?;
    Ok(query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, json!(row.get::<_, i64>(1)?)))
        })?
        .collect::<Result<serde_json::Map<_, _>, _>>()?
        .into())
}

pub fn overdue(context: &AccountContext, before: i64) -> Result<Vec<Value>, StoreError> {
    brief(
        context,
        "select id,chat_id,scheduled_at,retry_at,error from posts where status='scheduled' and coalesce(retry_at,scheduled_at) < ? order by scheduled_at limit 10",
        &[before],
    )
}

pub fn problem_posts(context: &AccountContext, since: i64) -> Result<Vec<Value>, StoreError> {
    brief(
        context,
        "select id,chat_id,status,comment_status,error,coalesce(sent_at,scheduled_at,created_at) as at from posts where (status in ('failed','missed') and coalesce(sent_at,scheduled_at,created_at)>=?) or status='sending' or (comment_status='failed' and coalesce(sent_at,created_at)>=?) order by at desc limit 20",
        &[since, since],
    )
}

pub fn uncertain_sends(context: &AccountContext, since: i64) -> Result<Vec<Value>, StoreError> {
    brief(
        context,
        "select key,scope,response,created_at from idempotency where status='uncertain' and created_at>=? order by created_at desc limit 10",
        &[since],
    )
}

fn brief(context: &AccountContext, sql: &str, args: &[i64]) -> Result<Vec<Value>, StoreError> {
    let connection = Store::connect(context)?;
    let mut query = connection.prepare(sql)?;
    Ok(query
        .query_map(rusqlite::params_from_iter(args), |row| {
            decode_row(row).map(Value::Object)
        })?
        .collect::<Result<_, _>>()?)
}

pub fn version_count(context: &AccountContext) -> Result<i64, StoreError> {
    Ok(Store::connect(context)?
        .query_row("select count(*) from post_versions", [], |row| row.get(0))?)
}

pub fn trim_versions(context: &AccountContext, keep: i64) -> Result<usize, StoreError> {
    Ok(Store::connect(context)?.execute("delete from post_versions where id in (select id from (select id,row_number() over (partition by post_id order by id desc) as n from post_versions) where n>?)",[keep])?)
}
