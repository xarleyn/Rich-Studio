use axum::http::StatusCode;
use rich_studio::services::media::public_address;
use serde_json::json;
use std::collections::BTreeSet;
use std::net::IpAddr;
use tempfile::TempDir;

use crate::support::{
    TestResult, call_fail, call_list, call_ok, file_server, media_names, request, studio_with_fake,
    upload_file,
};

#[tokio::test]
async fn mcp_media_upload_accepts_base64_and_fetched_urls() -> TestResult {
    use base64::Engine as _;

    let dir = TempDir::new()?;
    let (base, _tg, state, app) = studio_with_fake(&dir, &[("MEDIA_FETCH_PRIVATE", "on")]).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG fake");
    let file = call_ok(
        app.clone(),
        "upload_media",
        json!({"data_base64":encoded,"filename":"обложка.png"}),
    )
    .await;
    assert!(
        file["ref"]
            .as_str()
            .unwrap_or_default()
            .starts_with("upload:")
    );
    assert_eq!(file["kind"], "photo");
    let calls = call_ok(
        app.clone(),
        "upload_media",
        json!({"url":format!("{base}/log"),"filename":"calls.json"}),
    )
    .await;
    assert_eq!(calls["ref"], "upload:calls.json");
    let saved = std::fs::read(state.account.media_dir.join("calls.json"))?;
    assert!(saved.starts_with(b"["), "{saved:?}");
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":"file:///etc/passwd"}),
    )
    .await;
    assert!(text.contains("http"), "{text}");
    let text = call_fail(app.clone(), "upload_media", json!({"data_base64":"AAAA"})).await;
    assert!(text.contains("filename"), "{text}");
    let names = call_list(app, "list_media", json!({}))
        .await
        .into_iter()
        .filter_map(|file| file["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    assert!(names.contains("calls.json"));
    assert_eq!(names.len(), 2, "{names:?}");
    Ok(())
}

#[tokio::test]
async fn media_fetch_refuses_private_addresses_everywhere() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, state, app) = studio_with_fake(&dir, &[]).await;
    for host in [
        "127.0.0.1",
        "10.1.2.3",
        "192.168.0.10",
        "169.254.169.254",
        "::1",
        "::ffff:127.0.0.1",
        "0.0.0.0",
        "100.64.0.1",
    ] {
        let address: IpAddr = host
            .parse()
            .unwrap_or_else(|error| panic!("{host}: {error}"));
        assert!(public_address(&[address], host).is_err(), "accepted {host}");
    }
    let public: IpAddr = "1.1.1.1".parse()?;
    assert!(public_address(&[public], "1.1.1.1").is_ok());
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":"http://127.0.0.1:8080/api/config"}),
    )
    .await;
    assert!(text.contains("внутренний"), "{text}");
    let (status, result) = request(
        app,
        "POST",
        "/api/media/fetch",
        Some(json!({"url":"http://localhost:8080/"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    assert!(media_names(&state).is_empty(), "{:?}", media_names(&state));
    assert!(tg.all_calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn media_fetch_checks_redirects_and_leaves_no_partial_file() -> TestResult {
    let dir = TempDir::new()?;
    let files = file_server().await;
    let (_base, _tg, state, app) = studio_with_fake(&dir, &[("MEDIA_FETCH_PRIVATE", "on")]).await;
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":format!("{files}/to-inside")}),
    )
    .await;
    assert!(text.contains("внутренний"), "{text}");
    let text = call_fail(
        app.clone(),
        "upload_media",
        json!({"url":format!("{files}/hop")}),
    )
    .await;
    assert!(text.contains("перенаправлений"), "{text}");
    let (_base2, _tg2, state2, app2) = studio_with_fake(
        &dir,
        &[("MEDIA_FETCH_PRIVATE", "on"), ("MAX_UPLOAD_MB", "1")],
    )
    .await;
    let text = call_fail(
        app2,
        "upload_media",
        json!({"url":format!("{files}/big.jpg")}),
    )
    .await;
    assert!(text.contains("больше"), "{text}");
    assert!(media_names(&state).is_empty(), "{:?}", media_names(&state));
    assert!(
        media_names(&state2).is_empty(),
        "{:?}",
        media_names(&state2)
    );
    Ok(())
}

#[tokio::test]
async fn media_list_and_studio_status() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, _tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (status, uploaded) = upload_file(app.clone(), "cat.png", b"\x89PNG", "image/png").await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let files = call_list(app.clone(), "list_media", json!({})).await;
    assert_eq!(files[0]["ref"], "upload:cat.png");
    assert_eq!(files[0]["used"], false);
    let studio = call_ok(app, "studio_status", json!({})).await;
    assert_eq!(studio["token_set"], true);
    assert_eq!(studio["updates"]["state"], "off");
    assert_eq!(studio["schedule_late_min"].as_f64(), Some(60.0));
    Ok(())
}
