use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rich_studio::AppState;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::services::media::{MediaError, MediaService, internal_name, public_address};
use rich_studio::settings::Settings;
use tempfile::TempDir;

fn state(dir: &TempDir, extra: &[(&str, &str)]) -> AppState {
    let mut values = BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]);
    for (name, value) in extra {
        values.insert((*name).to_owned(), (*value).to_owned());
    }
    AppState::new(Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}")))
}

#[test]
fn uploads_have_safe_unique_names_and_are_isolated_by_account()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let state = state(&dir, &[]);
    let owner = MediaService::new(state.account.clone(), 50.0, 500.0);
    let first = owner.upload("../../etc/passwd", b"x")?;
    assert!(
        !first["name"]
            .as_str()
            .unwrap_or_default()
            .contains(['/', '\\'])
    );
    assert!(!first["name"].as_str().unwrap_or_default().contains(".."));
    let one = owner.upload("same.jpg", b"1")?;
    let two = owner.upload("same.jpg", b"2")?;
    assert_ne!(one["name"], two["name"]);
    assert_eq!(one["kind"], "photo");
    assert_eq!(
        owner.download(one["name"].as_str().unwrap_or_default())?,
        b"1"
    );
    assert!(matches!(
        owner.download("../same.jpg"),
        Err(MediaError::NotFound)
    ));
    assert_eq!(owner.list()?.len(), 3);

    let account = MediaService::new(
        AccountContext::account(&state.settings, 7, Role::Drafts),
        50.0,
        500.0,
    );
    assert!(account.list()?.is_empty());
    assert!(matches!(
        account.download(one["name"].as_str().unwrap_or_default()),
        Err(MediaError::NotFound)
    ));
    let own = account.upload("own.png", b"png")?;
    assert!(
        owner
            .download(own["name"].as_str().unwrap_or_default())
            .is_err()
    );
    Ok(())
}

#[test]
fn simultaneous_uploads_keep_each_callers_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let media = MediaService::new(state(&dir, &[]).account, 1.0, 0.0);
    let ready = std::sync::Barrier::new(12);
    std::thread::scope(|threads| {
        let uploads: Vec<_> = (0..12)
            .map(|id| {
                let (media, ready) = (&media, &ready);
                threads.spawn(move || {
                    ready.wait();
                    media.upload("same.jpg", &[id]).map(|file| (id, file))
                })
            })
            .collect();
        for upload in uploads {
            let (id, file) = upload.join().map_err(|_| "upload thread failed")??;
            assert_eq!(
                media.download(file["name"].as_str().ok_or("filename")?)?,
                [id]
            );
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

#[test]
fn upload_and_account_limits_leave_no_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let state = state(&dir, &[("ACCOUNT_STORAGE_MB", "0.000002")]);
    let limited = MediaService::new(state.account.clone(), 0.000001, 500.0);
    assert!(matches!(
        limited.upload("big.png", b"xx"),
        Err(MediaError::TooLarge)
    ));
    assert!(limited.list()?.is_empty());

    let account = MediaService::new(
        AccountContext::account(&state.settings, 8, Role::Drafts),
        50.0,
        0.000001,
    );
    account.upload("one.png", b"1")?;
    assert!(matches!(
        account.upload("two.png", b"2"),
        Err(MediaError::Capacity)
    ));
    assert_eq!(account.list()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn public_address_policy_rejects_local_names_and_every_non_global_address() {
    for host in [
        "intranet",
        "localhost",
        "db.corp",
        "metadata.google.internal",
        "nas.local",
        "router.home.arpa",
        "x.lan.",
    ] {
        assert!(internal_name(host), "{host}");
    }
    assert!(!internal_name("files.example"));
    for address in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        assert!(public_address(&[address], "internal.example").is_err());
    }
    assert!(
        public_address(
            &[IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            "files.example"
        )
        .is_ok()
    );
}

#[tokio::test]
async fn http_upload_download_and_mcp_upload_use_the_same_storage()
-> Result<(), Box<dyn std::error::Error>> {
    use tower::ServiceExt as _;

    let dir = TempDir::new()?;
    let app = rich_studio::build_router(state(&dir, &[]));
    let boundary = "studio-test-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"page.html\"\r\nContent-Type: text/html\r\n\r\n<script>alert(1)</script>\r\n--{boundary}--\r\n"
    );
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .header("host", "localhost")
                .method("POST")
                .uri("/api/media")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let uploaded: serde_json::Value = serde_json::from_slice(&bytes)?;
    let name = uploaded["name"].as_str().ok_or("name")?;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .header("host", "localhost")
                .uri(format!("/media/{name}"))
                .body(axum::body::Body::empty())?,
        )
        .await?;
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/octet-stream")
    );
    assert!(
        response
            .headers()
            .get("content-disposition")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("attachment")
    );
    assert_eq!(
        response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok()),
        Some("sandbox; default-src 'none'")
    );

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .header("host", "localhost")
                .method("POST")
                .uri("/api/media/fetch")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"url":"http://127.0.0.1:8080/config"}"#,
                ))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

    let response = app.oneshot(
        axum::http::Request::builder().header("host", "localhost").method("POST").uri("/mcp")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"upload_media","arguments":{"filename":"dot.png","data_base64":"cG5n"}}}"#))?,
    ).await?;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let result: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(
        result["result"]["structuredContent"]["ref"],
        "upload:dot.png"
    );
    Ok(())
}
