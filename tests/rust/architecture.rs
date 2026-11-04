//! Dependency direction for the single Rust crate. This scans Rust paths rather
//! than text so documentation, payload strings and grouped imports do not hide
//! violations. No parser dependency is needed for this small import contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const LAYERS: &[&str] = &["api", "mcp_tools", "services", "store", "telegram", "spec"];
const SHARED: &[&str] = &[
    "settings",
    "account_context",
    "errors",
    "hex",
    "logs",
    "metrics",
];

#[derive(Debug)]
struct Token {
    text: String,
    line: usize,
}

fn literal_end(chars: &[char], start: usize) -> Option<usize> {
    let mut quote = start;
    if matches!(chars[quote], 'b' | 'c') {
        quote += 1;
    }
    if chars.get(quote) == Some(&'r') {
        quote += 1;
        let hashes = quote;
        while chars.get(quote) == Some(&'#') {
            quote += 1;
        }
        if chars.get(quote) != Some(&'"') {
            return None;
        }
        let count = quote - hashes;
        for end in quote + 1..chars.len() {
            if chars[end] == '"'
                && chars
                    .get(end + 1..end + count + 1)
                    .is_some_and(|tail| tail.iter().all(|c| *c == '#'))
            {
                return Some(end + count + 1);
            }
        }
        return Some(chars.len());
    }
    let delimiter = *chars.get(quote)?;
    if delimiter != '"'
        && !(delimiter == '\''
            && (chars.get(quote + 1) == Some(&'\\') || chars.get(quote + 2) == Some(&'\'')))
    {
        return None;
    }
    let mut end = quote + 1;
    while end < chars.len() {
        if chars[end] == '\\' {
            end += 2;
        } else if chars[end] == delimiter {
            return Some(end + 1);
        } else {
            end += 1;
        }
    }
    Some(chars.len())
}

fn tokens(source: &str) -> Vec<Token> {
    let chars: Vec<_> = source.chars().collect();
    let mut result = Vec::new();
    let (mut at, mut line) = (0, 1);
    while at < chars.len() {
        let start = at;
        let mut ignored = false;
        if chars.get(at..at + 2) == Some(&['/', '/']) {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
            ignored = true;
        } else if chars.get(at..at + 2) == Some(&['/', '*']) {
            at += 2;
            let mut depth = 1;
            while at < chars.len() && depth > 0 {
                match chars.get(at..at + 2) {
                    Some(['/', '*']) => {
                        depth += 1;
                        at += 2;
                    }
                    Some(['*', '/']) => {
                        depth -= 1;
                        at += 2;
                    }
                    _ => at += 1,
                }
            }
            ignored = true;
        } else if let Some(end) = literal_end(&chars, at) {
            at = end;
            ignored = true;
        } else if chars[at].is_whitespace() {
            at += 1;
            ignored = true;
        } else if chars.get(at..at + 2) == Some(&[':', ':']) {
            at += 2;
        } else if chars[at].is_alphabetic() || chars[at] == '_' {
            at += 1;
            // A raw identifier is still the same module name (r#api).
            if chars[start] == 'r' && chars.get(at) == Some(&'#') {
                at += 1;
            }
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                at += 1;
            }
        } else {
            at += 1;
        }
        if !ignored {
            let text: String = chars[start..at].iter().collect();
            result.push(Token {
                text: text.strip_prefix("r#").unwrap_or(&text).to_owned(),
                line,
            });
        }
        line += chars[start..at].iter().filter(|c| **c == '\n').count();
    }
    result
}

#[derive(Clone, Debug)]
struct Dependency {
    path: Vec<String>,
    line: usize,
    alias: Option<String>,
}

fn item(tokens: &[Token], at: usize) -> &str {
    tokens.get(at).map_or("", |token| token.text.as_str())
}

