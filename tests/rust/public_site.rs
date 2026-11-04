use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rich_studio::account_context::AccountContext;
use rich_studio::store::{drafts, posts};
use rich_studio::{AppState, api::static_assets, build_router, public_site, settings::Settings};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn settings_with(
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

fn app(directory: &TempDir) -> Result<Router, Box<dyn std::error::Error>> {
    Ok(build_router(AppState::new(settings_with(directory, &[])?)))
}

fn app_with(
    directory: &TempDir,
    overrides: &[(&str, &str)],
) -> Result<(Router, Settings), Box<dyn std::error::Error>> {
    let settings = settings_with(directory, overrides)?;
    let router = build_router(AppState::new(settings.clone()));
    Ok((router, settings))
}

async fn get_with(
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

async fn get(
    app: Router,
    path: &str,
    host: &str,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    get_with(app, path, host, &[]).await
}

async fn head_with(
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

async fn head(
    app: Router,
    path: &str,
    host: &str,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    head_with(app, path, host, &[]).await
}

async fn post_json(
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

fn body_text(body: &[u8]) -> Result<&str, Box<dyn std::error::Error>> {
    Ok(std::str::from_utf8(body)?)
}

fn between<'a>(
    text: &'a str,
    start: &str,
    end: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    text.split_once(start)
        .and_then(|(_, rest)| rest.split_once(end))
        .map(|(value, _)| value)
        .ok_or_else(|| format!("missing {start}..{end}").into())
}

fn seo_pages() -> Result<BTreeMap<String, Value>, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string("app/bundled/public-site-seo.json")?;
    let data: Value = serde_json::from_str(&raw)?;
    let pages = data["pages"].as_object().ok_or("SEO pages")?;
    Ok(pages
        .iter()
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect())
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
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

fn signed_callback(
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

async fn login(
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

async fn issue_token(
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

fn bearer(issued: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "Bearer {}",
        issued["secret"].as_str().ok_or("token secret")?
    ))
}

/// Minimal reader for the two-line TOML configuration excerpt in the setup
/// guide: `[table]` sections and quoted `key = "value"` pairs.
fn toml_section(
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
async fn recording_telegram() -> Result<(String, Arc<AtomicUsize>), Box<dyn std::error::Error>> {
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

#[tokio::test]
async fn public_documents_keep_exact_routes_head_metadata_and_configured_destinations() -> TestResult
{
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let placeholders = regex::Regex::new(r"__[A-Z_]+__")?;
    for host in ["example.com", "app.example.com"] {
        for path in public_site::AGENT_PATHS {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(*path)
                        .header("host", host)
                        .header("x-forwarded-host", "attacker.invalid")
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::OK, "{host}{path}");
            assert_eq!(response.headers()["cache-control"], "public, max-age=300");
            assert!(
                response.headers()["link"].to_str()?.contains("describedby"),
                "{host}{path}"
            );
            assert!(!response.headers().contains_key("set-cookie"));
            let content_type = response.headers()["content-type"].to_str()?.to_owned();
            assert!(
                content_type.starts_with(if path.ends_with(".json") {
                    "application/json"
                } else {
                    "text/"
                }),
                "{host}{path}: {content_type}"
            );
            let body =
                String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
            assert!(!body.starts_with('\u{feff}'), "{host}{path}: BOM");
            assert!(!placeholders.is_match(&body), "{host}{path}: placeholder");
            assert!(
                !body.contains("attacker.invalid")
                    && !body.contains("public-site-secret")
                    && !body.contains("private-operator-key")
            );
            assert!(
                !body.contains("__APP_URL__")
                    && !body.contains("__SITE_URL__")
                    && !body.contains("__TOKEN_STEPS__")
            );
            let head = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("HEAD")
                        .uri(*path)
                        .header("host", host)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(head.status(), StatusCode::OK);
            assert_eq!(head.headers()["content-type"].to_str()?, content_type);
            assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents.json")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    let catalog: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await?)?;
    assert_eq!(catalog["mcp"][0]["url"], "https://app.example.com/mcp");
    for path in [
        "/skills/other/SKILL.md",
        "/skills/richpost/AGENTS.md",
        "/agents.md/extra",
        "/features/unknown/",
        "/bundled/agent-docs/setup.md",
        "/skills/richpost/../../settings.py",
        "/skills/richpost/%2e%2e/%2e%2e/settings.py",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let protected = app
        .oneshot(
            Request::builder()
                .uri("/api/drafts")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn marketing_templates_and_sitemap_preserve_languages_and_fixed_inventory() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::page_paths() {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let text = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
        assert!(text.contains("rel=\"describedby\" href=\"/llms.txt\""));
        assert!(!text.contains("__APP_URL__") && !text.contains("__LANDING_URL__"));
        assert!(text.contains(&format!("https://example.com{path}")));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sitemap.xml")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    let text = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
    assert_eq!(public_site::page_paths().len(), 12);
    assert_eq!(text.matches("<url>").count(), 12);
    assert_eq!(text.matches("<lastmod>").count(), 12);
    assert_eq!(text.matches("<xhtml:link").count(), 36);
    assert!(text.contains("xmlns:xhtml=\"http://www.w3.org/1999/xhtml\""));
    for path in public_site::page_paths() {
        assert!(text.contains(&format!("<loc>https://example.com{path}</loc>")));
    }
    assert!(!text.contains("/api/") && !text.contains("/landing") && !text.contains("/agents.md"));
    let entries = text
        .split("<url>")
        .skip(1)
        .filter_map(|chunk| chunk.split("</url>").next())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), public_site::page_paths().len());
    let date_format = time::format_description::parse_borrowed::<3>("[year]-[month]-[day]")?;
    let today = time::OffsetDateTime::now_utc().date();
    let hreflang = regex::Regex::new(r#"hreflang="([^"]+)""#)?;
    let mut locations = BTreeSet::new();
    for entry in entries {
        let location = between(entry, "<loc>", "</loc>")?;
        locations.insert(location.to_owned());
        let lastmod = between(entry, "<lastmod>", "</lastmod>")?;
        let modified = time::Date::parse(lastmod, &date_format)?;
        assert!(modified <= today, "{lastmod}");
        let alternates = hreflang
            .captures_iter(entry)
            .filter_map(|captures| captures.get(1))
            .map(|matched| matched.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            alternates,
            BTreeSet::from(["ru", "en", "x-default"]),
            "{location}"
        );
    }
    let expected = public_site::page_paths()
        .into_iter()
        .map(|path| format!("https://example.com{path}"))
        .collect::<BTreeSet<_>>();
    assert_eq!(locations, expected);
    let pages = seo_pages()?;
    assert_eq!(
        pages.keys().cloned().collect::<BTreeSet<_>>(),
        public_site::page_paths()
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    for location in &locations {
        let path = location
            .strip_prefix("https://example.com")
            .ok_or("sitemap host")?;
        let (status, headers, body) = get(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(!headers.contains_key("x-robots-tag"), "{path}");
        let html = body_text(&body)?;
        assert!(
            html.contains(&format!("<link rel=\"canonical\" href=\"{location}\">")),
            "{path}"
        );
        assert_eq!(html.matches("<h1").count(), 1, "{path}");
        assert!(
            html.contains("hreflang=\"ru\"") && html.contains("hreflang=\"en\""),
            "{path}"
        );
        assert!(!html.contains("__APP_URL__"), "{path}");
        assert!(
            headers["content-security-policy"]
                .to_str()?
                .contains("script-src 'self'"),
            "{path}"
        );
        let (head_status, head_headers, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(head_status, StatusCode::OK, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
        assert_eq!(
            head_headers["content-type"], headers["content-type"],
            "{path}"
        );
        assert_eq!(
            head_headers["content-length"], headers["content-length"],
            "{path}"
        );
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/sitemap.xml")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn public_head_and_canonical_slashes_keep_methods_and_queries() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::page_paths() {
        let head = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(&path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(head.status(), StatusCode::OK, "HEAD {path}");
        assert!(
            head.headers()["content-type"]
                .to_str()?
                .starts_with("text/html")
        );
        assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
        if path == "/" {
            continue;
        }
        for query in ["utm_source=a%20b&lang=en", "utm_source=telegram&x=1"] {
            for method in ["GET", "HEAD"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(format!("{}?{query}", path.trim_end_matches('/')))
                            .header("host", "example.com")
                            .body(Body::empty())?,
                    )
                    .await?;
                assert_eq!(
                    response.status(),
                    StatusCode::PERMANENT_REDIRECT,
                    "{method} {path}?{query}"
                );
                assert_eq!(
                    response.headers()["location"],
                    format!("{path}?{query}"),
                    "{method} {path}?{query}"
                );
            }
        }
    }
    for path in ["/robots.txt", "/sitemap.xml", "/favicon.ico"] {
        let head = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(path)
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(head.status(), StatusCode::OK, "HEAD {path}");
        assert!(to_bytes(head.into_body(), 1_000_000).await?.is_empty());
    }
    let unknown = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/features/unknown")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert!(!unknown.headers().contains_key("location"));
    let write = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/en")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(write.status(), StatusCode::FORBIDDEN);
    assert!(!write.headers().contains_key("location"));
    Ok(())
}

#[tokio::test]
async fn marketing_missing_pages_are_html_without_changing_api_authorization() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in [
        "/missing-page",
        "/mcp-missing",
        "/docs-missing",
        "/features/no-such-guide/",
        "/en/features/no-such-guide/",
        "/missing",
        "/en/missing",
    ] {
        let (status, headers, body) = get(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!headers.contains_key("location"), "{path}");
        assert!(
            headers["content-type"].to_str()?.starts_with("text/html"),
            "{path}"
        );
        assert_eq!(headers["x-robots-tag"], "noindex, nofollow", "{path}");
        assert!(headers.contains_key("content-security-policy"), "{path}");
        let text = body_text(&body)?;
        assert!(text.contains("RichPost"), "{path}");
        assert!(
            !text.contains("rel=\"canonical\"") && !text.contains("__"),
            "{path}"
        );
        assert!(
            text.contains(if path.starts_with("/en/") {
                "lang=\"en\""
            } else {
                "lang=\"ru\""
            }),
            "{path}"
        );
        assert!(text.contains("<a") && text.contains("/features/"), "{path}");
        assert_eq!(text.matches("<h1").count(), 1, "{path}");
        let (head_status, _, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(head_status, StatusCode::NOT_FOUND, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
    }
    let (status, _, body) = get(app.clone(), "/en/no-such-page", "example.com").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body_text(&body)?.starts_with("<!doctype html>\n<html lang=\"en\">"),
        "English fallback keeps its document language"
    );
    let unauthorized = app
        .oneshot(
            Request::builder()
                .uri("/api/unknown")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(
        unauthorized.headers()["content-type"]
            .to_str()?
            .starts_with("application/json")
    );
    Ok(())
}

#[tokio::test]
async fn public_metadata_is_parseable_hashed_and_not_authorized_by_request_headers() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let pages = seo_pages()?;
    let placeholders = regex::Regex::new(r"__[A-Z_]+__")?;
    let social_preview = regex::Regex::new(
        r#"<meta property="og:image" content="https://example\.com/static/images/social-preview\.png\?v=[a-f0-9]{12}">"#,
    )?;
    for path in public_site::page_paths() {
        let entry = pages.get(&path).ok_or("SEO entry")?;
        let title = entry["title"].as_str().ok_or("SEO title")?;
        let description = entry["description"].as_str().ok_or("SEO description")?;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("host", "example.com")
                    .header("x-csp-hash", "attacker")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        let csp = response.headers()["content-security-policy"]
            .to_str()?
            .to_owned();
        assert!(!csp.contains("attacker"));
        let script_policy = csp
            .split(';')
            .find(|part| part.trim_start().starts_with("script-src"))
            .ok_or("missing script-src")?;
        assert!(!script_policy.contains("unsafe-inline") && !script_policy.contains("unsafe-eval"));
        let html = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await?.to_vec())?;
        assert!(html.contains(&format!("<title>{title}</title>")), "{path}");
        assert!(
            html.contains(&format!(
                "<meta name=\"description\" content=\"{description}\">"
            )),
            "{path}"
        );
        assert_eq!(
            html.matches("<script type=\"application/ld+json\">")
                .count(),
            1,
            "{path}"
        );
        let json = html
            .split_once("<script type=\"application/ld+json\">")
            .and_then(|(_, tail)| tail.split_once("</script>"))
            .map(|(json, _)| json)
            .ok_or("missing structured data")?;
        let hash = STANDARD.encode(Sha256::digest(json.as_bytes()));
        assert!(script_policy.contains(&format!("'sha256-{hash}'")));
        let data: Value = serde_json::from_str(json)?;
        assert_eq!(data["@context"], "https://schema.org");
        let graph = data["@graph"].as_array().ok_or("missing schema graph")?;
        assert!(!graph.is_empty());
        assert!(!placeholders.is_match(&html), "{path}");
        assert!(social_preview.is_match(&html), "{path}");
        assert!(!html.contains("__STRUCTURED_DATA__") && !html.contains("__SITE_URL__"));
        assert!(html.contains("property=\"og:image\""));
        assert!(html.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(html.contains("/static/images/social-preview.png?v="));
        if path.contains("/features/") {
            assert!(
                graph.iter().any(|item| item["@type"] == "BreadcrumbList"),
                "{path}"
            );
        }
    }
    let app_page = app
        .oneshot(
            Request::builder()
                .uri("/en/features/")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(app_page.status(), StatusCode::OK);
    assert_eq!(app_page.headers()["x-robots-tag"], "noindex, nofollow");
    Ok(())
}

#[tokio::test]
async fn static_cache_requires_actual_file_fingerprint_and_respects_brotli_quality() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let css = std::fs::read("app/static/landing.css")?;
    let fingerprint = rich_studio::hex::encode(&Sha256::digest(&css))[..12].to_owned();
    for (query, immutable) in [
        (String::new(), false),
        ("?v=old".to_owned(), false),
        ("?v=000000000000".to_owned(), false),
        (format!("?v={fingerprint}&extra=1"), false),
        (format!("?v={fingerprint}&v={fingerprint}"), false),
        (format!("?v={fingerprint}"), true),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/static/landing.css{query}"))
                    .header("host", "example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["cache-control"],
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=0, must-revalidate"
            }
        );
        assert_eq!(
            to_bytes(response.into_body(), 1_000_000).await?.as_ref(),
            css
        );
    }
    for (accept, encoded) in [
        ("br", true),
        ("gzip, br", true),
        ("br;q=0, identity;q=1", false),
        ("br;q=0, *;q=1", false),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/landing.css")
                    .header("host", "example.com")
                    .header("accept-encoding", accept)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["vary"], "Accept-Encoding");
        assert!(
            response.headers()["content-type"]
                .to_str()?
                .starts_with("text/css"),
            "{accept}"
        );
        assert_eq!(response.headers().contains_key("content-encoding"), encoded);
        if encoded {
            assert_eq!(response.headers()["content-encoding"], "br");
        }
        let expected = if encoded {
            std::fs::read("app/static/landing.css.br")?
        } else {
            css.clone()
        };
        assert_eq!(
            to_bytes(response.into_body(), 1_000_000).await?.as_ref(),
            expected
        );
    }
    let partial = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/landing.css")
                .header("host", "example.com")
                .header("accept-encoding", "br")
                .header("range", "bytes=0-9")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!partial.headers().contains_key("content-encoding"));
    assert_eq!(
        partial.headers()["content-range"],
        format!("bytes 0-9/{}", css.len())
    );
    assert_eq!(
        to_bytes(partial.into_body(), 1_000_000).await?.as_ref(),
        &css[..10]
    );
    let multiple = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/landing.css")
                .header("host", "example.com")
                .header("accept-encoding", "br")
                .header("range", "bytes=0-9,30-39")
                .body(Body::empty())?,
        )
        .await?;
    // Multipart ranges keep serving the original file and never borrow the
    // Brotli sidecar as a compressed envelope.
    assert_eq!(multiple.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!multiple.headers().contains_key("content-encoding"));
    assert!(
        multiple.headers()["content-type"]
            .to_str()?
            .starts_with("multipart/byteranges")
    );
    let multiple = to_bytes(multiple.into_body(), 1_000_000).await?;
    assert!(multiple.windows(10).any(|window| window == &css[..10]));
    assert!(multiple.windows(10).any(|window| window == &css[30..40]));
    let icon = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/favicon.ico")
                .header("host", "example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(icon.status(), StatusCode::OK);
    assert!(
        to_bytes(icon.into_body(), 1_000_000)
            .await?
            .starts_with(&[0, 0, 1, 0])
    );
    let (status, headers, _) = get(app, "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .contains("immutable")
    );
    Ok(())
}

#[tokio::test]
async fn fingerprints_cannot_read_parent_paths_encoded_traversal_or_missing_assets() -> TestResult {
    let directory = TempDir::new()?;
    let static_dir = directory.path().join("static");
    std::fs::create_dir(&static_dir)?;
    std::fs::write(static_dir.join("asset.css"), "a{}")?;
    std::fs::write(directory.path().join("secret.css"), "private")?;
    let first = static_assets::fingerprint(&static_dir, "/static/asset.css")
        .await
        .ok_or("missing fingerprint")?;
    for path in [
        "/static/../secret.css",
        "/static/../../.env",
        "/static/%2e%2e/secret.css",
        "/static/%2e%2e/%2e%2e/.env",
        "/static/%2E%2E%2fsecret.css",
        "/static/..%5csecret.css",
        "/static/C:/secret.css",
        "/static/..\\secret.css",
        "/static//asset.css",
        "/static/missing.css",
        "/static/%00",
        "/static/",
        "/api/drafts",
        "/other/asset.css",
    ] {
        assert_eq!(
            static_assets::fingerprint(&static_dir, path).await,
            None,
            "{path}"
        );
    }
    std::fs::write(static_dir.join("asset.css"), "b{}")?;
    let second = static_assets::fingerprint(&static_dir, "/static/asset.css")
        .await
        .ok_or("missing changed fingerprint")?;
    assert_ne!(first, second);
    let html = static_assets::version_urls(&static_dir, format!(
        "<link href=\"/static/asset.css\"><link href=\"/static/asset.css?v={first}\"><img src=\"/static/missing.png\">"
    )).await?;
    assert!(html.contains(&format!("/static/asset.css?v={second}")));
    assert!(html.contains(&format!("/static/asset.css?v={first}")));
    assert!(html.contains("/static/missing.png\""));
    Ok(())
}

#[tokio::test]
async fn pwa_assets_are_public_and_linked_from_editor() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in ["/api/drafts", "/media/private.png"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "app.example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    let worker = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sw.js")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(worker.status(), StatusCode::OK);
    assert_eq!(worker.headers()["cache-control"], "no-cache");
    assert_eq!(worker.headers()["service-worker-allowed"], "/");
    assert!(
        worker.headers()["content-type"]
            .to_str()?
            .contains("javascript")
    );
    assert!(!to_bytes(worker.into_body(), 1_000_000).await?.is_empty());

    let manifest = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/manifest.webmanifest")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(manifest.status(), StatusCode::OK);
    let manifest: Value =
        serde_json::from_slice(&to_bytes(manifest.into_body(), 1_000_000).await?)?;
    assert_eq!(manifest["id"], "/");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["scope"], "/");
    assert_eq!(manifest["display"], "standalone");
    let icons = manifest["icons"].as_array().ok_or("icons")?;
    assert!(
        icons
            .iter()
            .filter(|icon| icon["purpose"] == "any")
            .filter_map(|icon| icon["sizes"].as_str())
            .any(|size| size == "192x192")
    );
    assert!(icons.iter().any(|icon| icon["purpose"] == "maskable"));
    for icon in icons {
        let src = icon["src"].as_str().ok_or("icon src")?;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(src)
                    .header("host", "app.example.com")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{src}");
        let bytes = to_bytes(response.into_body(), 1_000_000).await?;
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{src}");
        let width = u32::from_be_bytes(bytes[16..20].try_into()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into()?);
        assert_eq!(format!("{width}x{height}"), icon["sizes"], "{src}");
    }
    let apple = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/icons/apple-touch-icon.png")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(apple.status(), StatusCode::OK);

    let offline = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/static/offline.html")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(offline.status(), StatusCode::OK);
    let offline = String::from_utf8(to_bytes(offline.into_body(), 1_000_000).await?.to_vec())?;
    assert!(offline.contains("Нет подключения"));
    assert!(offline.contains("You're offline"));
    assert!(!offline.contains("<script"));

    let editor = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", "app.example.com")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(editor.status(), StatusCode::OK);
    let editor = String::from_utf8(to_bytes(editor.into_body(), 1_000_000).await?.to_vec())?;
    assert!(editor.contains("rel=\"manifest\" href=\"/static/manifest.webmanifest\""));
    assert!(editor.contains("rel=\"apple-touch-icon\""));
    assert!(!editor.contains("<rs-install"));
    assert!(editor.contains("rel=\"describedby\" href=\"/llms.txt\" type=\"text/markdown\""));
    Ok(())
}

#[tokio::test]
async fn agent_discovery_links_resolve_and_all_views_agree() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, _, llms) = get(app.clone(), "/llms.txt", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let llms = body_text(&llms)?;
    assert!(llms.starts_with("# RichPost\n"));
    let link = regex::Regex::new(r"\]\((https://(?:app\.)?example\.com[^)]+)\)")?;
    let urls = link
        .captures_iter(llms)
        .filter_map(|captures| captures.get(1))
        .map(|matched| matched.as_str())
        .collect::<Vec<_>>();
    assert!(!urls.is_empty());
    for url in urls {
        let parsed = url::Url::parse(url)?;
        let host = parsed.host_str().ok_or("discovery link host")?;
        let (status, _, body) = get(app.clone(), parsed.path(), host).await?;
        assert_eq!(status, StatusCode::OK, "{url}: {}", body_text(&body)?);
    }
    let (status, _, catalog) = get(app.clone(), "/agents.json", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let catalog: Value = serde_json::from_slice(&catalog)?;
    assert_eq!(catalog["site"]["url"], "https://example.com");
    assert_eq!(catalog["mcp"][0]["url"], "https://app.example.com/mcp");
    assert_eq!(catalog["mcp"][0]["type"], "streamable-http");
    let keys = catalog
        .as_object()
        .ok_or("catalog object")?
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from(["$schema", "version", "standard", "site", "mcp", "skills"])
    );
    let (_, _, directives) = get(app.clone(), "/agents.txt", "example.com").await?;
    let directives = body_text(&directives)?;
    let mcp_url = catalog["mcp"][0]["url"].as_str().ok_or("mcp url")?;
    let skill_url = catalog["skills"][0]["url"].as_str().ok_or("skill url")?;
    assert!(directives.contains(&format!("MCP: {mcp_url}\n")));
    assert!(directives.contains(&format!("Skills: {skill_url}\n")));
    let (_, _, full) = get(app.clone(), "/llms-full.txt", "example.com").await?;
    let full = body_text(&full)?;
    let (_, _, guide) = get(app.clone(), "/agents.md", "example.com").await?;
    let (_, _, skill) = get(app.clone(), "/skills/richpost/SKILL.md", "example.com").await?;
    assert!(full.contains(body_text(&guide)?));
    assert!(full.contains(body_text(&skill)?));
    Ok(())
}

#[tokio::test]
async fn agent_setup_examples_are_parseable_and_keep_credentials_out_of_config() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, _, guide) = get(app.clone(), "/agents.md", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let guide = body_text(&guide)?;
    let fenced_toml = regex::Regex::new(r"(?s)```toml\n(.*?)```")?;
    let toml = fenced_toml
        .captures(guide)
        .and_then(|captures| captures.get(1))
        .ok_or("toml setup block")?
        .as_str();
    let codex = toml_section(toml, "mcp_servers.richpost")?;
    assert_eq!(
        codex,
        BTreeMap::from([
            ("url".to_owned(), "https://app.example.com/mcp".to_owned()),
            (
                "bearer_token_env_var".to_owned(),
                "RICHPOST_TOKEN".to_owned()
            ),
        ])
    );
    let fenced_json = regex::Regex::new(r"(?s)```json\n(.*?)```")?;
    let claude: Value = serde_json::from_str(
        fenced_json
            .captures(guide)
            .and_then(|captures| captures.get(1))
            .ok_or("json setup block")?
            .as_str(),
    )?;
    let claude = &claude["mcpServers"]["richpost"];
    assert_eq!(
        claude,
        &json!({
            "type": "http",
            "url": "https://app.example.com/mcp",
            "headers": {"Authorization": "Bearer ${RICHPOST_TOKEN}"}
        })
    );
    assert_eq!(claude["url"].as_str(), Some(codex["url"].as_str()));
    for required in [
        "Required human step",
        "Токены API и MCP",
        "draft",
        "tools/list",
        "studio_status",
        "isError",
    ] {
        assert!(guide.contains(required), "{required}");
    }
    assert!(guide.contains("Do not mint a token"));
    let (status, _, skill) = get(app.clone(), "/skills/richpost/SKILL.md", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let skill = body_text(&skill)?;
    assert!(skill.starts_with("---\nname: richpost\ndescription:"));
    assert!(skill.contains("https://example.com/agents.md"));
    assert!(skill.contains("Token issuance is a human step"));
    Ok(())
}

#[tokio::test]
async fn forwarded_host_cannot_rewrite_installation_destinations() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in public_site::AGENT_PATHS {
        let forged = [
            ("x-forwarded-host", "attacker.invalid"),
            ("forwarded", "host=attacker.invalid"),
        ];
        let (status, _, body) = get_with(app.clone(), path, "mirror.example", &forged).await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        let (plain_status, _, plain) = get(app.clone(), path, "example.com").await?;
        assert_eq!(plain_status, StatusCode::OK, "{path}");
        assert_eq!(body, plain, "{path}");
        let text = body_text(&body)?;
        assert!(
            !text.contains("attacker.invalid") && !text.contains("mirror.example"),
            "{path}"
        );
    }
    let (status, _, _) = get(app.clone(), "/agents.md", "attacker.invalid").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn agent_documents_do_not_expose_secrets_or_account_data() -> TestResult {
    let directory = TempDir::new()?;
    let (app, _settings) = app_with(
        &directory,
        &[
            ("STUDIO_API_KEY", "operator-secret-sentinel"),
            ("TG_BOT_TOKEN", "999:BOT_SECRET_SENTINEL"),
            ("TG_IMPORT_API_ID", "12345"),
            ("TG_IMPORT_API_HASH", "import-secret-sentinel"),
        ],
    )?;
    let mut public_docs = BTreeMap::new();
    for path in public_site::AGENT_PATHS {
        let (status, _, body) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        public_docs.insert(*path, body);
    }
    let session = login(&app, "app.example.com", "999:BOT_SECRET_SENTINEL").await?;
    let created = issue_token(&app, "app.example.com", &session).await?;
    let bearer = bearer(&created)?;
    let block = json!({
        "rich_message": {
            "blocks": [
                {"type": "heading", "size": 1, "text": "Тест"},
                {"type": "paragraph", "text": ["Привет, ", {"type": "bold", "text": "мир"}]}
            ]
        }
    });
    let (status, _, draft) = post_json(
        app.clone(),
        "/api/drafts",
        "app.example.com",
        json!({"title": "PRIVATE_ACCOUNT_SENTINEL", "post": block}),
        &[("cookie", &session)],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let secret = created["secret"].as_str().ok_or("token secret")?;
    for (path, content) in &public_docs {
        let (status, _, plain) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(&plain, content, "{path} changed without authentication");
        let (status, _, authenticated) = get_with(
            app.clone(),
            path,
            "app.example.com",
            &[("authorization", &bearer)],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(&authenticated, content, "{path} changed for a bearer token");
        for value in [
            "operator-secret-sentinel",
            "999:BOT_SECRET_SENTINEL",
            "import-secret-sentinel",
            secret,
            "PRIVATE_ACCOUNT_SENTINEL",
        ] {
            assert!(
                !body_text(content)?.contains(value),
                "{path} leaked {value}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn public_document_access_does_not_grant_mcp_or_token_management() -> TestResult {
    let directory = TempDir::new()?;
    let (base, calls) = recording_telegram().await?;
    let (app, settings) = app_with(
        &directory,
        &[("TG_API_BASE", base.as_str()), ("STUDIO_BACKGROUND", "off")],
    )?;
    let (status, _, _) = get(app.clone(), "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mcp = [
        ("accept", "application/json, text/event-stream"),
        ("mcp-protocol-version", "2025-06-18"),
    ];
    let (status, _, body) = post_json(
        app.clone(),
        "/mcp",
        "app.example.com",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        &mcp,
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let session = login(&app, "app.example.com", "1:public-site-secret").await?;
    let created = issue_token(&app, "app.example.com", &session).await?;
    let bearer = bearer(&created)?;
    let mut headers = vec![("authorization", bearer.as_str())];
    headers.extend(mcp);
    let (status, _, result) = post_json(
        app.clone(),
        "/mcp",
        "app.example.com",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"studio_status","arguments":{}}}),
        &headers,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_ne!(result["result"]["isError"], true, "{result}");
    let (status, _, denied) = post_json(
        app.clone(),
        "/api/account/tokens",
        "app.example.com",
        json!({"name":"automatic"}),
        &[("authorization", &bearer)],
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let root = AccountContext::root(&settings);
    assert!(drafts::list(&root)?.is_empty());
    assert!(posts::list(&root)?.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "Telegram was called");
    Ok(())
}

#[tokio::test]
async fn private_installation_documents_work_behind_operator_auth() -> TestResult {
    let directory = TempDir::new()?;
    let (app, _settings) = app_with(
        &directory,
        &[
            ("STUDIO_PUBLIC", "off"),
            ("STUDIO_API_KEY", "private-studio-sentinel"),
            ("STUDIO_LANDING_URL", ""),
        ],
    )?;
    for path in public_site::AGENT_PATHS {
        let (status, _, _) = get(app.clone(), path, "localhost").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let (status, _, guide) = get(app.clone(), "/agents.md", "localhost").await?;
    assert_eq!(status, StatusCode::OK);
    let guide = body_text(&guide)?;
    assert!(guide.contains("Ask the operator"));
    assert!(!guide.contains("private-studio-sentinel"));
    let (status, _, _) = get(app.clone(), "/api/drafts", "localhost").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn human_pages_expose_setup_links_and_markdown_discovery() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for prefix in ["", "/en"] {
        let (status, _, home) = get(app.clone(), &format!("{prefix}/"), "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{prefix}/");
        let home = body_text(&home)?;
        assert!(
            home.contains(&format!("href=\"{prefix}/features/ai-agents/\"")),
            "{prefix}/"
        );
        let (status, _, page) = get(
            app.clone(),
            &format!("{prefix}/features/ai-agents/"),
            "example.com",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{prefix}/features/ai-agents/");
        let page = body_text(&page)?;
        assert!(page.contains("rel=\"describedby\" href=\"/llms.txt\" type=\"text/markdown\""));
        assert!(page.contains("href=\"https://example.com/agents.md\""));
        assert!(page.contains("href=\"/skills/richpost/SKILL.md\""));
    }
    Ok(())
}

#[tokio::test]
async fn languages_are_explicit_and_reciprocal() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let alternate = regex::Regex::new(r#"<link rel="alternate"[^>]+>"#)?;
    let cyrillic = regex::Regex::new("[а-яА-ЯёЁ]")?;
    for path in ["/", "/features/telegram-tables/"] {
        let (status, _, ru_body) = get_with(
            app.clone(),
            path,
            "example.com",
            &[("accept-language", "en")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        let (status, _, en_body) = get_with(
            app.clone(),
            &format!("/en{path}"),
            "example.com",
            &[("accept-language", "ru")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "/en{path}");
        let ru = body_text(&ru_body)?;
        let en = body_text(&en_body)?;
        assert!(ru.contains("<html lang=\"ru\">"), "{path}");
        assert!(en.contains("<html lang=\"en\">"), "/en{path}");
        let ru_alternates = alternate
            .find_iter(ru)
            .map(|matched| matched.as_str())
            .collect::<Vec<_>>();
        let en_alternates = alternate
            .find_iter(en)
            .map(|matched| matched.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ru_alternates, en_alternates, "{path}");
        assert!(en.contains("?lang=en\""), "/en{path}");
        let without_label = en.replace("aria-label=\"Русская версия\"", "");
        assert!(!cyrillic.is_match(&without_label), "/en{path}");
    }
    Ok(())
}

#[tokio::test]
async fn app_and_local_previews_are_noindex() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    for path in ["/", "/en/", "/landing", "/landing/en/"] {
        let (status, headers, _) = get(app.clone(), path, "app.example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers["x-robots-tag"], "noindex, nofollow", "{path}");
    }
    let (status, _, home) = get(app.clone(), "/", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body_text(&home)?.contains("data-nosnippet"));
    let (status, headers, _) = get(app, "/api/drafts", "app.example.com").await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["x-robots-tag"], "noindex, nofollow");
    Ok(())
}

#[tokio::test]
async fn robots_allows_reading_noindex_and_advertises_only_marketing_sitemap() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let (status, headers, body) = get(app.clone(), "/robots.txt", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str()?.starts_with("text/plain"));
    let text = body_text(&body)?;
    assert!(text.contains("Allow: /"));
    assert!(text.contains("Sitemap: https://example.com/sitemap.xml"));
    let (status, _, body) = get(app.clone(), "/robots.txt", "app.example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let text = body_text(&body)?;
    assert!(!text.contains("Disallow: /") && !text.contains("Sitemap:"));
    let (status, _, _) = get(app, "/sitemap.xml", "app.example.com").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn json_ld_cannot_break_out_of_script_from_a_configured_url() -> TestResult {
    let directory = TempDir::new()?;
    let malicious = "https://app.example.com/?x=</script><script>alert(1)</script>&y=2";
    let (app, _settings) = app_with(&directory, &[("STUDIO_URL", malicious)])?;
    let (status, _, body) = get(app, "/", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    let html = body_text(&body)?;
    assert_eq!(
        html.matches("<script type=\"application/ld+json\">")
            .count(),
        1
    );
    let data = between(html, "<script type=\"application/ld+json\">", "</script>")?;
    assert!(!data.contains('<') && !data.contains('&'), "{data}");
    assert!(!html.contains("<script>alert(1)</script>"));
    let parsed: Value = serde_json::from_str(data)?;
    assert!(serde_json::to_string(&parsed)?.contains("</script>"));
    Ok(())
}

#[tokio::test]
async fn icons_discovery_head_and_fingerprint_cache_are_usable() -> TestResult {
    let directory = TempDir::new()?;
    let app = app(&directory)?;
    let mut paths = vec!["/favicon.ico", "/robots.txt", "/sitemap.xml"];
    paths.extend_from_slice(public_site::AGENT_PATHS);
    for path in paths {
        let (status, headers, _) = get(app.clone(), path, "example.com").await?;
        let (head_status, head_headers, head_body) = head(app.clone(), path, "example.com").await?;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(head_status, status, "HEAD {path}");
        assert!(head_body.is_empty(), "HEAD {path}");
        assert_eq!(
            head_headers["content-type"], headers["content-type"],
            "{path}"
        );
    }
    let (status, _, favicon) = get(app.clone(), "/favicon.ico", "example.com").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(favicon.starts_with(b"\x00\x00\x01\x00"));

    let static_dir = rich_studio::runtime::static_dir();
    let asset = "/static/landing.css";
    let digest = static_assets::fingerprint(&static_dir, asset)
        .await
        .ok_or("missing fingerprint")?;
    let source = std::fs::read("app/static/landing.css")?;
    for (query, immutable) in [
        (String::new(), false),
        ("?v=old".to_owned(), false),
        (format!("?v={digest}"), true),
        (format!("?v={digest}&v={digest}"), false),
    ] {
        let (status, headers, body) = get_with(
            app.clone(),
            &format!("{asset}{query}"),
            "example.com",
            &[("accept-encoding", "identity")],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(
            headers["cache-control"],
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=0, must-revalidate"
            },
            "{query}"
        );
        assert_eq!(body, source, "{query}");
    }
    for path in [
        "/static/../../.env",
        "/static/%2e%2e/%2e%2e/.env",
        "/api/drafts",
        "/static/%00",
    ] {
        assert_eq!(
            static_assets::fingerprint(&static_dir, path).await,
            None,
            "{path}"
        );
    }
    for encoding in ["identity", "br"] {
        let (status, _, _) = get_with(
            app.clone(),
            "/static/%00",
            "example.com",
            &[("accept-encoding", encoding)],
        )
        .await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{encoding}");
    }
    Ok(())
}

#[test]
fn checked_in_public_derivatives_match_their_sources() -> TestResult {
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string("app/bundled/public-assets.json")?)?;
    let static_dir = std::path::Path::new("app/static");
    let sha = |path: &str| -> Result<String, Box<dyn std::error::Error>> {
        let bytes = std::fs::read(static_dir.join(path))?;
        Ok(rich_studio::hex::encode(&Sha256::digest(&bytes)))
    };
    for (path, entry) in manifest["compressed"].as_object().ok_or("compressed")? {
        assert_eq!(
            sha(path)?,
            entry["source_sha256"].as_str().ok_or("source_sha256")?,
            "{path}"
        );
        assert_eq!(
            sha(&format!("{path}.br"))?,
            entry["brotli_sha256"].as_str().ok_or("brotli_sha256")?,
            "{path}.br"
        );
    }
    for (path, entry) in manifest["derived"].as_object().ok_or("derived")? {
        assert_eq!(
            sha(entry["source"].as_str().ok_or("source")?)?,
            entry["source_sha256"].as_str().ok_or("source_sha256")?,
            "{path}"
        );
        assert_eq!(
            sha(path)?,
            entry["sha256"].as_str().ok_or("sha256")?,
            "{path}"
        );
    }
    let font = &manifest["font"];
    assert_eq!(
        sha("fonts/manrope.ttf")?,
        font["source_sha256"].as_str().ok_or("font source")?
    );
    assert_eq!(
        sha("fonts/manrope.woff2")?,
        font["sha256"].as_str().ok_or("font")?
    );
    Ok(())
}
