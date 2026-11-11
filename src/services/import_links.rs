//! Strict message references. A supplied URL is never a fetch destination.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::errors::ImportError;

const MESSAGE: &str =
    "Нужна ссылка https://t.me/канал/сообщение или https://t.me/канал/тема/сообщение.";
const RESERVED: &[&str] = &[
    "c",
    "s",
    "share",
    "joinchat",
    "addstickers",
    "proxy",
    "socks",
    "login",
    "addlist",
    "boost",
];

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct MessageLink {
    pub username: Option<String>,
    pub channel_id: Option<i64>,
    pub message_id: i32,
    pub topic_id: Option<i32>,
    pub single: bool,
}

impl MessageLink {
    pub fn url(&self) -> String {
        let peer = self
            .username
            .clone()
            .unwrap_or_else(|| format!("c/{}", self.channel_id.unwrap_or_default()));
        let topic = self
            .topic_id
            .map_or_else(String::new, |id| format!("{id}/"));
        format!(
            "https://t.me/{peer}/{topic}{}{}",
            self.message_id,
            if self.single { "?single" } else { "" }
        )
    }

    pub fn describe(&self) -> Value {
        json!({"username":self.username,"channel_id":self.channel_id,"message_id":self.message_id,"topic_id":self.topic_id,"single":self.single,"url":self.url()})
    }
}

fn bad() -> ImportError {
    ImportError::new(
        422,
        format!("{MESSAGE} Параметры комментариев и других действий не поддерживаются."),
    )
}

fn decimal(raw: &str, length: usize) -> bool {
    !raw.is_empty()
        && raw.len() <= length
        && !raw.starts_with('0')
        && raw.bytes().all(|ch| ch.is_ascii_digit())
}

fn id(raw: &str) -> Result<i32, ImportError> {
    if !decimal(raw, 10) {
        return Err(bad());
    }
    raw.parse::<i32>().map_err(|_| bad())
}

pub fn parse_link(raw: &str) -> Result<MessageLink, ImportError> {
    if raw.chars().count() > 2048
        || raw
            .trim()
            .chars()
            .any(|ch| ch <= '\u{20}' || ch == '\u{7f}' || ch == '\\' || ch == '%')
    {
        return Err(ImportError::new(422, MESSAGE));
    }
    let raw = raw.trim();
    let url = if raw.starts_with("t.me/") {
        format!("https://{raw}")
    } else {
        raw.to_owned()
    };
    // Preserve the authority/path verbatim: URL normalization would accept :443 and ../.
    let rest = url.strip_prefix("https://").ok_or_else(bad)?;
    let (authority, rest) = rest.split_once('/').ok_or_else(bad)?;
    if !authority.eq_ignore_ascii_case("t.me") || rest.contains('#') {
        return Err(bad());
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = path.strip_suffix('/').unwrap_or(path);
    let parts = path.split('/').collect::<Vec<_>>();
    let mut link = MessageLink {
        username: None,
        channel_id: None,
        message_id: 0,
        topic_id: None,
        single: false,
    };
    let ids = if parts.first() == Some(&"c") {
        if !(3..=4).contains(&parts.len()) || !decimal(parts[1], 13) {
            return Err(bad());
        }
        link.channel_id = Some(parts[1].parse().map_err(|_| bad())?);
        &parts[2..]
    } else {
        if !(2..=3).contains(&parts.len()) {
            return Err(bad());
        }
        let username = parts[0];
        if !(4..=32).contains(&username.len())
            || !username.as_bytes()[0].is_ascii_alphabetic()
            || !username
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        {
            return Err(bad());
        }
        let username = username.to_ascii_lowercase();
        if RESERVED.contains(&username.as_str()) {
            return Err(bad());
        }
        link.username = Some(username);
        &parts[1..]
    };
    link.message_id = id(ids[ids.len() - 1])?;
    if ids.len() == 2 {
        link.topic_id = Some(id(ids[0])?);
    }
    let mut seen = BTreeSet::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !seen.insert(key) {
            return Err(bad());
        }
        match key {
            "single" if value.is_empty() || value == "1" => link.single = true,
            "thread" => {
                let topic = id(value)?;
                if link.topic_id.is_some_and(|old| old != topic) {
                    return Err(bad());
                }
                link.topic_id = Some(topic);
            }
            "t" if (1..=20).contains(&value.len())
                && value
                    .bytes()
                    .all(|ch| ch.is_ascii_digit() || b"hms:".contains(&ch)) => {}
            _ => return Err(bad()),
        }
    }
    Ok(link)
}
