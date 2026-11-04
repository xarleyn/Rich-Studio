//! Cache only existing public assets. A year-long lifetime requires the current
//! file's content fingerprint, never an arbitrary user-supplied query string.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use regex::Regex;
use sha2::{Digest as _, Sha256};

use crate::AppState;

/// Multipart ranges are rare; keep the response bounded instead of copying an
/// arbitrary number of overlapping slices into memory.
const MAX_MULTIPART_RANGES: usize = 16;

async fn file_path(directory: &Path, url: &str) -> Option<PathBuf> {
    let relative = url.strip_prefix("/static/")?;
    // All shipped asset names are ASCII. Encoded separators/dots and Windows
    // drive syntax must never turn a URL fingerprint into a filesystem oracle.
    if relative.is_empty()
        || !relative
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/".contains(&byte))
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || Path::new(relative)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let root = tokio::fs::canonicalize(directory).await.ok()?;
    let file = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
    if !file.starts_with(&root) || !tokio::fs::metadata(&file).await.ok()?.is_file() {
        return None;
    }
    Some(file)
}

pub async fn fingerprint(directory: &Path, url: &str) -> Option<String> {
    let file = file_path(directory, url).await?;
    let bytes = tokio::fs::read(file).await.ok()?;
    Some(crate::hex::encode(&Sha256::digest(bytes))[..12].to_owned())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn multipart_ranges(value: &str, size: u64) -> Option<Vec<(u64, u64)>> {
    let specifications = value.strip_prefix("bytes=")?;
    let mut ranges = Vec::new();
    for specification in specifications.split(',') {
        let (start, end) = specification.trim().split_once('-')?;
        let start = start.trim().parse::<u64>().ok()?;
        let end = if end.trim().is_empty() {
            size.checked_sub(1)?
        } else {
            end.trim().parse::<u64>().ok()?
        };
        if start > end || end >= size {
            return None;
        }
        ranges.push((start, end));
    }
    (2..=MAX_MULTIPART_RANGES)
        .contains(&ranges.len())
        .then_some(ranges)
}

async fn multipart_response(
    state: &AppState,
    method: Method,
    path: &str,
    range: Option<&str>,
) -> Option<Response> {
    if ![Method::GET, Method::HEAD].contains(&method) || !path.starts_with("/static/") {
        return None;
    }
    let range = range?;
    if !range.contains(',') {
        return None;
    }
    let file = file_path(&state.static_dir, path).await?;
    let bytes = tokio::fs::read(&file).await.ok()?;
    let ranges = multipart_ranges(range, bytes.len() as u64)?;
    let boundary = format!(
        "richstudio-{}",
        &crate::hex::encode(&Sha256::digest(&bytes))[..16]
    );
    let mime = content_type(&file);
    let mut body = Vec::new();
    for (start, end) in ranges {
        let start = usize::try_from(start).ok()?;
        let end = usize::try_from(end).ok()?;
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {start}-{end}/{}\r\n\r\n",
                bytes.len()
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes.get(start..=end)?);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let length = body.len();
    let content_type =
        HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}")).ok()?;
    let mut response = Response::new(if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body)
    });
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).ok()?,
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    Some(response)
}

/// Templates and generated JSON-LD share the same asset URLs. Fingerprint the
/// finished document before calculating the inline structured-data CSP hash.
pub async fn version_urls(directory: &Path, html: String) -> Result<String, regex::Error> {
    let urls = Regex::new(r#"/static/[A-Za-z0-9_./-]+(?:\?[^\s\"'<>]*)?"#)?;
    let mut replacements = BTreeMap::new();
    for matched in urls.find_iter(&html) {
        let url = matched.as_str();
        if url.contains('?') || replacements.contains_key(url) {
            continue;
        }
        if let Some(hash) = fingerprint(directory, url).await {
            replacements.insert(url.to_owned(), format!("{url}?v={hash}"));
        }
    }
    Ok(urls
        .replace_all(&html, |captures: &regex::Captures<'_>| {
            let url = &captures[0];
            replacements
                .get(url)
                .cloned()
                .unwrap_or_else(|| url.to_owned())
        })
        .into_owned())
}

pub async fn cache(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    // Range responses must refer to the original file. A partial compressed
    // stream cannot be decoded on its own, and a multipart envelope is never
    // itself Brotli data merely because each requested part came from a .br.
    if path.starts_with("/static/") && request.headers().contains_key(header::RANGE) {
        request.headers_mut().insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    let multipart = if path.starts_with("/static/") {
        let range = request
            .headers()
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok());
        multipart_response(&state, request.method().clone(), &path, range).await
    } else {
        None
    };
    let mut response = match multipart {
        Some(response) => response,
        None => next.run(request).await,
    };
    if !path.starts_with("/static/")
        || !(response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED)
    {
        return response;
    }
    let version = query.as_deref().and_then(|query| {
        let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
        (pairs.len() == 1 && pairs[0].0 == "v").then(|| pairs[0].1.to_string())
    });
    let immutable = if let Some(version) = version {
        fingerprint(&state.static_dir, &path)
            .await
            .is_some_and(|fingerprint| version == fingerprint)
    } else {
        false
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=0, must-revalidate"
        }),
    );
    // Precompressed responses and the identity representation share one URL.
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}
