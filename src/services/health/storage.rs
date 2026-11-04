use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::logs::Logger;
use crate::metrics::Metrics;
use crate::settings::Settings;

use super::database::open;
use super::report::check;

pub(super) fn disk(settings: &Settings) -> Value {
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

pub(super) fn backups(settings: &Settings, context: &AccountContext, metrics: &Metrics) -> Value {
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

pub(super) fn storage(settings: &Settings, context: &AccountContext, logger: &Logger) -> Value {
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

pub(super) fn log_problems(logger: &Logger, metrics: &Metrics) -> Value {
    let errors = metrics
        .errors()
        .into_iter()
        .filter(|entry| {
            entry["at"].as_i64().unwrap_or(0)
                >= time::OffsetDateTime::now_utc().unix_timestamp() - 3600
        })
        .collect::<Vec<_>>();
    let summary = if let Some(last) = errors.last() {
        format!(
            "ошибок в журнале за час: {}; последняя: {}",
            errors.len(),
            last["message"].as_str().unwrap_or_default()
        )
    } else if logger.dropped() > 0 || logger.write_errors() > 0 {
        "журнал не успевает записывать строки".to_owned()
    } else {
        "ошибок за час нет".to_owned()
    };
    let mut result = if !errors.is_empty() || logger.dropped() > 0 || logger.write_errors() > 0 {
        check(
            "log",
            "warn",
            &summary,
            Some("GET /api/logs?level=ERROR покажет подробности"),
        )
    } else {
        check("log", "ok", &summary, None)
    };
    result["details"] = json!({"errors":errors.into_iter().rev().take(5).collect::<Vec<_>>(),"dropped":logger.dropped(),"write_errors":logger.write_errors(),"file":logger.current_file().map(|_|"studio.log")});
    result
}
