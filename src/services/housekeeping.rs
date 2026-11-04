//! Only expired service rows are always removed; user data retention is opt-in.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::media::MediaService;
use crate::{account_context::AccountContext, logs::Logger, settings::Settings, store::db::Store};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn cleanup(settings: &Settings, context: &AccountContext, now: i64) -> Result<Value, Error> {
    let connection = Store::connect(context)?;
    // Never free an unresolved publication merely because it is old.
    let purged = connection.execute(
        "delete from idempotency where created_at<? and status='done'",
        [now - 86400],
    )?;
    let mut done = json!({"idempotency_keys":purged});
    if settings.versions_keep > 0 {
        let deleted=connection.execute("delete from post_versions where id in (select id from (select id,row_number() over (partition by post_id order by id desc) as n from post_versions) where n>?)",[settings.versions_keep as i64])?;
        done["post_versions"] = json!(deleted);
    }
    if settings.media_unused_days > 0.0 {
        let service = MediaService::new(
            context.clone(),
            settings.max_upload_mb,
            settings.account_storage_mb,
        );
        let threshold = (now as f64 - settings.media_unused_days * 86400.0).max(0.0);
        let mut removed = Vec::new();
        for file in service.list()? {
            if file["used"].as_bool().unwrap_or(true) {
                continue;
            }
            let Some(name) = file["name"].as_str() else {
                continue;
            };
            let path = service.path_of(name)?;
            let modified = std::fs::metadata(path)?
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |duration| duration.as_secs_f64());
            if modified < threshold && service.used_by(name)?.is_empty() {
                service.delete(name, false)?;
                removed.push(name.to_owned());
            }
        }
        done["media"] = json!(removed);
    }
    Ok(done)
}

pub fn watch(report: &Value, previous: &mut BTreeMap<String, String>, logger: &Logger) {
    for check in report["checks"].as_array().into_iter().flatten() {
        let name = check["name"].as_str().unwrap_or_default();
        let status = check["status"].as_str().unwrap_or("fail");
        if previous.get(name).map_or("ok", String::as_str) != status {
            logger.event(match status{"ok"=>"INFO","warn"=>"WARNING",_=>"ERROR"},"rich-studio.health",check["summary"].as_str().unwrap_or_default(),json!({"event":"health.change","check":name,"status":status,"action":check["action"]}));
        }
        previous.insert(name.to_owned(), status.to_owned());
    }
}

pub async fn ping(settings: &Settings, status: &str, logger: &Logger) {
    if settings.deadman_url.is_empty() {
        return;
    }
    let url = if status == "fail" {
        format!("{}/fail", settings.deadman_url)
    } else {
        settings.deadman_url.clone()
    };
    let result = async {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .get(url)
            .send()
            .await?
            .error_for_status()?;
        Ok::<_, reqwest::Error>(())
    }
    .await;
    if result.is_err() {
        logger.event(
            "WARNING",
            "rich-studio.health",
            "dead man's switch not reached",
            json!({"event":"deadman.error"}),
        );
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