fn use_tree(tokens: &[Token], at: &mut usize, prefix: &[String], output: &mut Vec<Dependency>) {
    let mut path = prefix.to_vec();
    let line = tokens.get(*at).map_or(1, |token| token.line);
    if item(tokens, *at) == "::" {
        *at += 1;
    }
    loop {
        if item(tokens, *at) == "{" {
            *at += 1;
            while !matches!(item(tokens, *at), "}" | "") {
                use_tree(tokens, at, &path, output);
                if item(tokens, *at) == "," {
                    *at += 1;
                } else {
                    break;
                }
            }
            if item(tokens, *at) == "}" {
                *at += 1;
            }
            return;
        }
        let next = item(tokens, *at);
        if matches!(next, "" | ";" | "," | "}") {
            return;
        }
        path.push(next.to_owned());
        *at += 1;
        if item(tokens, *at) != "::" {
            break;
        }
        *at += 1;
    }
    if path.last().is_some_and(|part| part == "self") && path.len() > 1 {
        path.pop();
    }
    let alias = if item(tokens, *at) == "as" {
        *at += 1;
        let alias = item(tokens, *at).to_owned();
        *at += 1;
        Some(alias)
    } else {
        path.last().filter(|part| part.as_str() != "*").cloned()
    };
    output.push(Dependency { path, line, alias });
}

#[derive(Clone)]
struct Resolved {
    internal: bool,
    path: Vec<String>,
}

fn resolve(path: &[String], module: &[String], aliases: &BTreeMap<String, Resolved>) -> Resolved {
    let Some(root) = path.first() else {
        return Resolved {
            internal: true,
            path: Vec::new(),
        };
    };
    match root.as_str() {
        "crate" | "rich_studio" => Resolved {
            internal: true,
            path: path[1..].to_vec(),
        },
        "self" | "super" => {
            let mut base = module.to_vec();
            let mut count = 0;
            while path
                .get(count)
                .is_some_and(|part| part == "super" || part == "self")
            {
                if path[count] == "super" {
                    base.pop();
                }
                count += 1;
            }
            base.extend_from_slice(&path[count..]);
            Resolved {
                internal: true,
                path: base,
            }
        }
        _ => {
            if let Some(alias) = aliases.get(root) {
                let mut resolved = alias.clone();
                resolved.path.extend_from_slice(&path[1..]);
                resolved
            } else {
                Resolved {
                    internal: false,
                    path: path.to_vec(),
                }
            }
        }
    }
}

fn allowed(file: &str, layer: &str, dependency: &Resolved) -> bool {
    let path: Vec<_> = dependency.path.iter().map(String::as_str).collect();
    let Some(root) = path.first() else {
        return true;
    };
    let adapter = matches!(layer, "api" | "mcp_tools");
    if !dependency.internal {
        return adapter
            || !["axum", "axum_extra", "rmcp", "tower", "tower_http", "utoipa"].contains(root)
            // Only schema derives on shared input structs, never HTTP handlers.
            || (file == "services/templates.rs" && path == ["utoipa", "ToSchema"]);
    }
    if SHARED.contains(root) || *root == layer {
        return true;
    }
    match *root {
        "api" | "mcp_tools" => {
            // Composition belongs to the top-level router, not reusable services.
            file == "api/mod.rs" && *root == "mcp_tools"
        }
        "services" => adapter,
        "store" | "telegram" | "spec" => {
            adapter || layer == "services"
                // Final Telegram authorization checks the account's channel cache.
                || (file == "telegram/client.rs" && path.starts_with(&["store", "channels"]))
        }
        "AppState" => adapter || layer == "services",
        "security" | "public_site" => adapter,
        "runtime" => adapter || (file == "services/health.rs" && path == ["runtime", "static_dir"]),
        _ => false,
    }
}

