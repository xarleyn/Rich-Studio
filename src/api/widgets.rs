//! Предпросмотр HTML-виджета: документ с вендоренным runtime WidgetSandbox.
//!
//! Рантайму нужен SharedArrayBuffer, а он доступен только в изолированном
//! top-level документе, поэтому страница открывается отдельным окном
//! (`window.open` через form target из редактора), а не iframe: заголовки
//! COOP/COEP ниже делают её изолированной. CSP ставит `security.rs`
//! (`WIDGET_PREVIEW_CSP`): сеть закрыта, скрипты — вендоренный host и QuickJS.
//! Исходник виджета передаётся как JSON-данные, исполняет его QuickJS-worker,
//! до DOM страницы он не доходит.

use axum::extract::Form;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PreviewForm {
    html: String,
    #[serde(default)]
    chat_data: Option<String>,
    #[serde(default)]
    public_data: Option<String>,
}

#[utoipa::path(post, path = "/render/widget", operation_id = "widget_preview_render_widget_post", summary = "Предпросмотр виджета: страница с runtime WidgetSandbox", description = "Принимает форму с полями `html`, `chat_data`, `public_data` и отдаёт страницу предпросмотра (открывается отдельным окном: рантайму нужен SharedArrayBuffer, а он требует COOP/COEP). Ничего не отправляется и не сохраняется.", tag = "preview", responses((status = 200, description = "Successful Response")))]
pub async fn preview(Form(form): Form<PreviewForm>) -> Response {
    let mut source = json!({"html": form.html});
    if let Some(data) = form.chat_data.filter(|value| !value.is_empty()) {
        source["chat_data"] = json!(data);
    }
    if let Some(data) = form.public_data.filter(|value| !value.is_empty()) {
        source["public_data"] = json!(data);
    }
    // `<` и `>` внутри JSON-строк экранируем: блок type="application/json" не исполняется,
    // но без экранирования в нём может оказаться закрывающий `</script>`.
    let embedded = serde_json::to_string(&source)
        .unwrap_or_else(|_| "{}".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let document = format!(
        "<!doctype html>\n<html lang=\"ru\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>Виджет — предпросмотр</title>\n\
         <style>html,body{{margin:0;padding:0;background:#fff}}\
         #host{{padding:8px}}\
         #status{{position:fixed;inset:auto 0 0 0;padding:4px 12px;font:12px/1.6 system-ui;color:#708;\n\
         background:rgba(255,255,255,.9);border-top:1px solid #e4e0f0}}</style>\n\
         </head>\n<body>\n<div id=\"host\"></div>\n<div id=\"status\">загрузка…</div>\n\
         <script type=\"application/json\" id=\"widget-source\">{embedded}</script>\n\
         <script type=\"module\" src=\"/static/vendor/widget/host.js\"></script>\n\
         </body>\n</html>\n"
    );
    let mut response = Html(document).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-embedder-policy", "require-corp"),
        ("cross-origin-resource-policy", "same-origin"),
    ] {
        headers.insert(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_static(value),
        );
    }
    response
}
