use serde_json::{Value, json};

use crate::errors::ImportError;

use super::inline::{escape_html, plain, tag, text_html};
use super::{array, flag, invalid, string, text};

fn audit(value: &Value, depth: usize, warnings: &mut Vec<String>) -> Result<(), ImportError> {
    if depth > 40 {
        return Err(invalid("Слишком глубокая структура содержимого."));
    }
    if let Some(items) = value.as_array() {
        for item in items {
            audit(item, depth + 1, warnings)?;
        }
    } else if let Some(items) = value.as_object() {
        let kind = text(&value["type"]);
        if !kind.is_empty()
            && tag(kind).is_none()
            && ![
                "url",
                "custom_emoji",
                "mathematical_expression",
                "anchor",
                "paragraph",
                "heading",
                "footer",
                "thinking",
                "pre",
                "divider",
                "map",
                "blockquote",
                "expandable_blockquote",
                "pullquote",
                "list",
                "details",
                "collage",
                "slideshow",
                "table",
                "photo",
                "video",
                "audio",
                "animation",
                "voice_note",
                "document",
                "1",
                "a",
                "A",
                "i",
                "I",
            ]
            .contains(&kind)
        {
            warnings.push(format!(
                "HTML/Markdown: оформление {kind} упрощено; проверьте визуальный оригинал."
            ));
        }
        for child in items.values() {
            if child.is_array() || child.is_object() {
                audit(child, depth + 1, warnings)?;
            }
        }
    }
    Ok(())
}

struct Renderer<'a> {
    media: Vec<Value>,
    warnings: &'a mut Vec<String>,
}

