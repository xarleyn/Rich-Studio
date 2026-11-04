use std::sync::Arc;

use axum::http::StatusCode;
use rich_studio::{build_router, logs::Logger, metrics::Metrics, services::housekeeping};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, check, initialized, mcp_json, mcp_lines, report, request_with_headers, settings,
};

#[test]
fn log_rotation_keeps_the_cap_and_logging_can_be_turned_off() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.log_max_mb = 0.01;
    settings.log_files = 2;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    for index in 0..1000 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let logs = directory.path().join("logs");
    let mut names = std::fs::read_dir(&logs)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    assert_eq!(names, ["studio.log", "studio.log.1", "studio.log.2"]);
    let cap = (3.0 * 0.01 * 1024.0 * 1024.0) as u64;
    assert!(logger.disk_bytes() <= cap + 1024, "{}", logger.disk_bytes());
    let newest = logger.read("INFO", 1, None, "", "")?;
    assert!(
        newest[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 999")
    );
    settings.log_max_mb = 0.0;
    let silent = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(silent.current_file().is_none());
    assert!(silent.read("INFO", 10, None, "", "")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn logs_endpoint_filters_by_level_logger_text_and_time() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    state
        .logs
        .event("INFO", "rich-studio.scheduler", "tick", json!({}));
    state.logs.event(
        "ERROR",
        "rich-studio.scheduler",
        "scheduler broke",
        json!({}),
    );
    state
        .logs
        .event("WARNING", "rich-studio.updates", "late click", json!({}));
    let (status, warning, _) =
        request_with_headers(app.clone(), "GET", "/api/logs?level=WARNING", None, &[]).await?;
    assert_eq!(status, StatusCode::OK, "{warning}");
    assert!(
        warning["file"]
            .as_str()
            .unwrap_or_default()
            .ends_with("studio.log")
    );
    assert_eq!(
        warning["lines"]
            .as_array()
            .ok_or("lines")?
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["scheduler broke", "late click"]
    );
    let (_, by_logger, _) = request_with_headers(
        app.clone(),
        "GET",
        "/api/logs?logger=rich-studio.scheduler",
        None,
        &[],
    )
    .await?;
    assert_eq!(
        by_logger["lines"]
            .as_array()
            .ok_or("lines")?
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["tick", "scheduler broke"]
    );
    let (_, by_text, _) =
        request_with_headers(app.clone(), "GET", "/api/logs?q=LATE", None, &[]).await?;
    assert_eq!(by_text["lines"][0]["msg"], "late click");
    let future = housekeeping::now() + 60;
    let (_, empty, _) = request_with_headers(
        app.clone(),
        "GET",
        &format!("/api/logs?since={future}"),
        None,
        &[],
    )
    .await?;
    assert_eq!(empty["lines"], json!([]));
    let (status, _, _) =
        request_with_headers(app, "GET", "/api/logs?level=LOUD", None, &[]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[test]
fn timestamps_are_utc_and_since_filters_by_them() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    logger.event("INFO", "rich-studio.test", "now", json!({}));
    let line = logger
        .read("INFO", 100, None, "", "")?
        .pop()
        .ok_or("line")?;
    let timestamp = line["ts"].as_str().ok_or("ts")?;
    assert_eq!(timestamp.len(), 24, "{timestamp}");
    assert!(timestamp.ends_with('Z'));
    let now = time::OffsetDateTime::now_utc();
    let prefix = format!(
        "{}-{:02}-{:02}T{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    );
    assert!(timestamp.starts_with(&prefix), "{timestamp} {prefix}");
    let recent = logger.read(
        "INFO",
        100,
        Some(now.unix_timestamp() as f64 - 60.0),
        "",
        "",
    )?;
    assert_eq!(recent.last().ok_or("line")?["msg"], "now");
    let future = logger.read(
        "INFO",
        100,
        Some(now.unix_timestamp() as f64 + 3600.0),
        "",
        "",
    )?;
    assert!(future.is_empty());
    Ok(())
}

#[tokio::test]
async fn mcp_health_and_logs_match_http() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    state
        .logs
        .event("WARNING", "rich-studio.updates", "late click", json!({}));
    let app = build_router(state.clone());
    let health = mcp_json(app.clone(), "studio_health", json!({})).await?;
    assert!(
        health["checks"]
            .as_array()
            .ok_or("checks")?
            .iter()
            .any(|check| check["name"] == "database")
    );
    let lines = mcp_lines(app, "studio_logs", json!({"level":"WARNING","limit":50})).await?;
    assert_eq!(
        lines
            .iter()
            .map(|line| line["msg"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["late click"]
    );
    Ok(())
}

#[tokio::test]
async fn access_log_path_is_short_and_printable() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    let app = build_router(state.clone());
    let path = format!("/x%1B[31mred{}", "y".repeat(500));
    let _ = request_with_headers(app, "GET", &path, None, &[]).await?;
    let lines = state
        .logs
        .read("INFO", 1000, None, "rich-studio.http", "")?;
    let http = lines
        .iter()
        .find(|line| line["event"] == "http")
        .ok_or("access line")?;
    let logged = http["path"].as_str().ok_or("path")?;
    assert!(!logged.contains('\u{1b}'), "{logged}");
    assert!(logged.len() <= 210, "{}", logged.len());
    assert!(logged.starts_with("/x%1B"), "{logged}");
    Ok(())
}

#[test]
fn zero_log_files_still_rotate_and_stray_copies_are_ignored() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    std::fs::create_dir_all(settings.data_dir.join("logs"))?;
    std::fs::write(settings.data_dir.join("logs/studio.log.old"), "not ours")?;
    std::fs::write(settings.data_dir.join("logs/studio.log.7"), "{}\n")?;
    settings.log_max_mb = 0.01;
    settings.log_files = 0;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(!settings.data_dir.join("logs/studio.log.7").exists());
    assert!(settings.data_dir.join("logs/studio.log.old").exists());
    for index in 0..1000 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let cap = (2.0 * 0.01 * 1024.0 * 1024.0) as u64;
    assert!(logger.disk_bytes() <= cap + 1024, "{}", logger.disk_bytes());
    assert!(
        logger.read("INFO", 1, None, "", "")?[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 999")
    );
    Ok(())
}

#[test]
fn a_rotated_away_copy_is_skipped_while_reading_and_counting() -> TestResult {
    let directory = TempDir::new()?;
    let mut settings = settings(&directory)?;
    settings.log_max_mb = 0.01;
    settings.log_files = 1;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    for index in 0..500 {
        logger.event(
            "INFO",
            "rich-studio.test",
            &format!("line {index} {}", "x".repeat(100)),
            json!({}),
        );
    }
    logger.flush()?;
    let current = logger.current_file().ok_or("log file")?.to_path_buf();
    let rotated = current.with_file_name("studio.log.1");
    assert!(rotated.exists());
    std::fs::remove_file(&rotated)?;
    assert_eq!(logger.disk_bytes(), std::fs::metadata(&current)?.len());
    assert!(
        logger.read("INFO", 1, None, "", "")?[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .starts_with("line 499")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn log_file_is_readable_only_by_its_owner() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    logger.event("INFO", "rich-studio.test", "x", json!({}));
    logger.flush()?;
    let mode = std::fs::metadata(logger.current_file().ok_or("log file")?)?
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    Ok(())
}

#[tokio::test]
async fn since_out_of_range_is_refused_not_a_server_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings.clone())?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));
    assert!(
        logger
            .read("INFO", 10, Some(f64::INFINITY), "", "")?
            .is_empty()
    );
    let app = build_router(state);
    let (status, _, _) =
        request_with_headers(app, "GET", "/api/logs?since=1e300", None, &[]).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn watchers_own_error_line_does_not_count_as_a_log_error() -> TestResult {
    let directory = TempDir::new()?;
    let settings = settings(&directory)?;
    let state = initialized(settings)?;
    state.logs.event(
        "ERROR",
        "rich-studio.health",
        "x: ok → fail",
        json!({"event":"health.change"}),
    );
    assert_eq!(check(&report(&state).await, "log")["status"], "ok");
    state
        .logs
        .event("ERROR", "rich-studio.test", "real", json!({}));
    for _ in 0..40 {
        state
            .logs
            .event("WARNING", "rich-studio.test", "noise", json!({}));
    }
    let report = report(&state).await;
    assert_eq!(check(&report, "log")["status"], "warn");
    assert!(
        check(&report, "log")["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("real")
    );
    Ok(())
}
