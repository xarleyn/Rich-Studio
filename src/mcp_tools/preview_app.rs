use std::sync::OnceLock;

pub const URI: &str = "ui://rich-studio/preview";
const STYLES: &[(&str, &str)] = &[
    (
        "vendor/katex/katex.min.css",
        include_str!("../../app/static/vendor/katex/katex.min.css"),
    ),
    ("tg.css", include_str!("../../app/static/tg.css")),
];
const SCRIPTS: &[&str] = &[
    include_str!("../../app/static/vendor/katex/katex.min.js"),
    include_str!("../../app/static/vendor/markdown-it.min.js"),
    include_str!("../../app/static/render.js"),
    include_str!("../../app/static/preview-app.js"),
];
const PAGE_CSS: &str = "html,body{margin:0}body{background:transparent;font:14px system-ui,sans-serif}\n#root{padding:8px}.note{margin:4px 8px;padding:6px 10px;border-radius:8px;white-space:pre-wrap}\n.note.error{background:#fde8e8;color:#9b1c1c}.note.warning{background:#fdf6e3;color:#7a5b00}";

pub struct PreviewPage;

impl PreviewPage {
    pub fn html() -> &'static str {
        static PAGE: OnceLock<String> = OnceLock::new();
        PAGE.get_or_init(|| {
            let styles = STYLES.iter().map(|(_, source)| format!("<style>{source}</style>")).collect::<String>();
            let scripts = SCRIPTS.iter().map(|source| format!("<script>{}</script>", source.replace("</script", "<\\/script"))).collect::<String>();
            format!("<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Rich Studio</title>{styles}<style>{PAGE_CSS}</style></head><body><div id=\"root\" class=\"tg-preview\"></div><div id=\"notes\"></div>{scripts}</body></html>")
        })
    }
}
