use super::formula;
use crate::spec::validator::text::{self, address};
use std::collections::BTreeMap;

#[derive(Clone)]
struct Open {
    tag: String,
    sure: usize,
    about: usize,
    bare: bool,
    holder: String,
    p: Option<usize>,
    li: Option<usize>,
    cell: Option<usize>,
    row: Option<usize>,
}
impl Default for Open {
    fn default() -> Self {
        Self {
            tag: String::new(),
            sure: 0,
            about: 0,
            bare: true,
            holder: String::new(),
            p: None,
            li: None,
            cell: None,
            row: None,
        }
    }
}

fn close(stack: &mut Vec<Open>, counts: &mut BTreeMap<String, usize>, index: usize) {
    while stack.len() > index.max(1) {
        if let Some(opened) = stack.pop()
            && let Some(count) = counts.get_mut(&opened.tag)
        {
            *count = count.saturating_sub(1);
        }
    }
}

fn measured(tag: &str) -> bool {
    [
        "blockquote",
        "aside",
        "details",
        "ul",
        "ol",
        "table",
        "b",
        "strong",
        "i",
        "em",
        "u",
        "ins",
        "s",
        "strike",
        "del",
    ]
    .contains(&tag)
}
fn maybe(tag: &str) -> bool {
    [
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "pre",
        "footer",
        "figure",
        "tg-collage",
        "tg-slideshow",
        "tg-thinking",
        "tg-button-row",
        "tg-math-block",
        "video",
        "audio",
        "tg-document",
        "tg-map",
        "a",
        "code",
        "sub",
        "sup",
        "mark",
        "tg-spoiler",
        "tg-emoji",
        "tg-math",
        "tg-time",
        "tg-reference",
    ]
    .contains(&tag)
}
fn holds(tag: &str) -> bool {
    [
        "p",
        "blockquote",
        "aside",
        "li",
        "td",
        "th",
        "summary",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "pre",
        "footer",
        "figcaption",
        "caption",
        "cite",
        "tg-thinking",
        "tg-button",
        "tg-math-block",
    ]
    .contains(&tag)
}
fn ends_p(tag: &str) -> bool {
    [
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "pre",
        "footer",
        "blockquote",
        "aside",
        "details",
        "summary",
        "ul",
        "ol",
        "li",
        "table",
        "tr",
        "td",
        "th",
        "figure",
        "figcaption",
        "hr",
        "tg-collage",
        "tg-slideshow",
        "tg-thinking",
        "tg-button-row",
        "tg-math-block",
        "tg-map",
    ]
    .contains(&tag)
}

