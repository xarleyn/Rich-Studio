use std::collections::BTreeMap;
use std::ops::Range;

use regex::Regex;
use serde_json::Value;

use crate::errors::StoreError;

use super::{TemplateError, invalid};

pub(super) fn visit<F>(
    value: &Value,
    path: &mut Vec<String>,
    depth: usize,
    variable: &Regex,
    action: &mut F,
) -> Result<(), TemplateError>
where
    F: FnMut(&str, &[String]) -> Result<(), TemplateError>,
{
    if depth > 200 {
        return invalid("Слишком большая вложенность шаблона.");
    }
    match value {
        Value::String(text) => action(text, path),
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(index.to_string());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        Value::Object(items) => {
            for (key, child) in items {
                if variable.is_match(key) {
                    return invalid(
                        "Переменные допускаются в значениях, но не в именах полей JSON.",
                    );
                }
                path.push(key.clone());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Mode {
    Plain,
    Html,
    Markdown,
    MarkdownUrl,
}

/// The source determines the escaping context before any value is inserted.
pub(super) struct Modes {
    mode: Mode,
    destinations: Vec<Range<usize>>,
}

impl Modes {
    pub(super) fn new(path: &[String], source: &str) -> Self {
        let mode = Mode::for_path(path);
        Self {
            mode,
            destinations: if mode == Mode::Markdown {
                markdown_destinations(source)
            } else {
                Vec::new()
            },
        }
    }
    pub(super) fn at(&self, start: usize, end: usize) -> Mode {
        let index = self
            .destinations
            .partition_point(|range| range.start <= start);
        if index
            .checked_sub(1)
            .and_then(|index| self.destinations.get(index))
            .is_some_and(|range| end <= range.end)
        {
            Mode::MarkdownUrl
        } else {
            self.mode
        }
    }
}

fn destination_end(source: &str, start: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    let mut at = start;
    while at < limit {
        if bytes[at] == b'\\' {
            at = (at + 2).min(limit);
            continue;
        }
        if bytes[at].is_ascii_whitespace() {
            break;
        }
        at += 1;
    }
    at
}

fn matching_parentheses(source: &str, tags: &[(usize, usize)]) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut closing = vec![usize::MAX; bytes.len()];
    let mut stack = Vec::new();
    let mut tag_index = 0;
    let mut at = 0;
    while at < bytes.len() {
        if let Some(&(start, end)) = tags.get(tag_index) {
            if at >= end {
                tag_index += 1;
                continue;
            }
            if at >= start {
                at = end;
                tag_index += 1;
                continue;
            }
        }
        if bytes[at] == b'\\' {
            at = (at + 2).min(bytes.len());
            continue;
        }
        if bytes[at] == b'(' {
            stack.push(at);
        } else if bytes[at] == b')'
            && let Some(opening) = stack.pop()
        {
            closing[opening] = at;
        }
        at += 1;
    }
    closing
}

/// Native Markdown link/image destinations and reference definitions. HTML attributes
/// keep their separate entity-decoding semantics and must not be mistaken for Markdown.
pub(super) fn markdown_destinations(source: &str) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let tags = crate::spec::validator::text::tags(source);
    let parentheses = matching_parentheses(source, &tags);
    let mut tag_index = 0;
    let mut brackets = Vec::new();
    let mut ranges = Vec::new();
    let mut at = 0;
    let mut line_start = 0;
    while at < bytes.len() {
        if let Some(&(start, end)) = tags.get(tag_index) {
            if at >= end {
                tag_index += 1;
                continue;
            }
            if at >= start {
                if let Some(newline) = source[at..end].rfind('\n') {
                    line_start = at + newline + 1;
                }
                at = end;
                tag_index += 1;
                continue;
            }
        }
        if bytes[at] == b'\\' {
            at = (at + 2).min(bytes.len());
            continue;
        }
        match bytes[at] {
            b'\n' => line_start = at + 1,
            b'[' => brackets.push((at, line_start)),
            b']' => {
                if let Some((opening, opening_line)) = brackets.pop() {
                    let inline = bytes.get(at + 1) == Some(&b'(');
                    let closing = parentheses
                        .get(at + 1)
                        .copied()
                        .filter(|closing| *closing != usize::MAX);
                    let prefix = &source[opening_line..opening];
                    let reference = bytes.get(opening + 1) != Some(&b'^')
                        && bytes.get(at + 1) == Some(&b':')
                        && prefix.len() <= 3
                        && prefix.bytes().all(|byte| byte == b' ');
                    if inline && closing.is_some() || reference {
                        let limit = closing.unwrap_or(bytes.len());
                        let mut start = at + 2;
                        while start < limit && bytes.get(start).is_some_and(u8::is_ascii_whitespace)
                        {
                            start += 1;
                        }
                        let end = destination_end(source, start, limit);
                        if start < end {
                            ranges.push(start..end);
                            let after = if inline { limit + 1 } else { end };
                            if let Some(newline) = source[at..after].rfind('\n') {
                                line_start = at + newline + 1;
                            }
                            at = after;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        at += 1;
    }
    ranges.sort_unstable_by_key(|range| range.start);
    ranges
}

impl Mode {
    pub(super) fn for_path(path: &[String]) -> Self {
        if path.len() == 2 && path[0] == "callbacks" {
            return Self::Plain;
        }
        match path.last().map(String::as_str) {
            Some("html") => Self::Html,
            Some("markdown") => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

pub(super) fn escape(text: &str, mode: Mode) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match (mode, ch) {
            (Mode::Html, '&') => output.push_str("&amp;"),
            (Mode::Html, '<') => output.push_str("&lt;"),
            (Mode::Html, '>') => output.push_str("&gt;"),
            (Mode::Html, '"') => output.push_str("&quot;"),
            (Mode::Html, '\'') => output.push_str("&#x27;"),
            (Mode::Markdown, ch) if "&<>\"'\\`*_{}[]()#+.!|~=$-".contains(ch) => {
                output.push_str(&format!("&#{};", u32::from(ch)))
            }
            (Mode::MarkdownUrl, ch) if "`*_{}[]()<>\"'\\|~!".contains(ch) => {
                for byte in ch.to_string().bytes() {
                    output.push_str(&format!("%{byte:02X}"));
                }
            }
            _ => output.push(ch),
        }
    }
    output
}

type Replacements = BTreeMap<(Mode, String), (String, usize)>;

pub(super) fn substitute(
    value: &mut Value,
    path: &mut Vec<String>,
    variable: &Regex,
    replacements: &Replacements,
) -> Result<(), TemplateError> {
    match value {
        Value::String(text) => {
            let modes = Modes::new(path, text);
            let mut replaced = String::new();
            let mut position = 0;
            for capture in variable.captures_iter(text) {
                let matched = capture
                    .get(0)
                    .ok_or_else(|| StoreError::Migration("missing variable match".to_owned()))?;
                replaced.push_str(&text[position..matched.start()]);
                let item = replacements
                    .get(&(
                        modes.at(matched.start(), matched.end()),
                        capture[1].to_owned(),
                    ))
                    .ok_or_else(|| {
                        TemplateError::Invalid(format!("Неизвестное поле: {}", &capture[1]))
                    })?;
                replaced.push_str(&item.0);
                position = matched.end();
            }
            replaced.push_str(&text[position..]);
            *text = replaced;
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        Value::Object(items) => {
            for (key, child) in items {
                path.push(key.clone());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}