fn violations(file: &str, source: &str) -> Vec<String> {
    let module: Vec<_> = file
        .trim_end_matches(".rs")
        .split('/')
        .filter(|part| *part != "mod")
        .map(str::to_owned)
        .collect();
    let layer = module[0].as_str();
    let tokens = tokens(source);
    let mut imports = Vec::new();
    let mut paths = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        if item(&tokens, at) == "use" {
            at += 1;
            use_tree(&tokens, &mut at, &[], &mut imports);
            while at < tokens.len() && item(&tokens, at) != ";" {
                at += 1;
            }
        } else if item(&tokens, at + 1) == "::" {
            let line = tokens[at].line;
            let mut path = vec![tokens[at].text.clone()];
            while item(&tokens, at + 1) == "::"
                && tokens.get(at + 2).is_some_and(|token| {
                    token
                        .text
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_')
                })
            {
                at += 2;
                path.push(tokens[at].text.clone());
            }
            paths.push(Dependency {
                path,
                line,
                alias: None,
            });
        }
        at += 1;
    }
    let mut aliases = BTreeMap::new();
    for dependency in &imports {
        if let Some(alias) = &dependency.alias {
            aliases.insert(alias.clone(), resolve(&dependency.path, &module, &aliases));
        }
    }
    let mut problems = BTreeSet::new();
    for dependency in imports.iter().chain(&paths) {
        let resolved = resolve(&dependency.path, &module, &aliases);
        if !allowed(file, layer, &resolved) {
            problems.insert(format!(
                "src/{file}:{}: {layer} -> {}{}",
                dependency.line,
                if resolved.internal { "crate::" } else { "" },
                resolved.path.join("::")
            ));
        }
    }
    problems.into_iter().collect()
}

fn rust_files(directory: &Path, output: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
    Ok(())
}

#[test]
fn rust_dependencies_follow_architecture_layers() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    for layer in LAYERS {
        rust_files(&root.join(layer), &mut files)?;
    }
    files.sort();
    let mut problems = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        problems.extend(violations(&relative, &fs::read_to_string(path)?));
    }
    assert!(
        problems.is_empty(),
        "Rust layer contract (api | mcp_tools -> services -> store | telegram | spec):\n{}",
        problems.join("\n")
    );
    Ok(())
}

#[test]
fn gate_detects_grouped_relative_alias_and_framework_imports() {
    for source in [
        "use crate::{settings::Settings, api::{posts as routes}};",
        "use super::super::mcp_tools::router;",
        "use crate as app; fn bad() { app::api::router(); }",
        "use crate::{self as app}; fn bad() { app::r#api::router(); }",
        "fn bad() -> axum::Json<()> { todo!() }",
        "use rmcp::{model::{Tool}, ServerHandler};",
        "use crate::*;",
    ] {
        assert!(
            !violations("services/example.rs", source).is_empty(),
            "missed: {source}"
        );
    }
    assert!(!violations("store/example.rs", "use crate::services::drafts;").is_empty());
    assert!(!violations("spec/example.rs", "use crate::store::drafts;").is_empty());
    assert!(!violations("mcp_tools/tools/example.rs", "use crate::api::posts;").is_empty());
}

#[test]
fn gate_ignores_literals_comments_and_keeps_exceptions_narrow() {
    let valid = r####"
        // use crate::api::posts;
        /* outer /* use axum::Json; */ crate::mcp_tools::router(); */
        let text = "crate::api::posts();";
        let raw = br###"use crate::mcp_tools; \""###;
        let value = '\'';
        fn lifetime<'a>(value: &'a str) {}
        use crate::{settings::Settings, store::{posts, db::Store}};
    "####;
    assert!(violations("services/example.rs", valid).is_empty());
    for (file, path) in [
        ("api/mod.rs", "crate::mcp_tools::router"),
        ("telegram/client.rs", "crate::store::channels"),
        ("services/health.rs", "crate::runtime::static_dir"),
        ("services/templates.rs", "utoipa::ToSchema"),
    ] {
        let source = format!("use {path};");
        assert!(
            violations(file, &source).is_empty(),
            "approved exception: {file}"
        );
        assert!(
            !violations("spec/example.rs", &source).is_empty(),
            "exception leaked: {path}"
        );
    }
}