pub(crate) fn source(
    source: &str,
    html: bool,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> usize {
    if !html {
        for expression in source.split('$') {
            formula(expression, warnings, "rich_message.markdown");
        }
        return 0;
    }
    let mut stack = vec![Open::default()];
    let mut counts = BTreeMap::<String, usize>::new();
    let mut most = 0;
    let mut about = 0;
    let mut at = 0;
    let mut formula_text = String::new();
    let mut formula_at = None;
    let data = |text: &str,
                stack: &[Open],
                most: &mut usize,
                about: &mut usize,
                formula_text: &mut String,
                formula_at: Option<usize>| {
        if formula_at.is_some() {
            formula_text.push_str(text);
        }
        if !text.trim().is_empty()
            && let Some(top) = stack.last()
        {
            *most = (*most).max(top.sure + usize::from(top.bare));
            *about = (*about).max(top.about + usize::from(top.bare));
        }
    };
    for (start, end) in text::tags(source) {
        data(
            &source[at..start],
            &stack,
            &mut most,
            &mut about,
            &mut formula_text,
            formula_at,
        );
        at = end;
        let raw = &source[start..end];
        let closing = raw.starts_with("</");
        let tag = raw
            .trim_start_matches(['<', '/'])
            .split(|ch: char| ch.is_ascii_whitespace() || ch == '/' || ch == '>')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if closing {
            if counts.get(&tag).copied().unwrap_or(0) > 0
                && let Some(index) = stack.iter().rposition(|item| item.tag == tag)
            {
                close(&mut stack, &mut counts, index);
                if formula_at.is_some_and(|at| at >= index) {
                    formula(&formula_text, warnings, "rich_message.html");
                    formula_text.clear();
                    formula_at = None;
                }
            }
            continue;
        }
        loop {
            let top = stack.last().cloned().unwrap_or_default();
            let target = if ends_p(&tag) { top.p } else { None }
                .or_else(|| if tag == "li" { top.li } else { None })
                .or_else(|| if tag == "tr" { top.row } else { None })
                .or_else(|| {
                    if tag == "td" || tag == "th" {
                        top.cell
                    } else {
                        None
                    }
                });
            if let Some(index) = target {
                close(&mut stack, &mut counts, index);
            } else {
                break;
            }
        }
        let top = stack.last().cloned().unwrap_or_default();
        if [
            "hr",
            "img",
            "video",
            "audio",
            "tg-document",
            "tg-map",
            "tg-math-block",
        ]
        .contains(&tag.as_str())
        {
            about = about.max(top.about + 1);
        }
        if text::attribute(raw, "src", true).is_some_and(|src| src.starts_with("upload:")) {
            errors.push(format!("<{tag} src>: upload: не заменяется в HTML-атрибуте; используйте tg://photo?id=<id> с rich_message.media"));
        }
        if ["a", "tg-button"].contains(&tag.as_str())
            && let Some(written) =
                text::attribute(raw, if tag == "a" { "href" } else { "url" }, false)
        {
            address(
                if tag == "a" { "link" } else { "button" },
                &text::unescape(&written),
                if tag == "a" { Some(&written) } else { None },
                errors,
                warnings,
                &format!("rich_message.html <{tag}>"),
            );
        }
        if [
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source",
            "wbr",
        ]
        .contains(&tag.as_str())
        {
            continue;
        }
        let i = stack.len();
        let (sure, approx) = if tag == "p" {
            (
                top.bare,
                top.bare || !["blockquote", "li"].contains(&top.holder.as_str()),
            )
        } else {
            let sure = measured(&tag);
            (
                sure,
                sure || maybe(&tag) && !(tag == "code" && top.tag == "pre"),
            )
        };
        let holder = tag == "details" || holds(&tag);
        let opened = Open {
            tag: tag.clone(),
            sure: top.sure + usize::from(sure),
            about: top.about + usize::from(approx),
            bare: tag == "details" || (top.bare && !holder),
            holder: if holder { tag.clone() } else { top.holder },
            p: if tag == "p" { Some(i) } else { top.p },
            li: if tag == "li" {
                Some(i)
            } else if tag == "ul" || tag == "ol" {
                None
            } else {
                top.li
            },
            cell: if tag == "td" || tag == "th" {
                Some(i)
            } else if tag == "tr" || tag == "table" {
                None
            } else {
                top.cell
            },
            row: if tag == "tr" {
                Some(i)
            } else if tag == "table" {
                None
            } else {
                top.row
            },
        };
        if ["tg-math", "tg-math-block"].contains(&tag.as_str()) && formula_at.is_none() {
            formula_at = Some(i);
            formula_text.clear();
        }
        *counts.entry(tag).or_default() += 1;
        stack.push(opened);
    }
    data(
        &source[at..],
        &stack,
        &mut most,
        &mut about,
        &mut formula_text,
        formula_at,
    );
    if formula_at.is_some() {
        formula(&formula_text, warnings, "rich_message.html");
    }
    if most > 16 {
        errors.push(format!("rich_message.html: {most} уровней вложенности, лимит 16; у текста вне блока и прямо в <details> ещё свой абзац"));
    } else if about > 16 {
        warnings.push(format!("rich_message.html: около {about} уровней вложенности, лимит 16; элементы без замера считаются приблизительно"));
    }
    most
}
