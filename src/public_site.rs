//! Fixed public inventory: user input never becomes a resource filename.

pub const GUIDES: &[&str] = &[
    "telegram-tables",
    "telegram-markdown",
    "scheduled-posts",
    "ai-agents",
];
pub const PREVIEW_PATHS: &[&str] = &["/landing", "/landing/en/"];
pub const AGENT_PATHS: &[&str] = &[
    "/llms.txt",
    "/llms-full.txt",
    "/agents.txt",
    "/agents.json",
    "/agents.md",
    "/skills/richpost/SKILL.md",
];

pub fn page_paths() -> Vec<String> {
    let mut paths = vec![
        "/".to_owned(),
        "/en/".to_owned(),
        "/features/".to_owned(),
        "/en/features/".to_owned(),
    ];
    for prefix in ["", "/en"] {
        paths.extend(
            GUIDES
                .iter()
                .map(|slug| format!("{prefix}/features/{slug}/")),
        );
    }
    paths.sort();
    paths
}

/// Only a renderer of trusted editorial HTML may attach this CSP allowance.
/// No request header or user post is allowed to supply a script hash.
#[derive(Clone)]
pub struct PublicHtml {
    pub structured_data_hash: Option<String>,
}

pub fn is_page(path: &str) -> bool {
    PREVIEW_PATHS.contains(&path) || page_paths().iter().any(|entry| entry == path)
}

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