impl Renderer<'_> {
    fn caption(&self, value: &Value) -> String {
        if value.is_null() {
            return String::new();
        }
        let credit = text_html(&value["credit"]);
        format!(
            "<figcaption>{}{}</figcaption>",
            text_html(&value["text"]),
            if credit.is_empty() {
                String::new()
            } else {
                format!("<cite>{credit}</cite>")
            }
        )
    }
    fn blocks(&mut self, values: &Value) -> String {
        array(values)
            .iter()
            .map(|value| self.render(value))
            .collect()
    }
    fn render(&mut self, b: &Value) -> String {
        let kind = text(&b["type"]);
        let content = text_html(&b["text"]);
        match kind {
            "paragraph" | "heading" | "footer" | "thinking" => {
                let tag = if kind == "heading" {
                    format!("h{}", string(&b["size"]))
                } else if kind == "footer" {
                    "footer".to_owned()
                } else {
                    "p".to_owned()
                };
                format!("<{tag}>{content}</{tag}>")
            }
            "pre" => format!(
                "<pre><code class=\"language-{}\">{}</code></pre>",
                escape_html(text(&b["language"])),
                escape_html(&plain(&b["text"]))
            ),
            "map" => format!(
                "<figure><tg-map lat=\"{:?}\" long=\"{:?}\" zoom=\"{}\"></tg-map>{}</figure>",
                b["location"]["latitude"].as_f64().unwrap_or(0.0),
                b["location"]["longitude"].as_f64().unwrap_or(0.0),
                b["zoom"].as_i64().unwrap_or(12),
                self.caption(&b["caption"])
            ),
            "divider" => "<hr>".to_owned(),
            "anchor" => format!("<a name=\"{}\"></a>", escape_html(text(&b["name"]))),
            "mathematical_expression" => format!(
                "<tg-math-block>{}</tg-math-block>",
                escape_html(text(&b["expression"]))
            ),
            "blockquote" | "expandable_blockquote" | "pullquote" => {
                let tag = if kind == "pullquote" {
                    "aside"
                } else {
                    "blockquote"
                };
                let attr = if kind == "expandable_blockquote" {
                    " expandable"
                } else {
                    ""
                };
                let body = self.blocks(&b["blocks"]);
                let credit = text_html(&b["credit"]);
                format!(
                    "<{tag}{attr}>{}{}</{tag}>",
                    if body.is_empty() { content } else { body },
                    if credit.is_empty() {
                        String::new()
                    } else {
                        format!("<cite>{credit}</cite>")
                    }
                )
            }
            "list" => {
                let ordered = array(&b["items"])
                    .iter()
                    .any(|item| item.get("value").is_some());
                let tag = if ordered { "ol" } else { "ul" };
                let mut rows = String::new();
                for item in array(&b["items"]) {
                    let attr = if item.get("value").is_some() {
                        format!(
                            " value=\"{}\" type=\"{}\"",
                            string(&item["value"]),
                            escape_html(item["type"].as_str().unwrap_or("1"))
                        )
                    } else {
                        String::new()
                    };
                    let check = if flag(&item["has_checkbox"]) {
                        format!(
                            "<input type=\"checkbox\"{}>",
                            if flag(&item["is_checked"]) {
                                " checked"
                            } else {
                                ""
                            }
                        )
                    } else {
                        String::new()
                    };
                    rows.push_str(&format!(
                        "<li{attr}>{check}{}</li>",
                        self.blocks(&item["blocks"])
                    ));
                }
                format!("<{tag}>{rows}</{tag}>")
            }
            "details" => format!(
                "<details{}><summary>{}</summary>{}</details>",
                if flag(&b["is_open"]) { " open" } else { "" },
                text_html(&b["summary"]),
                self.blocks(&b["blocks"])
            ),
            "collage" | "slideshow" => format!(
                "<tg-{kind}>{}{}</tg-{kind}>",
                self.blocks(&b["blocks"]),
                self.caption(&b["caption"])
            ),
            "table" => {
                let mut rows = String::new();
                for row in array(&b["cells"]) {
                    let mut cells = String::new();
                    for c in array(row) {
                        if c.get("text").is_none() {
                            continue;
                        }
                        let tag = if flag(&c["is_header"]) { "th" } else { "td" };
                        let mut attrs = String::new();
                        for key in ["align", "valign", "colspan", "rowspan"] {
                            if c.get(key).is_some() {
                                attrs.push_str(&format!(
                                    " {key}=\"{}\"",
                                    escape_html(&string(&c[key]))
                                ));
                            }
                        }
                        cells.push_str(&format!("<{tag}{attrs}>{}</{tag}>", text_html(&c["text"])));
                    }
                    rows.push_str(&format!("<tr>{cells}</tr>"));
                }
                let attrs = ["bordered", "striped", "compact"]
                    .iter()
                    .filter(|key| flag(&b[format!("is_{key}")]))
                    .map(|key| format!(" {key}"))
                    .collect::<String>();
                format!(
                    "<table{attrs}><caption>{}</caption>{rows}</table>",
                    text_html(&b["caption"])
                )
            }
            "photo" | "video" | "audio" | "animation" | "voice_note" | "document" => {
                let item = &b[kind];
                let mid = format!("import{}", self.media.len() + 1);
                self.media.push(json!({"id":mid,"media":item}));
                let scheme = if kind == "voice_note" { "voice" } else { kind };
                let tag = match kind {
                    "photo" => "img",
                    "animation" => "video",
                    "voice_note" => "audio",
                    "document" => "tg-document",
                    _ => kind,
                };
                let attrs = if flag(&item["has_spoiler"]) {
                    " tg-spoiler"
                } else {
                    ""
                };
                format!(
                    "<figure><{tag} src=\"tg://{scheme}?id={mid}\"{attrs}>{}{}{}</figure>",
                    if tag == "img" {
                        String::new()
                    } else {
                        format!("</{tag}>")
                    },
                    self.caption(&b["caption"]),
                    ""
                )
            }
            _ => {
                self.warnings.push(format!("HTML/Markdown: блок {kind} заменён отметкой; исходник сохранён в визуальном режиме."));
                format!(
                    "<p>{}</p>",
                    escape_html(&format!("[Блок {kind}: доступен в визуальном режиме]"))
                )
            }
        }
    }
}

pub fn representations(
    blocks: &[Value],
    warnings: &mut Vec<String>,
) -> Result<(String, String, Vec<Value>), ImportError> {
    audit(&json!(blocks), 0, warnings)?;
    let mut renderer = Renderer {
        media: Vec::new(),
        warnings,
    };
    let html_parts = blocks
        .iter()
        .map(|block| renderer.render(block))
        .collect::<Vec<_>>();
    let mut markdown = Vec::new();
    for (block, html) in blocks.iter().zip(&html_parts) {
        if ["paragraph", "heading"].contains(&text(&block["type"])) && block["text"].is_string() {
            let mut escaped = String::new();
            for ch in text(&block["text"]).chars() {
                if "\\`*_{}[]()#+.!|>~=$-".contains(ch) {
                    escaped.push('\\');
                }
                escaped.push(ch);
            }
            let escaped = escape_html(&escaped).replace('\n', "  \n");
            let prefix = if text(&block["type"]) == "heading" {
                format!(
                    "{} ",
                    "#".repeat(block["size"].as_u64().unwrap_or(1).min(6) as usize)
                )
            } else {
                String::new()
            };
            markdown.push(format!("{prefix}{escaped}"));
        } else {
            markdown.push(html.clone());
        }
    }
    Ok((html_parts.join("\n"), markdown.join("\n\n"), renderer.media))
}
