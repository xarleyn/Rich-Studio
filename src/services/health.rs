//! Read-only health checks; public readiness exposes only failing check names.

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::settings::Settings;
use crate::spec::source::Spec;
use crate::store::db;
use crate::{
    logs::{self, Logger},
    metrics::Metrics,
    services::loops::LoopStatus,
};

#[derive(Debug, Default)]
pub struct ReadyCache {
    pub(crate) value: tokio::sync::Mutex<Option<(std::time::Instant, Value)>>,
}

fn check(name: &str, status: &str, summary: &str, action: Option<&str>) -> Value {
    let mut value = json!({"name":name,"status":status,"summary":summary});
    if let Some(action) = action {
        value["action"] = json!(action);
    }
    value
}

pub fn report(
    settings: &Settings,
    context: &AccountContext,
    spec: &Spec,
    logger: &Logger,
    metrics: &Metrics,
    loop_status: &LoopStatus,
) -> Value {
    let mut checks = if context.root {
        vec![database(context)]
    } else {
        vec![]
    };
    checks.extend(queue_checks(context));
    checks.push(spec_source(settings, spec));
    if context.root {
        checks.push(
            if crate::runtime::static_dir().join("index.html").is_file() {
                check("editor", "ok", "ресурсы редактора доступны", None)
            } else {
                check(
                    "editor",
                    "fail",
                    "ресурсы редактора недоступны",
                    Some("проверьте STUDIO_STATIC_DIR"),
                )
            },
        );
        checks.push(telegram(settings, metrics));
        checks.push(loops(settings, metrics, loop_status));
        checks.push(disk(settings));
        checks.push(backups(settings, context, metrics));
        checks.push(storage(settings, context, logger));
        checks.push(log_problems(logger, metrics));
    }
    let status = if checks.iter().any(|item| item["status"] == "fail") {
        "fail"
    } else if checks.iter().any(|item| item["status"] == "warn") {
        "warn"
    } else {
        "ok"
    };
    let now = time::OffsetDateTime::now_utc();
    let timestamp = format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let problems: Vec<String> = checks
        .iter()
        .filter(|item| item["status"] != "ok")
        .map(|item| {
            format!(
                "{}: {}",
                item["name"].as_str().unwrap_or_default(),
                item["summary"].as_str().unwrap_or_default()
            )
        })
        .collect();
    logs::scrub_all(
        settings,
        &json!({"status":status,"version":env!("CARGO_PKG_VERSION"),"bot_api":spec.version(),"time":timestamp,
        "checked_at":now.unix_timestamp(),"problems":problems,"checks":checks,"metrics":if context.root{metrics.snapshot()}else{json!({})},"recent_problems":if context.root{metrics.recent().into_iter().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>()}else{vec![]}}),
    )
}

fn spec_source(settings: &Settings, spec: &Spec) -> Value {
    let mut result = if spec.version() == "?" {
        check(
            "spec",
            "fail",
            "спецификация Bot API недоступна",
            Some("переустановите ресурсы приложения"),
        )
    } else if let Some(error) = spec
        .last_check
        .as_ref()
        .and_then(|value| value["error"].as_str())
        && !error.is_empty()
        && (spec.loaded_at == 0.0
            || time::OffsetDateTime::now_utc().unix_timestamp() as f64 - spec.loaded_at
                > 7200.0 * settings.spec_refresh_hours)
    {
        check(
            "spec",
            "warn",
            &format!(
                "{}: свежую спецификацию не скачать ({error})",
                spec.version()
            ),
            Some("проверьте доступ к SPEC_URL; редактор работает на сохранённой копии"),
        )
    } else {
        check(
            "spec",
            "ok",
            &format!("{} ({})", spec.version(), spec.source),
            None,
        )
    };
    result["details"] =
        json!({"version":spec.version(),"source":spec.source,"last_check":spec.last_check});
    result
}

