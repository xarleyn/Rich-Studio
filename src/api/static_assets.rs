//! Cache only existing public assets. A year-long lifetime requires the current
//! file's content fingerprint, never an arbitrary user-supplied query string.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use regex::Regex;
use sha2::{Digest as _, Sha256};

use crate::AppState;

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
    Some(format!("{:x}", Sha256::digest(bytes))[..12].to_owned())
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
    let mut response = next.run(request).await;
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
