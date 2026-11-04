use serde_json::{Value, json};

use crate::metrics::Metrics;
use crate::services::updates::Status as UpdatesStatus;
use crate::settings::Settings;

use super::report::check;

pub(super) fn telegram(settings: &Settings, metrics: &Metrics, updates: &UpdatesStatus) -> Value {
    if settings.bot_token.is_empty() {
        return check(
            "telegram",
            "warn",
            "TG_BOT_TOKEN не задан: публиковать нельзя",
            Some("задайте токен в .env"),
        );
    }
    let last = metrics.telegram_last();
    let error = &last["error"];
    let details = json!({"updates":updates,"last_error":error});
    if updates.state == "conflict" {
        let mut result = check(
            "telegram",
            "fail",
            &format!(
                "приём событий остановлен: {}",
                updates.error.as_deref().unwrap_or_default()
            ),
            Some(
                "этого бота читает другой процесс или стоит вебхук: остановите его или задайте TG_UPDATES=off",
            ),
        );
        result["details"] = details;
        return result;
    }
    if !error.is_null() {
        let mut result = check(
            "telegram",
            if error["code"] == 409 { "fail" } else { "warn" },
            &format!(
                "последний вызов {} не удался: {}",
                error["method"].as_str().unwrap_or("Telegram"),
                error["error"].as_str().unwrap_or_default()
            ),
            Some("проверьте связь с api.telegram.org и права бота (GET /api/check)"),
        );
        result["details"] = details;
        return result;
    }
    if updates.state == "error" {
        let mut result = check(
            "telegram",
            "warn",
            &format!(
                "приём событий: {}",
                updates.error.as_deref().unwrap_or_default()
            ),
            Some("проверьте связь с Telegram"),
        );
        result["details"] = details;
        return result;
    }
    if !settings.updates_on {
        let mut result = check(
            "telegram",
            "ok",
            "приём событий выключен (TG_UPDATES=off)",
            None,
        );
        result["details"] = details;
        return result;
    }
    if settings.background && !metrics.loops().contains_key("updates") {
        let mut result = check(
            "telegram",
            "warn",
            "приём событий ещё не подтвердил работу",
            Some("проверьте фоновые циклы и журнал"),
        );
        result["details"] = details;
        return result;
    }
    let mut result = check(
        "telegram",
        "ok",
        &format!(
            "приём событий: {}",
            if updates.state.is_empty() {
                "ok"
            } else {
                &updates.state
            }
        ),
        None,
    );
    result["details"] = details;
    result
}