fn open(context: &AccountContext) -> Result<Connection, rusqlite::Error> {
    let connection = Connection::open_with_flags(
        context.database_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(connection)
}

fn queue_checks(context: &AccountContext) -> Vec<Value> {
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
                        || !super::in_flight::contains(
                            context,
                            row["id"].as_str().unwrap_or_default(),
                        )
                })
                .collect::<Vec<_>>();
            let mut result = check(
                name,
                if rows.is_empty() { "ok" } else { "warn" },
                if rows.is_empty() { good } else { bad },
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

fn telegram(settings: &Settings, metrics: &Metrics) -> Value {
    if settings.bot_token.is_empty() {
        return check(
            "telegram",
            "warn",
            "TG_BOT_TOKEN не задан: публиковать нельзя",
            Some("задайте токен в .env"),
        );
    }
    let last = metrics.telegram_last();
    if !last["error"].is_null() {
        let mut result = check(
            "telegram",
            if last["error"]["code"] == 409 {
                "fail"
            } else {
                "warn"
            },
            "последний вызов Telegram не удался",
            Some("проверьте подключение и права бота; при 409 остановите второй getUpdates"),
        );
        result["details"] = json!({"last_error":last["error"]});
        return result;
    }
    if !settings.updates_on {
        return check(
            "telegram",
            "ok",
            "приём событий выключен (TG_UPDATES=off)",
            None,
        );
    }
    if settings.background && !metrics.loops().contains_key("updates") {
        return check(
            "telegram",
            "warn",
            "приём событий ещё не подтвердил работу",
            Some("проверьте фоновые циклы и журнал"),
        );
    }
    check(
        "telegram",
        "ok",
        "сбоев подключения не зарегистрировано",
        None,
    )
}

fn loops(settings: &Settings, metrics: &Metrics, state: &LoopStatus) -> Value {
    if !settings.background {
        return check(
            "loops",
            "ok",
            "фоновые циклы выключены (STUDIO_BACKGROUND=off)",
            None,
        );
    }
    if state.role == "standby" {
        let mut result = check(
            "loops",
            "warn",
            "фоновые циклы ведёт другой процесс",
            Some("этот процесс продолжит после освобождения lease"),
        );
        result["details"] = json!(state);
        return result;
    }
    if state.stopping {
        return check("loops", "warn", "фоновые циклы останавливаются", None);
    }
    if state.error.is_some() {
        let mut result = check(
            "loops",
            "warn",
            "ошибка фонового цикла",
            Some("посмотрите журнал"),
        );
        result["details"] = json!(state);
        return result;
    }
    let mut expected = vec![
        ("scheduler", 10.0),
        ("bot_notifications", 10.0),
        ("housekeeping", 300.0),
        ("spec", 1800.0),
    ];
    if settings.backup_keep > 0 {
        expected.push(("backup", 3600.0));
    }
    if settings.updates_on {
        expected.push(("updates", 60.0));
    }
    let recorded = metrics.loops();
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as f64;
    let dead = expected
        .iter()
        .filter(|(name, interval)| {
            let limit = 3.0 * interval + 300.0;
            recorded
                .get(*name)
                .and_then(|entry| entry["beat_at"].as_f64())
                .map_or(metrics.uptime() as f64 > limit, |beat| now - beat > limit)
        })
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let mut result = if dead.is_empty() {
        check(
            "loops",
            "ok",
            if state.role == "leader" {
                "работают"
            } else {
                "ожидание запуска"
            },
            None,
        )
    } else {
        check(
            "loops",
            "fail",
            &format!("не отвечают: {}", dead.join(", ")),
            Some("перезапустите студию и посмотрите журнал"),
        )
    };
    result["details"] = json!({"loops":recorded,"lease":state});
    result
}

fn disk(settings: &Settings) -> Value {
    match fs2::available_space(&settings.data_dir) {
        Ok(free) => check(
            "disk",
            if free < 200 * 1024 * 1024 {
                "fail"
            } else if free < 1024 * 1024 * 1024 {
                "warn"
            } else {
                "ok"
            },
            &format!("свободно {:.1} ГБ", free as f64 / 1024.0f64.powi(3)),
            (free < 1024 * 1024 * 1024).then_some("освободите место для постов, базы и файлов"),
        ),
        Err(_) => check(
            "disk",
            "fail",
            "свободное место не удалось определить",
            Some("проверьте каталог данных"),
        ),
    }
}

fn backups(settings: &Settings, context: &AccountContext, metrics: &Metrics) -> Value {
    if settings.backup_keep == 0 {
        return check(
            "backup",
            "warn",
            "ежедневная копия базы выключена (BACKUP_KEEP=0)",
            Some("скачивайте zip: GET /api/export"),
        );
    }
    let Some(parent) = context.database_path().parent() else {
        return check("backup", "fail", "нет каталога базы", None);
    };
    let folder = parent.join("backups");
    let copies = match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with("studio-")
                    && entry.path().extension().is_some_and(|ext| ext == "db")
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(_) => return check("backup", "fail", "каталог резервных копий недоступен", None),
    };
    let newest = copies
        .iter()
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max();
    let stale = newest.map_or(metrics.uptime() > 7200, |time| {
        time.elapsed().is_ok_and(|age| age.as_secs() > 36 * 3600)
    });
    check(
        "backup",
        if settings.background && stale {
            "warn"
        } else {
            "ok"
        },
        &format!("резервных копий: {}", copies.len()),
        (settings.background && stale)
            .then_some("свежей копии нет больше суток: посмотрите журнал"),
    )
}

