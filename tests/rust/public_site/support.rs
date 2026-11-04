use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use rich_studio::{AppState, build_router, settings::Settings};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(crate) fn settings_with(
    directory: &TempDir,
    overrides: &[(&str, &str)],
) -> Result<Settings, Box<dyn std::error::Error>> {
    let mut values = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("STUDIO_MODE".to_owned(), "server".to_owned()),
        ("STUDIO_PUBLIC".to_owned(), "on".to_owned()),
        (
            "STUDIO_URL".to_owned(),
            "https://app.example.com".to_owned(),
        ),
        (
            "STUDIO_LANDING_URL".to_owned(),
            "https://example.com".to_owned(),
        ),
        ("TG_BOT_USERNAME".to_owned(), "fixture_bot".to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:public-site-secret".to_owned()),
        (
            "STUDIO_API_KEY".to_owned(),
            "private-operator-key".to_owned(),
        ),
        ("ALLOWED_HOSTS".to_owned(), "mirror.example".to_owned()),
        ("TG_API_BASE".to_owned(), "http://127.0.0.1:1".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
    ]);
    for (key, value) in overrides {
        values.insert((*key).to_owned(), (*value).to_owned());
    }
    Ok(Settings::from_map(&values)?)
}

pub(crate) fn app(directory: &TempDir) -> Result<Router, Box<dyn std::error::Error>> {
    Ok(build_router(AppState::new(settings_with(directory, &[])?)))
}

pub(crate) fn app_with(
    directory: &TempDir,
    overrides: &[(&str, &str)],
) -> Result<(Router, Settings), Box<dyn std::error::Error>> {
    let settings = settings_with(directory, overrides)?;
    let router = build_router(AppState::new(settings.clone()));
    Ok((router, settings))
}

pub(crate) async fn get_with(
    app: Router,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    let mut builder = Request::builder().uri(path).header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app.oneshot(builder.body(Body::empty())?).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1_000_000).await?.to_vec();
    Ok((status, headers, body))
}

pub(crate) async fn get(
    app: Router,
    path: &str,
    host: &str,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    get_with(app, path, host, &[]).await
}

pub(crate) async fn head_with(
    app: Router,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    let mut builder = Request::builder()
        .method("HEAD")
        .uri(path)
        .header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app.oneshot(builder.body(Body::empty())?).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1_000_000).await?.to_vec();
    Ok((status, headers, body))
}

pub(crate) async fn head(
    app: Router,
    path: &str,
    host: &str,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    head_with(app, path, host, &[]).await
}

pub(crate) async fn post_json(
    app: Router,
    path: &str,
    host: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, HeaderMap, Value), Box<dyn std::error::Error>> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", host)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_string()))?)
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1_000_000).await?;
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, headers, body))
}

pub(crate) fn body_text(body: &[u8]) -> Result<&str, Box<dyn std::error::Error>> {
    Ok(std::str::from_utf8(body)?)
}

pub(crate) fn between<'a>(
    text: &'a str,
    start: &str,
    end: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    text.split_once(start)
        .and_then(|(_, rest)| rest.split_once(end))
        .map(|(value, _)| value)
        .ok_or_else(|| format!("missing {start}..{end}").into())
}

pub(crate) fn seo_pages() -> Result<BTreeMap<String, Value>, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string("app/bundled/public-site-seo.json")?;
    let data: Value = serde_json::from_str(&raw)?;
    let pages = data["pages"].as_object().ok_or("SEO pages")?;
    Ok(pages
        .iter()
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect())
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()
                .and_then(|pair| pair.trim().strip_prefix(&prefix))
                .map(str::to_owned)
        })
}

pub(crate) fn signed_callback(
    state: &str,
    uid: i64,
    token: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest as _, Sha256};
    let mut fields = BTreeMap::from([
        ("id", uid.to_string()),
        ("first_name", "User".to_owned()),
        ("auth_date", rich_studio::store::auth::now().to_string()),
    ]);
    let check = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&Sha256::digest(token.as_bytes()))?;
    mac.update(check.as_bytes());
    let hash = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fields.insert("hash", hash);
    fields.insert("state", state.to_owned());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    Ok(format!("/auth/telegram/callback?{query}"))
}

pub(crate) async fn login(
    app: &Router,
    host: &str,
    bot_token: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let (status, headers, body) = post_json(
        app.clone(),
        "/api/auth/telegram/start",
        host,
        json!({}),
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let auth_url = body["auth_url"].as_str().ok_or("auth_url")?;
    let state = url::Url::parse(auth_url)?
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("login state")?;
    let state_cookie = cookie(&headers, "rs_telegram_login").ok_or("login cookie")?;
    let callback = signed_callback(&state, 101, bot_token)?;
    let state_header = format!("rs_telegram_login={state_cookie}");
    let (status, headers, body) =
        get_with(app.clone(), &callback, host, &[("cookie", &state_header)]).await?;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let session = cookie(&headers, "rs_session").ok_or("session cookie")?;
    Ok(format!("rs_session={session}"))
}

pub(crate) async fn issue_token(
    app: &Router,
    host: &str,
    session_cookie: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let (status, headers, body) = post_json(
        app.clone(),
        "/api/account/tokens",
        host,
        json!({"name":"Claude laptop","level":"draft"}),
        &[("cookie", session_cookie)],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    Ok(body)
}

pub(crate) fn bearer(issued: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "Bearer {}",
        issued["secret"].as_str().ok_or("token secret")?
    ))
}

/// Minimal reader for the two-line TOML configuration excerpt in the setup
/// guide: `[table]` sections and quoted `key = "value"` pairs.
pub(crate) fn toml_section(
    text: &str,
    wanted: &str,
) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let mut section = "";
    let mut values = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name;
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("not a TOML pair: {line}"))?;
        if section != wanted {
            continue;
        }
        let value = value
            .trim()
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .ok_or_else(|| format!("not a quoted TOML string: {line}"))?;
        values.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(values)
}

/// Accepts any Telegram Bot API request and counts it, so a test can prove
/// that public documents and token checks caused no Telegram traffic.
pub(crate) async fn recording_telegram()
-> Result<(String, Arc<AtomicUsize>), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            let body = b"{\"ok\":true,\"result\":true}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, body).await;
        }
    });
    Ok((format!("http://{address}"), calls))
}
