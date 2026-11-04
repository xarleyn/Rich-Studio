use serde_json::{Value, json};

use crate::metrics::Metrics;
use crate::services::loops::LoopStatus;
use crate::settings::Settings;

use super::report::check;

pub(super) fn loops(settings: &Settings, metrics: &Metrics, state: &LoopStatus) -> Value {
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
