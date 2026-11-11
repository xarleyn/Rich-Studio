use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::settings::Settings;
use crate::store::db;

use super::report::check;

pub(super) fn open(context: &AccountContext) -> Result<Connection, rusqlite::Error> {
    let connection = Connection::open_with_flags(
        context.database_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(connection)
}

pub(super) fn queue_checks(context: &AccountContext, settings: &Settings) -> Vec<Value> {
    let inspect = || -> Result<Vec<Value>, rusqlite::Error> {
        let connection = open(context)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut checks = Vec::new();
        for (name, query, cutoff, good, bad, action, field) in [
            (
                "scheduler",
                "select id,chat_id,scheduled_at,retry_at,error from posts where status='scheduled' and coalesce(retry_at,scheduled_at)<? order by scheduled_at limit 10",
                now - 120,
                "очередь в порядке",
                "отложенные посты ждут дольше срока",
                "проверьте фоновые циклы и GET /api/posts",
                "posts",
            ),
            (
                "posts",
                "select id,chat_id,status,comment_status,error,coalesce(sent_at,scheduled_at,created_at) as at from posts where (status in ('failed','missed') and coalesce(sent_at,scheduled_at,created_at)>=?1) or status='sending' or (comment_status='failed' and coalesce(sent_at,created_at)>=?1) order by at desc limit 20",
                now - 86400,
                "за сутки без сбоев",
                "посты требуют внимания",
                "прочитайте error каждого поста и проверьте канал перед повтором",
                "posts",
            ),
            (
                "sends",
                "select key,scope,response,created_at from idempotency where status='uncertain' and created_at>=? order by created_at desc limit 10",
                now - 86400,
                "неясных отправок нет",
                "есть отправки с неизвестным исходом",
                "проверьте канал: прежний Idempotency-Key не повторяет отправку",
                "sends",
            ),
        ] {
            let mut statement = connection.prepare(query)?;
            let rows = statement
                .query_map([cutoff], db::decode_row)?
                .collect::<Result<Vec<_>, _>>()?;
            let rows = rows
                .into_iter()
                .map(Value::Object)
                .filter(|row| {
                    name != "posts"
                        || row["status"] != "sending"
                        || !crate::services::in_flight::contains(
                            context,
                            row["id"].as_str().unwrap_or_default(),
                        )
                })
                .collect::<Vec<_>>();
            let summary = if rows.is_empty() {
                good.to_owned()
            } else if name == "scheduler" && !settings.background {
                format!("{bad} (фоновые циклы выключены)")
            } else {
                bad.to_owned()
            };
            let mut result = check(
                name,
                if rows.is_empty() { "ok" } else { "warn" },
                &summary,
                (!rows.is_empty()).then_some(action),
            );
            if !rows.is_empty() {
                result["details"] = json!({field:rows});
            }
            checks.push(result);
        }
        Ok(checks)
    };
    inspect().unwrap_or_else(|_| {
        ["scheduler", "posts", "sends"]
            .iter()
            .map(|name| {
                check(
                    name,
                    "fail",
                    "состояние базы не удалось прочитать",
                    Some("проверьте базу и доступ к каталогу данных"),
                )
            })
            .collect()
    })
}

pub(super) fn database(context: &AccountContext) -> Value {
    let inspect = || -> Result<Value, rusqlite::Error> {
        let connection = Connection::open_with_flags(
            context.database_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        let version: i64 = connection.query_row("pragma user_version", [], |row| row.get(0))?;
        let expected = db::embedded_migrations().len() as i64;
        if version != expected {
            return Ok(check(
                "database",
                "fail",
                &format!("схема базы {version}, код ждёт {expected}"),
                Some("перезапустите студию: миграции идут при старте"),
            ));
        }
        let mut counts = serde_json::Map::new();
        let mut statement =
            connection.prepare("select status, count(*) from posts group by status")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (status, count) = row?;
            counts.insert(status, json!(count));
        }
        let mut result = check("database", "ok", "база доступна, схема актуальна", None);
        result["details"] = json!({"posts":counts});
        Ok(result)
    };
    match inspect() {
        Ok(result) => result,
        Err(_) => check(
            "database",
            "fail",
            "база недоступна",
            Some("проверьте каталог данных, права и свободное место"),
        ),
    }
}