fn storage(settings: &Settings, context: &AccountContext, logger: &Logger) -> Value {
    let inspect = || -> Result<Value, Box<dyn std::error::Error>> {
        let connection = open(context)?;
        let mut refs = std::collections::HashSet::new();
        fn visit(value: &Value, refs: &mut std::collections::HashSet<String>) {
            match value {
                Value::String(text) => {
                    if let Some(name) = text.strip_prefix("upload:") {
                        refs.insert(name.to_owned());
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        visit(value, refs)
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        visit(value, refs)
                    }
                }
                _ => {}
            }
        }
        for query in [
            "select payload from drafts",
            "select payload from posts where status in ('scheduled','sending','missed','sent')",
            "select payload from post_templates",
            "select fields from post_templates",
        ] {
            let mut statement = connection.prepare(query)?;
            for text in statement.query_map([], |row| row.get::<_, String>(0))? {
                visit(&serde_json::from_str::<Value>(&text?)?, &mut refs);
            }
        }
        let (mut files, mut bytes, mut unused_files, mut unused_bytes) = (0u64, 0u64, 0u64, 0u64);
        if context.media_dir.exists() {
            for entry in std::fs::read_dir(&context.media_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry.file_name().to_string_lossy().starts_with('.')
                {
                    continue;
                }
                let size = entry.metadata()?.len();
                files += 1;
                bytes += size;
                if !refs.contains(entry.file_name().to_string_lossy().as_ref()) {
                    unused_files += 1;
                    unused_bytes += size;
                }
            }
        }
        let size =
            |path: &std::path::Path| std::fs::metadata(path).map_or(0, |metadata| metadata.len());
        let database_bytes = size(context.database_path())
            + size(&context.database_path().with_file_name("studio.db-wal"));
        let backups_bytes = context
            .database_path()
            .parent()
            .map(|parent| parent.join("backups"))
            .and_then(|path| std::fs::read_dir(path).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| metadata.len())
            .sum::<u64>();
        let versions: i64 =
            connection.query_row("select count(*) from post_versions", [], |row| row.get(0))?;
        let logs_bytes = logger.disk_bytes();
        let cap = settings.log_max_mb * 1024.0 * 1024.0 * (settings.log_files + 1) as f64;
        let mut result = if cap > 0.0 && logs_bytes as f64 > 1.5 * cap {
            check(
                "storage",
                "warn",
                "журнал больше заданного предела",
                Some("проверьте LOG_MAX_MB и LOG_FILES"),
            )
        } else if unused_bytes > 200 * 1024 * 1024 {
            check(
                "storage",
                "warn",
                "ненужные загрузки занимают больше 200 МБ",
                Some("удалите ненужные загрузки или задайте MEDIA_UNUSED_DAYS"),
            )
        } else {
            check(
                "storage",
                "ok",
                &format!(
                    "данные занимают {:.1} МБ",
                    (database_bytes + bytes + backups_bytes + logs_bytes) as f64 / 1024.0 / 1024.0
                ),
                None,
            )
        };
        result["details"] = json!({"database_bytes":database_bytes,"media":{"files":files,"bytes":bytes,"unused_files":unused_files,"unused_bytes":unused_bytes},"backups_bytes":backups_bytes,"logs_bytes":logs_bytes,"post_versions":versions});
        Ok(result)
    };
    inspect().unwrap_or_else(|_| {
        check(
            "storage",
            "fail",
            "объём данных не удалось определить",
            Some("проверьте базу и файлы"),
        )
    })
}

