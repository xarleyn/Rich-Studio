use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::static_assets;
use crate::{
    AppState,
    public_site::{self, PublicHtml, escape_html},
    settings::Settings,
};

const SEO: &str = include_str!("../../app/bundled/public-site-seo.json");
const JSON_LD_OPEN: &str = "<script type=\"application/ld+json\">";

#[derive(Deserialize)]
struct PageMetadata {
    lastmod: String,
    schemas: Vec<Value>,
}

#[derive(Deserialize)]
struct Metadata {
    pages: BTreeMap<String, PageMetadata>,
}

fn metadata() -> Result<&'static Metadata, StatusCode> {
    static PARSED: OnceLock<Result<Metadata, serde_json::Error>> = OnceLock::new();
    PARSED
        .get_or_init(|| serde_json::from_str(SEO))
        .as_ref()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub fn router() -> Router<AppState> {
    let mut router = Router::new()
        .route("/landing", get(preview))
        .route("/landing/en/", get(english))
        .route("/en/", get(english))
        .route("/features/", get(features))
        .route("/en/features/", get(features))
        .route("/features/{slug}/", get(guide))
        .route("/en/features/{slug}/", get(guide))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/favicon.ico", get(favicon));
    for path in public_site::page_paths() {
        if path != "/" {
            router = router.route(path.trim_end_matches('/'), get(canonical_slash));
        }
    }
    router
}

fn language(uri: &Uri) -> &'static str {
    if uri.path().starts_with("/en/") {
        "en"
    } else {
        "ru"
    }
}

fn site(settings: &Settings) -> &str {
    if settings.landing_url.is_empty() {
        &settings.public_url
    } else {
        &settings.landing_url
    }
}

fn replace_schema_urls(value: &mut Value, settings: &Settings, canonical: &str) {
    match value {
        Value::String(text) => {
            *text = text
                .replace("__SITE_URL__", site(settings))
                .replace("__PAGE_URL__", canonical)
                .replace("__APP_URL__", &settings.public_url);
        }
        Value::Array(items) => {
            for item in items {
                replace_schema_urls(item, settings, canonical);
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                replace_schema_urls(item, settings, canonical);
            }
        }
        _ => {}
    }
}

fn structured_data(settings: &Settings, path: &str, canonical: &str) -> Result<String, StatusCode> {
    let entry = metadata()?
        .pages
        .get(path)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut graph = json!({"@context": "https://schema.org", "@graph": entry.schemas});
    replace_schema_urls(&mut graph, settings, canonical);
    let json = serde_json::to_string(&graph)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(format!("{JSON_LD_OPEN}{json}</script>"))
}

