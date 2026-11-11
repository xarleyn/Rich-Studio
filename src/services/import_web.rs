//! Bounded public Telegram widget, fetched only at a URL derived from a strict reference.

use crate::errors::ImportError;
use crate::services::{import_links::MessageLink, media};
use crate::spec::import_content::safe_link;
use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;

pub const MAX_WIDGET_BYTES: usize = 2 * 1024 * 1024;
pub struct Widget {
    pub root: Vec<Value>,
    pub media_urls: Vec<String>,
    pub warnings: Vec<String>,
}

fn unavailable() -> ImportError {
    ImportError {
        status: 409,
        detail: json!({"code":"telegram_connection_required","message":"Telegram не отдаёт содержимое этого поста через веб-виджет. Подключите Telegram для импорта или перешлите сообщение боту студии."}),
    }
}

pub fn parse_widget(body: &str, expected: &str) -> Result<Widget, ImportError> {
    if body.len() > MAX_WIDGET_BYTES {
        return Err(ImportError::new(
            413,
            "Предпросмотр Telegram превышает лимит 2 МБ.",
        ));
    }
    let document = Html::parse_document(body);
    let selector = Selector::parse(".tgme_widget_message")
        .map_err(|_| ImportError::new(500, "widget selector"))?;
    let root = document
        .select(&selector)
        .find(|element| element.value().attr("data-post") == Some(expected))
        .ok_or_else(unavailable)?;
    let descendants = Selector::parse("*").map_err(|_| ImportError::new(500, "widget selector"))?;
    let photos = Regex::new(r#"background-image:\s*url\(['\"]?(https://[^)'\"\s]+)"#)
        .map_err(|_| ImportError::new(500, "widget image pattern"))?;
    let mut widget = Widget {
        root: Vec::new(),
        media_urls: Vec::new(),
        warnings: Vec::new(),
    };
    for element in root.select(&descendants) {
        let class = element.value().classes().collect::<Vec<_>>();
        if class.contains(&"message_media_not_supported")
            || class.contains(&"tgme_widget_message_error")
        {
            return Err(unavailable());
        }
        if class.contains(&"tgme_widget_message_photo_wrap")
            && let Some(capture) = photos.captures(element.value().attr("style").unwrap_or(""))
        {
            widget.media_urls.push(capture[1].to_owned());
        }
        if ["video", "audio"].contains(&element.value().name()) {
            widget.warnings.push(
                "Видео/аудио виджета не импортировано. Для полного импорта подключите Telegram."
                    .into(),
            );
        }
        if class.contains(&"tgme_widget_message_text") {
            widget.root.extend(children(element, 0)?);
        }
    }
    if widget.root.is_empty() && widget.media_urls.is_empty() {
        return Err(unavailable());
    }
    Ok(widget)
}

fn children(element: ElementRef<'_>, depth: usize) -> Result<Vec<Value>, ImportError> {
    if depth > 40 {
        return Err(ImportError::new(
            413,
            "Слишком глубокое содержимое сообщения.",
        ));
    }
    let mut output = Vec::new();
    for child in element.children() {
        match child.value() {
            Node::Text(text) => output.push(json!(text.text.to_string())),
            Node::Element(_) => {
                if let Some(element) = ElementRef::wrap(child) {
                    let name = element.value().name();
                    if ["script", "style", "img", "input", "hr", "source", "wbr"].contains(&name) {
                        continue;
                    }
                    if name == "br" {
                        output.push(json!("\n"));
                        continue;
                    }
                    let content = children(element, depth + 1)?;
                    let kind = match name {
                        "b" | "strong" => Some("bold"),
                        "i" | "em" => Some("italic"),
                        "u" => Some("underline"),
                        "s" | "del" => Some("strikethrough"),
                        "code" | "pre" => Some("code"),
                        "tg-spoiler" => Some("spoiler"),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        output.push(json!({"type":kind,"text":content}));
                    } else if name == "a"
                        && !safe_link(element.value().attr("href").unwrap_or("")).is_empty()
                    {
                        output.push(json!({"type":"url","url":element.value().attr("href").unwrap_or(""),"text":content}));
                    } else {
                        output.push(json!(content));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(output)
}

pub async fn fetch_widget(link: &MessageLink) -> Result<Widget, ImportError> {
    let username = link.username.as_deref().ok_or_else(|| {
        ImportError::new(
            409,
            "Для приватной ссылки требуется собственное подключение Telegram.",
        )
    })?;
    let url = format!(
        "https://t.me/{username}/{}?embed=1{}",
        link.message_id,
        if link.single { "&single=1" } else { "" }
    );
    // reqwest honors the same environment proxy as other media requests. When DNS is available,
    // validate every answer and pin the chosen address to prevent a second DNS lookup.
    let client = public_client("https", "t.me", 443, 15).await?;
    let mut response = client.get(url).send().await.map_err(|_| {
        ImportError::new(502, "Telegram не отдал публичный предпросмотр сообщения.")
    })?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(ImportError::new(
            502,
            "Telegram не отдал публичный предпросмотр сообщения.",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ImportError::new(502, "Telegram прервал передачу предпросмотра."))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_WIDGET_BYTES {
            return Err(ImportError::new(
                413,
                "Предпросмотр Telegram превышает лимит 2 МБ.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let expected = format!("{username}/{}", link.message_id);
    tokio::task::spawn_blocking(move || parse_widget(&String::from_utf8_lossy(&bytes), &expected))
        .await
        .map_err(|_| ImportError::new(500, "Обработка предпросмотра прервана."))?
}

async fn public_client(
    scheme: &str,
    host: &str,
    port: u16,
    timeout: u64,
) -> Result<reqwest::Client, ImportError> {
    let proxied = media::system_proxy(scheme, host);
    let addresses = match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => addresses.collect::<Vec<_>>(),
        Err(_) if proxied => vec![],
        Err(_) => return Err(ImportError::new(502, "Адрес Telegram не найден.")),
    };
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::none());
    if !addresses.is_empty() {
        let ip = media::public_address(
            &addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>(),
            host,
        )
        .map_err(|_| ImportError::new(422, "Локальные адреса медиа недоступны."))?;
        if !proxied {
            builder = builder.resolve(host, SocketAddr::new(ip, port));
        }
    }
    builder
        .build()
        .map_err(|_| ImportError::new(502, "Не удалось настроить скачивание Telegram."))
}

pub async fn fetch_media(raw: &str) -> Result<reqwest::Response, ImportError> {
    let mut url =
        url::Url::parse(raw).map_err(|_| ImportError::new(422, "Неверная ссылка на медиа."))?;
    for _ in 0..=5 {
        if !["http", "https"].contains(&url.scheme())
            || url.username() != ""
            || url.password().is_some()
        {
            return Err(ImportError::new(422, "Неверная ссылка на медиа."));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ImportError::new(422, "Неверная ссылка на медиа."))?
            .to_owned();
        let port = url.port_or_known_default().unwrap_or(443);
        if media::internal_name(&host) {
            return Err(ImportError::new(422, "Локальные адреса медиа недоступны."));
        }
        let client = public_client(url.scheme(), &host, port, 60).await?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| ImportError::new(502, "Не удалось скачать медиа Telegram."))?;
        if response.status().is_redirection() {
            let target = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ImportError::new(502, "Пустое перенаправление медиа."))?;
            url = url
                .join(target)
                .map_err(|_| ImportError::new(422, "Неверная ссылка на медиа."))?;
            continue;
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(ImportError::new(502, "Не удалось скачать медиа Telegram."));
        }
        return Ok(response);
    }
    Err(ImportError::new(
        502,
        "Слишком много перенаправлений медиа.",
    ))
}
