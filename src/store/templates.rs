//! Account-local template records, compatible with migration 0015 and Python JSON.

use std::fmt;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::db::{self, Store, decode_row};

pub const MAX_TEMPLATES: i64 = 200;
pub const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum TemplateError {
    Invalid(String),
    Conflict(String),
    NotFound,
    TooLarge(String),
    Store(StoreError),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Conflict(message) | Self::TooLarge(message) => {
                formatter.write_str(message)
            }
            Self::NotFound => formatter.write_str("Шаблон не найден."),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TemplateError {}

impl From<StoreError> for TemplateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<rusqlite::Error> for TemplateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

struct PythonSpacing;

impl serde_json::ser::Formatter for PythonSpacing {
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}

/// Keep the legacy size boundary, including the spaces used by json.dumps.
pub fn encode(value: &Value) -> Result<String, TemplateError> {
    let mut buffer = Vec::new();
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut buffer,
            PythonSpacing,
        ))
        .map_err(|error| StoreError::Migration(format!("cannot encode template: {error}")))?;
    String::from_utf8(buffer)
        .map_err(|error| StoreError::Migration(format!("invalid template encoding: {error}")))
        .map_err(Into::into)
}

fn connect(context: &AccountContext) -> Result<Connection, TemplateError> {
    let mut connection = Store::connect(context)?;
    db::migrate(&mut connection)?;
    Ok(connection)
}

fn decode(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut record = decode_row(row)?;
    if let Some(Value::String(raw)) = record.get("fields") {
        let fields = serde_json::from_str(raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                row.as_ref().column_index("fields").unwrap_or(0),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        record.insert("fields".to_owned(), fields);
    }
    Ok(Value::Object(record))
}

pub fn list(context: &AccountContext) -> Result<Vec<Value>, TemplateError> {
    let connection = connect(context)?;
    let mut statement = connection.prepare(
        "select id,title,fields,version,created_at,updated_at from post_templates order by updated_at desc,id",
    )?;
    Ok(statement
        .query_map([], decode)?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn get(context: &AccountContext, id: &str) -> Result<Value, TemplateError> {
    connect(context)?
        .query_row("select * from post_templates where id=?", [id], decode)
        .optional()?
        .ok_or(TemplateError::NotFound)
}

pub fn save(
    context: &AccountContext,
    title: &str,
    payload: &Value,
    fields: &Value,
    id: Option<&str>,
    version: Option<i64>,
) -> Result<Value, TemplateError> {
    let data = encode(payload)?;
    let schema = encode(fields)?;
    if title.len() + data.len() + schema.len() > MAX_BYTES {
        return Err(TemplateError::TooLarge("Шаблон больше 1 МБ.".to_owned()));
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::Migration(error.to_string()))?
        .as_secs() as i64;
    let mut connection = connect(context)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = if let Some(id) = id {
        let exists: Option<i64> = transaction
            .query_row("select 1 from post_templates where id=?", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Err(TemplateError::NotFound);
        }
        let changed = transaction.execute(
            "update post_templates set title=?,payload=?,fields=?,updated_at=?,version=version+1 where id=? and version=?",
            params![title, data, schema, timestamp, id, version],
        ).map_err(|error| StoreError::from_sqlite(error, context))?;
        if changed != 1 {
            return Err(TemplateError::Conflict(
                "Шаблон изменился. Откройте его заново перед сохранением.".to_owned(),
            ));
        }
        id.to_owned()
    } else {
        let count: i64 =
            transaction.query_row("select count(*) from post_templates", [], |row| row.get(0))?;
        if count >= MAX_TEMPLATES {
            return Err(TemplateError::TooLarge(
                "Лимит шаблонов исчерпан. Удалите ненужные шаблоны.".to_owned(),
            ));
        }
        let id = Uuid::new_v4().simple().to_string()[..12].to_owned();
        transaction.execute(
            "insert into post_templates(id,title,payload,fields,created_at,updated_at) values(?,?,?,?,?,?)",
            params![id, title, data, schema, timestamp, timestamp],
        ).map_err(|error| StoreError::from_sqlite(error, context))?;
        id
    };
    let record = transaction.query_row("select * from post_templates where id=?", [&id], decode)?;
    transaction
        .commit()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(record)
}

pub fn delete(context: &AccountContext, id: &str, version: i64) -> Result<(), TemplateError> {
    let mut connection = connect(context)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = transaction
        .execute(
            "delete from post_templates where id=? and version=?",
            params![id, version],
        )
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    if changed != 1 {
        let exists: Option<i64> = transaction
            .query_row("select 1 from post_templates where id=?", [id], |row| {
                row.get(0)
            })
            .optional()?;
        return Err(if exists.is_some() {
            TemplateError::Conflict(
                "Шаблон изменился. Откройте его заново перед удалением.".to_owned(),
            )
        } else {
            TemplateError::NotFound
        });
    }
    transaction
        .commit()
        .map_err(|error| StoreError::from_sqlite(error, context))?;
    Ok(())
}

pub fn using(context: &AccountContext, reference: &str) -> Result<Vec<Value>, TemplateError> {
    let connection = connect(context)?;
    let mut statement = connection.prepare(
        "select id,title from post_templates where instr(payload,?)>0 or instr(fields,?)>0",
    )?;
    let reference = encode(&json!(reference))?;
    Ok(statement
        .query_map(params![reference, reference], decode)?
        .collect::<Result<Vec<_>, _>>()?)
}