pub async fn page(settings: &Settings, language: &str, slug: &str) -> Result<String, StatusCode> {
    if !["ru", "en"].contains(&language)
        || (!slug.is_empty()
            && !["features", "not-found"].contains(&slug)
            && !public_site::GUIDES.contains(&slug))
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let base = site(settings);
    let prefix = if language == "en" { "/en" } else { "" };
    let path = match slug {
        "" | "not-found" => "/".to_owned(),
        "features" => "/features/".to_owned(),
        _ => format!("/features/{slug}/"),
    };
    let canonical = format!("{base}{prefix}{path}");
    let template = if slug.is_empty() {
        format!("landing{}.html", if language == "en" { "-en" } else { "" })
    } else {
        format!("pages/{slug}.{language}.html")
    };
    let directory = crate::runtime::static_dir();
    let mut content = tokio::fs::read_to_string(directory.join(template))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        // Python reads templates in text mode, so both platforms serve LF.
        .replace("\r\n", "\n");
    for (key, value) in [
        (
            "__APP_URL__",
            format!("{}?lang={language}", settings.public_url),
        ),
        ("__SITE_URL__", base.to_owned()),
        ("__HUB_URL__", format!("{base}{prefix}/features/")),
        ("__LANDING_URL__", canonical.clone()),
        (
            "__OG_IMAGE_URL__",
            format!("{base}/static/images/social-preview.png"),
        ),
        ("__RU_URL__", format!("{base}{path}")),
        ("__EN_URL__", format!("{base}/en{path}")),
        ("__BOT_USERNAME__", settings.bot_username.clone()),
        ("__AGENT_GUIDE_URL__", format!("{base}/agents.md")),
    ] {
        content = content.replace(key, &escape_html(&value));
    }
    let schema = if slug == "not-found" {
        String::new()
    } else {
        structured_data(settings, &format!("{prefix}{path}"), &canonical)?
    };
    content = content.replace("__STRUCTURED_DATA__", &schema);
    content = content.replace(
        "</head>",
        "<link rel=\"describedby\" href=\"/llms.txt\" type=\"text/markdown\">\n</head>",
    );
    static_assets::version_urls(&directory, content)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.settings.is_landing_host(host(&headers)) {
        return render(&state.settings, "ru", "", StatusCode::OK).await;
    }
    match tokio::fs::read(state.static_dir.join("index.html")).await {
        Ok(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn render(settings: &Settings, language: &str, slug: &str, status: StatusCode) -> Response {
    match page(settings, language, slug).await {
        Ok(page) => {
            let structured_data_hash = page
                .split_once(JSON_LD_OPEN)
                .and_then(|(_, after)| after.split_once("</script>"))
                .map(|(json, _)| STANDARD.encode(Sha256::digest(json.as_bytes())));
            let mut response = (status, Html(page)).into_response();
            response.extensions_mut().insert(PublicHtml {
                structured_data_hash,
            });
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            if status == StatusCode::NOT_FOUND {
                response.headers_mut().insert(
                    "x-robots-tag",
                    HeaderValue::from_static("noindex, nofollow"),
                );
            }
            response
        }
        Err(status) => status.into_response(),
    }
}

async fn preview(State(state): State<AppState>) -> Response {
    render(&state.settings, "ru", "", StatusCode::OK).await
}
async fn english(State(state): State<AppState>) -> Response {
    render(&state.settings, "en", "", StatusCode::OK).await
}
async fn features(State(state): State<AppState>, uri: Uri) -> Response {
    render(&state.settings, language(&uri), "features", StatusCode::OK).await
}
async fn guide(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !public_site::GUIDES.contains(&slug.as_str()) {
        return missing(state, headers, uri).await;
    }
    render(&state.settings, language(&uri), &slug, StatusCode::OK).await
}

async fn missing(state: AppState, headers: HeaderMap, uri: Uri) -> Response {
    if state.settings.is_landing_host(host(&headers)) {
        render(
            &state.settings,
            language(&uri),
            "not-found",
            StatusCode::NOT_FOUND,
        )
        .await
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
    }
}

pub async fn not_found(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
) -> Response {
    let mut response = missing(state, headers, uri).await;
    if method == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn canonical_slash(uri: Uri) -> Response {
    let query = uri
        .query()
        .map_or_else(String::new, |query| format!("?{query}"));
    Redirect::permanent(&format!("{}/{query}", uri.path())).into_response()
}

fn host(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

async fn favicon(State(state): State<AppState>) -> Response {
    match tokio::fs::read(state.static_dir.join("icons/favicon.ico")).await {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "image/vnd.microsoft.icon"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn robots(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut body = "User-agent: *\nAllow: /\n".to_owned();
    if state.settings.is_landing_host(host(&headers)) {
        body.push_str(&format!(
            "Sitemap: {}/sitemap.xml\n",
            state.settings.landing_url
        ));
    }
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}

async fn sitemap(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.settings.is_landing_host(host(&headers)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let metadata = match metadata() {
        Ok(metadata) => metadata,
        Err(status) => return status.into_response(),
    };
    let mut urls = String::new();
    for path in public_site::page_paths() {
        let Some(entry) = metadata.pages.get(&path) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let ru = path
            .strip_prefix("/en/")
            .map_or_else(|| path.clone(), |rest| format!("/{rest}"));
        let base = &state.settings.landing_url;
        urls.push_str(&format!(
            "<url><loc>{}</loc><lastmod>{}</lastmod>",
            escape_html(&format!("{base}{path}")),
            escape_html(&entry.lastmod)
        ));
        for (language, target) in [
            ("ru", format!("{base}{ru}")),
            ("en", format!("{base}/en{ru}")),
            ("x-default", format!("{base}{ru}")),
        ] {
            urls.push_str(&format!(
                "<xhtml:link rel=\"alternate\" hreflang=\"{language}\" href=\"{}\"/>",
                escape_html(&target)
            ));
        }
        urls.push_str("</url>");
    }
    ([(header::CONTENT_TYPE, "application/xml"), (header::CACHE_CONTROL, "public, max-age=300")], format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" xmlns:xhtml=\"http://www.w3.org/1999/xhtml\">{urls}</urlset>")).into_response()
}