fn log_problems(logger: &Logger, metrics: &Metrics) -> Value {
    let errors = metrics
        .errors()
        .into_iter()
        .filter(|entry| {
            entry["at"].as_i64().unwrap_or(0)
                >= time::OffsetDateTime::now_utc().unix_timestamp() - 3600
        })
        .collect::<Vec<_>>();
    let mut result = if !errors.is_empty() || logger.dropped() > 0 || logger.write_errors() > 0 {
        check(
            "log",
            "warn",
            "в журнале есть ошибки или очередь переполнена",
            Some("GET /api/logs?level=ERROR покажет подробности"),
        )
    } else {
        check("log", "ok", "ошибок за час нет", None)
    };
    result["details"] = json!({"errors":errors.into_iter().rev().take(5).collect::<Vec<_>>(),"dropped":logger.dropped(),"write_errors":logger.write_errors(),"file":logger.current_file().map(|_|"studio.log")});
    result
}

fn database(context: &AccountContext) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[test]
    fn failed_spec_refresh_warns_only_when_the_saved_copy_is_stale() {
        let settings = Settings::from_map(&BTreeMap::new()).expect("settings");
        let mut spec = Spec::load_initial(&settings);
        spec.last_check = Some(json!({"error":"fixture offline"}));
        spec.loaded_at = 0.0;
        assert_eq!(spec_source(&settings, &spec)["status"], "warn");
        spec.loaded_at = time::OffsetDateTime::now_utc().unix_timestamp() as f64;
        assert_eq!(spec_source(&settings, &spec)["status"], "ok");
        spec.loaded_at -= 7200.0 * settings.spec_refresh_hours + 1.0;
        assert_eq!(spec_source(&settings, &spec)["status"], "warn");
        spec.last_check = None;
        assert_eq!(spec_source(&settings, &spec)["status"], "ok");
    }

    #[test]
    fn health_does_not_create_missing_database_or_disclose_paths() {
        let directory = TempDir::new().expect("temporary directory");
        let settings = Settings::from_map(&BTreeMap::from([(
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        )]))
        .expect("settings");
        let account = AccountContext::root(&settings);
        let metrics = std::sync::Arc::new(Metrics::default());
        let logger = Logger::new(&settings, metrics.clone());
        let report = report(
            &settings,
            &account,
            &Spec::load_initial(&settings),
            &logger,
            &metrics,
            &LoopStatus::default(),
        );
        assert_eq!(report["status"], "fail");
        assert!(
            !report
                .to_string()
                .contains(&directory.path().to_string_lossy().to_string())
        );
        assert!(!settings.root_database_path().exists());
    }
}
