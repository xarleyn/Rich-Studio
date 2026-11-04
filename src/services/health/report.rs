use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::logs::{self, Logger};
use crate::metrics::Metrics;
use crate::services::{loops::LoopStatus, updates::Status as UpdatesStatus};
use crate::settings::Settings;
use crate::spec::source::Spec;

use super::database::{database, queue_checks};
use super::loops::loops;
use super::storage::{backups, disk, log_problems, storage};
use super::telegram::telegram;

#[derive(Debug, Default)]
pub struct ReadyCache {
    pub(crate) value: tokio::sync::Mutex<Option<(std::time::Instant, Value)>>,
}

pub(super) fn check(name: &str, status: &str, summary: &str, action: Option<&str>) -> Value {
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
    updates_status: &UpdatesStatus,
) -> Value {
    let mut checks = if context.root {
        vec![database(context)]
    } else {
        vec![]
    };
    checks.extend(queue_checks(context, settings));
    checks.push(spec_source(settings, spec));
    if context.root {
        checks.push(editor());
        checks.push(telegram(settings, metrics, updates_status));
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

fn editor() -> Value {
    if crate::runtime::static_dir().join("index.html").is_file() {
        check("editor", "ok", "ресурсы редактора доступны", None)
    } else {
        check(
            "editor",
            "fail",
            "ресурсы редактора недоступны",
            Some("проверьте STUDIO_STATIC_DIR"),
        )
    }
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
            &UpdatesStatus::new(&settings),
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
