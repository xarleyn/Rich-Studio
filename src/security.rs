use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::json;

use crate::AppState;
use crate::account_context::{AccountContext, Role};
use crate::services::auth;
use crate::settings::Settings;
use crate::store::auth as store_auth;

pub use crate::services::auth::{AuthError, Principal};

pub const SESSION_COOKIE: &str = "rs_session";
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:; media-src 'self' blob: https:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

#[derive(Clone)]
pub struct ScopedState(pub AppState);

impl FromRequestParts<AppState> for ScopedState {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AppState>()
            .cloned()
            .map(Self)
            .ok_or_else(|| AuthError::new(500, "request account scope is missing").into_response())
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(json!({"detail":self.message}))).into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        if status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
        }
        response
    }
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

fn text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

pub fn https(headers: &HeaderMap) -> bool {
    text(headers, "x-forwarded-proto") == "https"
}

/// Resolve the visitor only through an explicitly trusted TCP peer. Every hop
/// must be an IP; malformed chains keep the peer's budget instead of granting
/// an attacker a fresh bucket for arbitrary header text.
pub fn client_ip(
    settings: &Settings,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<IpAddr> {
    let peer = peer?.ip();
    if !settings.forwarded_allow_ips.contains(peer) {
        return Some(peer);
    }
    let mut chain = Vec::new();
    for value in headers.get_all("x-forwarded-for") {
        let Ok(value) = value.to_str() else {
            return Some(peer);
        };
        for host in value.split(',').map(str::trim) {
            let address = host
                .parse::<IpAddr>()
                .ok()
                .or_else(|| host.parse::<SocketAddr>().ok().map(|address| address.ip()))
                .or_else(|| {
                    host.strip_prefix('[')?
                        .strip_suffix(']')?
                        .parse::<std::net::Ipv6Addr>()
                        .ok()
                        .map(IpAddr::V6)
                });
            let Some(address) = address else {
                return Some(peer);
            };
            chain.push(address);
        }
    }
    if settings.forwarded_allow_ips.all {
        return Some(chain.first().copied().unwrap_or(peer));
    }
    Some(
        chain
            .iter()
            .rev()
            .copied()
            .find(|address| !settings.forwarded_allow_ips.contains(*address))
            .or_else(|| chain.first().copied())
            .unwrap_or(peer),
    )
}

pub fn host_ok(settings: &Settings, host: &str) -> bool {
    let host = host.to_lowercase();
    if settings.allowed_hosts.contains(&host) {
        return true;
    }
    let name = if host.ends_with(']') {
        host.as_str()
    } else {
        host.rsplit_once(':')
            .map_or(host.as_str(), |(name, _)| name)
    };
    settings.allowed_hosts.contains(name)
}

pub fn same_origin(origin: &str, host: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let default = if url.scheme() == "https" {
        ":443"
    } else {
        ":80"
    };
    authority
        .trim_end_matches(default)
        .eq_ignore_ascii_case(host.trim_end_matches(default))
}

pub fn is_page(path: &str) -> bool {
    crate::public_site::is_page(path)
}

fn public_path(path: &str, method: &Method) -> bool {
    is_page(path)
        || ([Method::GET, Method::HEAD].contains(method)
            && crate::public_site::page_paths()
                .iter()
                .any(|known| known != "/" && known.trim_end_matches('/') == path))
        || [
            "/favicon.ico",
            "/healthz",
            "/readyz",
            "/sw.js",
            "/robots.txt",
            "/sitemap.xml",
            "/llms.txt",
            "/llms-full.txt",
            "/agents.txt",
            "/agents.json",
            "/agents.md",
            "/skills/richpost/SKILL.md",
            "/api/auth/config",
            "/api/auth/telegram/start",
            "/auth/telegram/callback",
        ]
        .contains(&path)
        || path.starts_with("/static/")
        || (path == "/api/session" && method != Method::GET)
        || (path == "/api/telegram/webhook" && method == Method::POST)
}

fn landing_path(path: &str, method: &Method) -> bool {
    is_page(path)
        || ([Method::GET, Method::HEAD].contains(method)
            && crate::public_site::page_paths()
                .iter()
                .any(|known| known != "/" && known.trim_end_matches('/') == path))
        || crate::public_site::AGENT_PATHS.contains(&path)
        || [
            "/favicon.ico",
            "/healthz",
            "/readyz",
            "/sw.js",
            "/robots.txt",
            "/sitemap.xml",
        ]
        .contains(&path)
        || path.starts_with("/static/")
}

fn safe(method: &Method) -> bool {
    [Method::GET, Method::HEAD, Method::OPTIONS].contains(method)
}

fn route_role(path: &str, method: &Method) -> Role {
    if path == "/api/export"
        || path.starts_with("/api/raw/")
        || (path.starts_with("/api/suggested-posts/") && path.ends_with("/reopen"))
    {
        return Role::Admin;
    }
    if safe(method) || path == "/mcp" {
        return Role::Read;
    }
    if [
        "/api/send",
        "/api/edit",
        "/api/schedule",
        "/api/poll",
        "/api/media-post",
    ]
    .contains(&path)
        || path.starts_with("/api/posts/")
        || path.starts_with("/api/invite-links")
        || path.starts_with("/api/suggested-posts/")
    {
        Role::Publish
    } else {
        Role::Drafts
    }
}

fn credentials(settings: &Settings, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
    let authorization = text(headers, "authorization");
    let bearer = authorization
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .map_or("", |_| authorization[7..].trim());
    let secret = if bearer.is_empty() {
        text(headers, "x-api-key")
    } else {
        bearer
    };
    if !secret.is_empty() {
        return auth::by_secret(settings, secret);
    }
    if let Some(secret) = cookie(headers, SESSION_COOKIE) {
        return auth::by_session(settings, &secret);
    }
    Ok(None)
}

pub async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let host = text(request.headers(), "host").to_owned();
    if !host_ok(&state.settings, &host) {
        return AuthError::new(400, "неизвестный Host; добавьте его в ALLOWED_HOSTS")
            .into_response();
    }
    let landing = state.settings.is_landing_host(&host);
    if landing && !landing_path(&path, &method) {
        if !safe(&method) {
            return AuthError::new(
                403,
                format!(
                    "Используйте адрес приложения: {}",
                    state.settings.public_url
                ),
            )
            .into_response();
        }
        if ["/mcp", "/docs", "/redoc", "/openapi.json"].contains(&path.as_str())
            || ["/api/", "/auth/", "/render/", "/media/", "/docs/"]
                .iter()
                .any(|prefix| path.starts_with(prefix))
        {
            return Redirect::temporary(&format!("{}{}", state.settings.public_url, request.uri()))
                .into_response();
        }
    }
    if !safe(&method) {
        let origin = text(request.headers(), "origin");
        if ["cross-site", "same-site"].contains(&text(request.headers(), "sec-fetch-site"))
            || origin == "null"
            || (!origin.is_empty() && !same_origin(origin, &host))
        {
            return AuthError::new(403, "запрос с другого сайта отклонён").into_response();
        }
        let ctype = text(request.headers(), "content-type")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let expected = if path == "/api/media" {
            "multipart/form-data"
        } else {
            "application/json"
        };
        if path.starts_with("/api/") && !ctype.is_empty() && ctype != expected {
            return AuthError::new(415, format!("ожидался {expected}")).into_response();
        }
    }
    let public = public_path(&path, &method) || (landing && safe(&method));
    let login_budget = state.settings.public
        && [
            "/api/auth/telegram/start",
            "/auth/telegram/callback",
            "/api/session",
        ]
        .contains(&path.as_str())
        && (method == Method::POST || path == "/auth/telegram/callback");
    let peer = client_ip(
        &state.settings,
        request.headers(),
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0),
    )
    .map_or_else(|| "unknown".to_owned(), |peer| peer.to_string());
    let settings = Arc::clone(&state.settings);
    let headers = request.headers().clone();
    let authentication = tokio::task::spawn_blocking(move || -> Result<Principal, AuthError> {
        if login_budget && !store_auth::take_budget(&settings, &format!("login:{peer}"), 30, 60)? {
            return Err(AuthError::new(
                429,
                "Слишком много попыток входа. Попробуйте через минуту.",
            ));
        }
        if public || !auth::auth_on(&settings)? {
            return Ok(Principal::owner());
        }
        credentials(&settings, &headers)?.ok_or_else(|| {
            AuthError::new(
                401,
                "нужен токен: заголовок Authorization: Bearer <токен> или X-API-Key",
            )
        })
    })
    .await;
    let who = match authentication {
        Ok(Ok(who)) => who,
        Ok(Err(error)) => return error.into_response(),
        Err(_) => return AuthError::new(500, "authentication worker failed").into_response(),
    };
    if !public && let Err(error) = auth::require(&who, route_role(&path, &method)) {
        return error.into_response();
    }
    if who.telegram_id.is_some()
        && (["/api/logs", "/api/spec/refresh"].contains(&path.as_str())
            || path.starts_with("/api/raw/"))
    {
        return AuthError::new(403, "доступно только владельцу сервиса").into_response();
    }
    let mut scoped = state.clone();
    scoped.principal = who.clone();
    if let Some(uid) = who.telegram_id {
        scoped.account = AccountContext::account(&state.settings, uid, who.role);
        let mut settings = (*state.settings).clone();
        settings.default_chat.clear();
        settings.preview_chat = uid.to_string();
        scoped.settings = Arc::new(settings);
    } else {
        scoped.account = AccountContext::root(&state.settings);
        scoped.account.role = who.role;
    }
    request.extensions_mut().insert(scoped);
    let mut response = next.run(request).await;
    let public_html = response
        .extensions()
        .get::<crate::public_site::PublicHtml>();
    if public_html.is_some()
        || is_page(&path)
        || path.starts_with("/render/")
        || ["/api/render", "/docs", "/redoc"].contains(&path.as_str())
    {
        let mut csp = CSP.to_owned();
        if let Some(hash) = public_html.and_then(|html| html.structured_data_hash.as_ref()) {
            csp = csp.replace(
                "script-src 'self'",
                &format!("script-src 'self' 'sha256-{hash}'"),
            );
        }
        if path == "/redoc" {
            csp.push_str("; worker-src 'self' blob:");
        }
        if path == "/" && state.settings.public && !landing {
            csp = csp.replace(
                "script-src 'self'",
                "script-src 'self' https://telegram.org",
            );
            csp.push_str("; frame-src https://oauth.telegram.org");
        }
        if let Ok(value) = header::HeaderValue::from_str(&csp) {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    if (!public || path.starts_with("/api/auth/") || path.starts_with("/auth/"))
        && !(path.starts_with("/api/emoji/img/") || path.starts_with("/api/emoji/file/"))
    {
        // Emoji pictures are immutable and account-private: their handler sets
        // `private, immutable, max-age=31536000`, which must survive this layer.
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}

fn tool_role(name: &str) -> Role {
    if [
        "publish_post",
        "schedule_post",
        "send_poll",
        "send_media_post",
        "edit_post",
        "reschedule_post",
        "cancel_scheduled_post",
        "create_invite_link",
        "edit_invite_link",
        "revoke_invite_link",
        "approve_suggested_post",
        "decline_suggested_post",
    ]
    .contains(&name)
    {
        Role::Publish
    } else if [
        "save_draft",
        "upload_media",
        "save_post_template",
        "draft_from_template",
        "send_preview_to_me",
        "refresh_bot_api",
        "import_telegram_post",
    ]
    .contains(&name)
    {
        Role::Drafts
    } else {
        Role::Read
    }
}

pub fn tool_visible(state: &AppState, name: &str) -> bool {
    state.settings.mcp_publish || tool_role(name) != Role::Publish
}

pub fn check_tool(state: &AppState, name: &str) -> Result<(), AuthError> {
    if !tool_visible(state, name) {
        return Err(AuthError::new(
            403,
            "публикация через MCP выключена (MCP_PUBLISH=off)",
        ));
    }
    if state.principal.telegram_id.is_some() && ["studio_logs", "refresh_bot_api"].contains(&name) {
        return Err(AuthError::new(403, "доступно только владельцу сервиса"));
    }
    auth::require(&state.principal, tool_role(name))
}
