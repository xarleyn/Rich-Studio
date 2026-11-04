use crate::{
    account_context::AccountContext,
    errors::StoreError,
    store::db::{self, Store, decode_row},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
const FIELDS: &[&str] = &[
    "state",
    "price_amount",
    "price_currency",
    "send_date",
    "comment",
    "actor",
    "decided_at",
    "message",
];
pub fn key(chat: i64, message: i64) -> String {
    format!("{chat}:{message}")
}
fn rank(state: &str) -> u8 {
    match state {
        "approving" | "declining" => 1,
        "approved" | "declined" => 2,
        "paid" | "refunded" | "failed" => 3,
        _ => 0,
    }
}
fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut row = decode_row(row)?;
    if let Some(Value::String(text)) = row.get("message") {
        let parsed = serde_json::from_str(text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        row.insert("message".to_owned(), parsed);
    }
    Ok(Value::Object(row))
}
fn connect(context: &AccountContext) -> Result<Connection, StoreError> {
    let mut c = Store::connect(context)?;
    db::migrate(&mut c)?;
    Ok(c)
}
pub fn get(context: &AccountContext, id: &str) -> Result<Option<Value>, StoreError> {
    Ok(connect(context)?
        .query_row("select * from suggested_posts where id=?", [id], decode)
        .optional()?)
}
pub fn listing(context: &AccountContext, state: Option<&str>) -> Result<Vec<Value>, StoreError> {
    let db = connect(context)?;
    let mut q=db.prepare("select * from suggested_posts where ?1 is null or state=?1 or (?1='open' and state in ('pending','approving','declining','failed')) order by received_at desc,rowid desc limit 100")?;
    Ok(q.query_map([state], decode)?.collect::<Result<_, _>>()?)
}
fn sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Number(n) if n.as_i64().is_some() => {
            rusqlite::types::Value::Integer(n.as_i64().unwrap_or(0))
        }
        Value::Bool(v) => rusqlite::types::Value::Integer(i64::from(*v)),
        _ => rusqlite::types::Value::Text(value.to_string()),
    }
}
fn patch(db: &Connection, id: &str, fields: &Value) -> Result<(), StoreError> {
    let Some(fields) = fields.as_object() else {
        return Ok(());
    };
    let state: Option<String> = db
        .query_row("select state from suggested_posts where id=?", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let mut sets = Vec::new();
    let mut values = Vec::new();
    for (key, value) in fields {
        if !FIELDS.contains(&key.as_str()) {
            return Err(StoreError::Migration(format!(
                "unknown suggested post column {key}"
            )));
        }
        if key == "state"
            && rank(value.as_str().unwrap_or("")) < rank(state.as_deref().unwrap_or(""))
        {
            continue;
        }
        sets.push(format!("{key}=?"));
        values.push(if key == "message" {
            rusqlite::types::Value::Text(value.to_string())
        } else {
            sql_value(value)
        });
    }
    if !sets.is_empty() {
        values.push(rusqlite::types::Value::Text(id.to_owned()));
        db.execute(
            &format!("update suggested_posts set {} where id=?", sets.join(",")),
            rusqlite::params_from_iter(values),
        )?;
    }
    Ok(())
}
pub fn update(
    context: &AccountContext,
    id: &str,
    fields: &Value,
) -> Result<Option<Value>, StoreError> {
    let mut db = connect(context)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    patch(&tx, id, fields)?;
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    get(context, id)
}
pub fn save(
    context: &AccountContext,
    chat: i64,
    message_id: i64,
    message: &Value,
    fields: &Value,
) -> Result<Value, StoreError> {
    let mut db = connect(context)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = key(chat, message_id);
    let title = message["chat"]["title"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches(" Direct Messages");
    let author = message["from"]["first_name"].as_str().unwrap_or("");
    let last = message["from"]["last_name"].as_str().unwrap_or("");
    let username = message["from"]["username"].as_str().unwrap_or("");
    let author = format!(
        "{}{}",
        [author, last]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        if username.is_empty() {
            String::new()
        } else {
            format!(" (@{username})")
        }
    );
    tx.execute("insert into suggested_posts(id,chat_num,message_id,received_at,channel_title,author,topic_id,message) values(?,?,?,?,?,?,?,?) on conflict(id) do update set channel_title=excluded.channel_title,author=excluded.author,topic_id=excluded.topic_id,message=excluded.message",
        params![id,chat,message_id,crate::store::auth::now(),if title.is_empty(){None}else{Some(title)},author,message["direct_messages_topic"]["topic_id"].as_i64(),message.to_string()]).map_err(|e|StoreError::from_sqlite(e,context))?;
    patch(&tx, &id, fields)?;
    tx.commit()
        .map_err(|e| StoreError::from_sqlite(e, context))?;
    get(context, &id)?.ok_or_else(|| StoreError::Migration("suggested post missing".to_owned()))
}
pub fn claim(
    context: &AccountContext,
    id: &str,
    from: &[&str],
    to: &str,
) -> Result<bool, StoreError> {
    let placeholders = vec!["?"; from.len()].join(",");
    let args = std::iter::once(to)
        .chain(std::iter::once(id))
        .chain(from.iter().copied());
    Ok(connect(context)?
        .execute(
            &format!("update suggested_posts set state=? where id=? and state in ({placeholders})"),
            rusqlite::params_from_iter(args),
        )
        .map_err(|e| StoreError::from_sqlite(e, context))?
        == 1)
}
