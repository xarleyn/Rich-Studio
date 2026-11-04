use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use rich_studio::{
    AppState,
    account_context::{AccountContext, Role},
    mcp_tools::tools::templates as mcp,
    services::{
        media::{MediaError, MediaService},
        templates::{FillInput, TemplateError, TemplateInput, TemplateService},
    },
    settings::Settings,
    store::{self, db::Store, templates as records},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn state(dir: &TempDir) -> Result<AppState, Box<dyn std::error::Error>> {
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
        ("TG_UPDATES".to_owned(), "off".to_owned()),
    ]))?;
    Ok(AppState::new(settings))
}

fn field() -> Value {
    json!({"key":"name","label":"Название","type":"text","required":true,"default":""})
}

fn post() -> Value {
    json!({
        "rich_message":{"blocks":[{"type":"heading","size":1,"text":"{{name}}"},{"type":"paragraph","text":{"type":"bold","text":"О {{name}}"}},{"type":"photo","photo":{"type":"photo","media":"upload:cover.png"}}],"future_flag":"keep"},
        "options":{"disable_notification":true},"reply_markup":{"inline_keyboard":[[{"text":"Открыть {{name}}","url":"https://example.com"}]]},
        "future_option":42,"_studio":{"buffers":{"json":"broken","html":"stale"}}
    })
}

fn input(post: Value, fields: Value) -> Result<TemplateInput, serde_json::Error> {
    serde_json::from_value(json!({"title":"Анонс","post":post,"fields":fields}))
}

fn fill(version: i64, values: Value) -> Result<FillInput, serde_json::Error> {
    serde_json::from_value(json!({"version":version,"values":values}))
}

fn tid(template: &Value) -> Result<&str, Box<dyn std::error::Error>> {
    template["id"]
        .as_str()
        .ok_or_else(|| "missing template id".into())
}

#[test]
fn templates_preserve_unknown_structure_and_make_independent_unpublished_drafts() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let service = TemplateService::new(state.account.clone());
    let template = service.save(None, input(post(), json!([field()]))?)?;
    assert!(template["payload"].get("_studio").is_none());
    assert_eq!(template["version"], 1);
    assert_eq!(service.list()?[0]["fields"], json!([field()]));
    let draft = service.create_draft(
        tid(&template)?,
        fill(1, json!({"name":"<тест> & {{literal}}"}))?,
    )?;
    assert!(draft["post_id"].is_null());
    assert!(draft["chat_ids"].is_null() || draft["chat_ids"] == json!([]));
    assert_eq!(
        draft["payload"]["rich_message"]["blocks"][0]["text"],
        "<тест> & {{literal}}"
    );
    assert_eq!(
        draft["payload"]["rich_message"]["blocks"][1]["text"]["text"],
        "О <тест> & {{literal}}"
    );
    assert_eq!(
        draft["payload"]["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Открыть <тест> & {{literal}}"
    );
    assert_eq!(draft["payload"]["future_option"], 42);
    assert_eq!(draft["payload"]["rich_message"]["future_flag"], "keep");
    assert_eq!(draft["payload"]["options"]["disable_notification"], true);
    let other = service.create_draft(tid(&template)?, fill(1, json!({"name":"second"}))?)?;
    assert_ne!(other["id"], draft["id"]);
    assert_eq!(service.get(tid(&template)?)?, template);
    assert!(store::posts::list(&state.account)?.is_empty());
    Ok(())
}

#[test]
fn values_are_literal_in_html_markdown_and_quoted_attributes() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    for format in ["html", "markdown"] {
        let source = "<p>{{name}}</p><a href=\"{{link}}\" title=\"{{name}}\">link</a>";
        let template = service.save(
            None,
            input(
                json!({"rich_message":{format:source}}),
                json!([field(),{"key":"link","label":"URL","type":"url"}]),
            )?,
        )?;
        let draft = service.create_draft(tid(&template)?, fill(1, json!({"name":"\"><img src=x onerror=alert(1)> **hi**","link":"https://example.com/a(b)?x=1&y=2"}))?)?;
        let source = draft["payload"]["rich_message"][format]
            .as_str()
            .ok_or("missing source")?;
        assert!(!source.contains("<img"));
        if format == "html" {
            assert!(source.contains("&lt;img"));
            assert!(source.contains("**hi**"));
            assert!(source.contains("href=\"https://example.com/a(b)?x=1&amp;y=2\""));
        } else {
            assert!(source.contains("&#60;img"));
            assert!(source.contains("&#42;&#42;hi&#42;&#42;"));
            assert!(
                source
                    .contains("href=\"https://example&#46;com/a&#40;b&#41;?x&#61;1&#38;y&#61;2\"")
            );
        }
        for url in [
            "javascript:alert(1)",
            "https://example.com/\" onerror=\"x",
            "http:example.com",
            "https://example.com/a b",
            "https://example.com/\\evil",
        ] {
            assert!(
                matches!(
                    service
                        .create_draft(tid(&template)?, fill(1, json!({"name":"safe","link":url}))?),
                    Err(TemplateError::Invalid(_))
                ),
                "{url}"
            );
        }
    }
    Ok(())
}

#[test]
fn unsafe_markup_and_variable_positions_are_rejected() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    for html in [
        "<p title={{name}}>text</p>",
        "<{{name}}>text</{{name}}>",
        "<p {{name}}=\"value\">text</p>",
        "<p onclick=\"{{name}}\">text</p>",
        "<a href=\"{{name}}\">text</a>",
        "<a href=\"java{{name}}script:x\">text</a>",
    ] {
        assert!(
            matches!(
                service.save(
                    None,
                    input(json!({"rich_message":{"html":html}}), json!([field()]))?
                ),
                Err(TemplateError::Invalid(_))
            ),
            "{html}"
        );
    }
    for fields in [
        json!([]),
        json!([field(), field()]),
        json!([field(), {"key":"unused","label":"Unused"}]),
    ] {
        assert!(matches!(
            service.save(None, input(post(), fields)?),
            Err(TemplateError::Invalid(_))
        ));
    }
    assert!(
        service
            .save(
                None,
                input(
                    json!({"rich_message":{"blocks":[{"type":"paragraph","{{name}}":"value"}]}}),
                    json!([field()])
                )?
            )
            .is_err()
    );
    for source in ["upload:{{name}}.png", "{{prefix}}{{name}}.png"] {
        let fields = if source.starts_with("upload:") {
            json!([{ "key":"name","label":"Name","default":"cover" }])
        } else {
            json!([{ "key":"name","label":"Name","default":"cover" },{ "key":"prefix","label":"Prefix","default":"upload:" }])
        };
        // A complete upload reference is required for media cleanup to see every default.
        assert!(service.save(None, input(json!({"rich_message":{"blocks":[{"type":"photo","photo":{"media":source}}]}}), fields)?).is_err());
    }
    Ok(())
}

#[test]
fn markdown_code_and_math_preserve_existing_rejection_boundaries() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    for source in [
        "`{{name}}`",
        "``a {{name}} b``",
        "```\n{{name}}\n```",
        "~~~\n{{name}}\n~~~",
        "    {{name}}",
        r"\\`{{name}}`",
        "> ~~~\n> {{name}}\n> ~~~",
        "- ~~~\n  {{name}}\n  ~~~",
        "> 1. ~~~~\n>    {{name}}\n>    ~~~~",
        "~~~\n- ~~~\n{{name}}\n~~~",
        "~~~\n~~~ not-a-closing-fence\n{{name}}\n~~~",
        ">     {{name}}",
        "-     {{name}}",
        "$a+{{name}}$",
        "$$a+{{name}}$$",
        "$$\na+{{name}}\n$$",
        "> $$\n> {{name}}\n> $$",
        "- $$\n  {{name}}\n  $$",
        "$$a $ {{name}}$$",
        "$a+{{name}}$$",
    ] {
        assert!(
            matches!(
                service.save(
                    None,
                    input(
                        json!({"rich_message":{"markdown":source}}),
                        json!([field()])
                    )?
                ),
                Err(TemplateError::Invalid(_))
            ),
            "{source}"
        );
    }
    for source in [
        r"\$a+{{name}}\$",
        "<tg-math>a+{{name}}</tg-math>",
        "$a$ {{name}} $b$",
        "$$a$$ {{name}} $$b$$",
        r"\`{{name}}`",
    ] {
        let template = service.save(
            None,
            input(
                json!({"rich_message":{"markdown":source}}),
                json!([field()]),
            )?,
        )?;
        assert!(
            service
                .create_draft(tid(&template)?, fill(1, json!({"name":"safe"}))?)
                .is_ok(),
            "{source}"
        );
    }
    Ok(())
}

#[test]
fn fields_require_defaults_and_reject_unknown_or_oversized_values() -> TestResult {
    let dir = TempDir::new()?;
    let service = TemplateService::new(state(&dir)?.account);
    let template = service.save(None, input(post(), json!([field()]))?)?;
    for values in [
        json!({}),
        json!({"name":" "}),
        json!({"name":"x","extra":"y"}),
        json!({"name":"a".repeat(10001)}),
    ] {
        assert!(matches!(
            service.create_draft(tid(&template)?, fill(1, values)?),
            Err(TemplateError::Invalid(_))
        ));
    }
    let default = service.save(
        None,
        input(
            post(),
            json!([{"key":"name","label":"Name","default":"По умолчанию"}]),
        )?,
    )?;
    assert_eq!(
        service.create_draft(tid(&default)?, fill(1, json!({}))?)?["payload"]["rich_message"]["blocks"]
            [0]["text"],
        "По умолчанию"
    );
    let optional = service.save(
        None,
        input(
            post(),
            json!([{"key":"name","label":"Name","required":false}]),
        )?,
    )?;
    assert_eq!(
        service.create_draft(tid(&optional)?, fill(1, json!({}))?)?["payload"]["rich_message"]["blocks"]
            [0]["text"],
        ""
    );
    Ok(())
}

#[test]
fn versions_guard_update_fill_delete_and_concurrent_writers() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let service = TemplateService::new(state.account.clone());
    let template = service.save(None, input(post(), json!([field()]))?)?;
    let id = tid(&template)?.to_owned();
    assert!(matches!(
        service.save(Some(&id), input(post(), json!([field()]))?),
        Err(TemplateError::Invalid(_))
    ));
    let barrier = Arc::new(Barrier::new(2));
    let workers = (0..2)
        .map(|index| {
            let context = state.account.clone();
            let barrier = barrier.clone();
            let id = id.clone();
            let mut input = input(post(), json!([field()]))?;
            input.version = Some(1);
            input.title = format!("writer {index}");
            Ok(std::thread::spawn(move || {
                barrier.wait();
                TemplateService::new(context).save(Some(&id), input)
            }))
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().map_err(|_| "template worker panic"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(TemplateError::Conflict(_))))
            .count(),
        1
    );
    assert!(matches!(
        service.create_draft(&id, fill(1, json!({"name":"stale"}))?),
        Err(TemplateError::Conflict(_))
    ));
    assert!(matches!(
        service.delete(&id, 1),
        Err(TemplateError::Conflict(_))
    ));
    let draft = service.create_draft(&id, fill(2, json!({"name":"current"}))?)?;
    service.delete(&id, 2)?;
    assert!(matches!(service.get(&id), Err(TemplateError::NotFound)));
    assert!(store::drafts::get(&state.account, draft["id"].as_str().ok_or("draft id")?)?.is_some());
    Ok(())
}

#[test]
fn expansion_is_bounded_before_building_large_strings_and_size_count_limits_apply() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let service = TemplateService::new(state.account.clone());
    let huge = input(
        json!({"rich_message":{"html":"x".repeat(records::MAX_BYTES)}}),
        json!([]),
    )?;
    assert!(matches!(
        service.save(None, huge),
        Err(TemplateError::TooLarge(_))
    ));
    let repeated = service.save(
        None,
        input(
            json!({"rich_message":{"html":"{{name}}".repeat(120000)}}),
            json!([field()]),
        )?,
    )?;
    assert!(matches!(
        service.create_draft(tid(&repeated)?, fill(1, json!({"name":"<&".repeat(5000)}))?),
        Err(TemplateError::TooLarge(_))
    ));
    assert!(store::drafts::list(&state.account)?.is_empty());
    Store::connect(&state.account)?.execute(
        "with recursive ids(n) as (select 1 union all select n+1 from ids where n<199) insert into post_templates(id,title,payload,fields,created_at,updated_at) select 'filler-'||n,'filler','{}','[]',0,0 from ids", [],
    )?;
    assert_eq!(service.list()?.len(), 200);
    assert!(matches!(
        service.save(None, input(post(), json!([field()]))?),
        Err(TemplateError::TooLarge(_))
    ));
    Ok(())
}

#[test]
fn old_template_rows_are_read_and_accounts_never_share_templates() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let one_context = AccountContext::account(&state.settings, 101, Role::Publish);
    let two_context = AccountContext::account(&state.settings, 202, Role::Publish);
    let one = TemplateService::new(one_context.clone());
    let two = TemplateService::new(two_context);
    let template = one.save(None, input(post(), json!([field()]))?)?;
    assert!(two.list()?.is_empty());
    assert!(matches!(
        two.get(tid(&template)?),
        Err(TemplateError::NotFound)
    ));
    assert!(matches!(
        two.create_draft(tid(&template)?, fill(1, json!({"name":"x"}))?),
        Err(TemplateError::NotFound)
    ));
    assert!(matches!(
        two.delete(tid(&template)?, 1),
        Err(TemplateError::NotFound)
    ));
    let mut change = input(post(), json!([field()]))?;
    change.version = Some(1);
    assert!(matches!(
        two.save(Some(tid(&template)?), change),
        Err(TemplateError::NotFound)
    ));
    let connection = Store::connect(&one_context)?;
    connection.execute("insert into post_templates(id,title,payload,fields,version,created_at,updated_at) values('legacy','Старый','{\"rich_message\": {\"html\": \"{{name}}\"}}','[{\"key\": \"name\", \"label\": \"Name\", \"type\": \"text\", \"required\": true, \"default\": \"old\"}]',7,123,124)", [])?;
    assert_eq!(one.get("legacy")?["version"], 7);
    assert_eq!(
        one.create_draft("legacy", fill(7, json!({}))?)?["payload"]["rich_message"]["html"],
        "old"
    );
    connection.execute("update post_templates set fields='{' where id='legacy'", [])?;
    assert!(matches!(one.get("legacy"), Err(TemplateError::Store(_))));
    Ok(())
}

#[test]
fn media_cleanup_keeps_literal_and_default_template_uploads() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let service = TemplateService::new(state.account.clone());
    let media = MediaService::new(state.account.clone(), 50.0, 500.0);
    let template = service.save(None, input(post(), json!([field()]))?)?;
    let _ = media.upload("cover.png", b"fake")?;
    assert_eq!(
        records::using(&state.account, "upload:cover.png")?,
        vec![json!({"id":template["id"],"title":template["title"]})]
    );
    assert!(
        media
            .used_by("cover.png")?
            .iter()
            .any(|label| label.contains("шаблон"))
    );
    assert!(matches!(
        media.delete("cover.png", false),
        Err(MediaError::Conflict(_))
    ));
    let _ = service.save(
        None,
        input(
            json!({"rich_message":{"blocks":[{"type":"photo","photo":{"media":"{{file}}"}}]}}),
            json!([{"key":"file","label":"Медиа","default":"upload:default.png"}]),
        )?,
    )?;
    let _ = media.upload("default.png", b"fake")?;
    assert!(!records::using(&state.account, "upload:default.png")?.is_empty());
    assert!(media.delete_unused()?.is_empty());
    Ok(())
}

fn router(state: AppState) -> Router {
    rich_studio::build_router(state)
}

async fn request(
    app: Router,
    method: &str,
    uri: &str,
    body: Value,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))?;
    let response = app.oneshot(request).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 2 * records::MAX_BYTES).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

#[tokio::test]
async fn http_contract_has_created_status_version_guards_and_typed_requests() -> TestResult {
    let dir = TempDir::new()?;
    let app = router(state(&dir)?);
    let body = json!({"title":"HTTP","post":post(),"fields":[field()]});
    let (status, template) = request(app.clone(), "POST", "/api/templates", body.clone()).await?;
    assert_eq!(status, StatusCode::CREATED);
    let path = format!("/api/templates/{}", tid(&template)?);
    assert_eq!(
        request(app.clone(), "PUT", &path, body.clone()).await?.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(app.clone(), "DELETE", &path, Value::Null).await?.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("{path}?version=0"),
            Value::Null
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut changed = body;
    changed["version"] = json!(1);
    assert_eq!(
        request(app.clone(), "PUT", &path, changed.clone()).await?.0,
        StatusCode::OK
    );
    assert_eq!(
        request(app.clone(), "PUT", &path, changed).await?.0,
        StatusCode::CONFLICT
    );
    let (status, draft) = request(
        app.clone(),
        "POST",
        &format!("{path}/drafts"),
        json!({"version":2,"values":{"name":"from HTTP"}}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        draft["payload"]["rich_message"]["blocks"][0]["text"],
        "from HTTP"
    );
    assert_eq!(
        request(
            app.clone(),
            "DELETE",
            &format!("{path}?version=2"),
            Value::Null
        )
        .await?
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(app.clone(), "GET", &path, Value::Null).await?.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            app,
            "POST",
            "/api/templates",
            json!({"title":"invalid","post":{},"fields":[{"key":"x","label":42}]})
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    Ok(())
}

#[tokio::test]
async fn mcp_tools_share_services_and_return_editor_links_without_publication() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let template = mcp::call(
        state.clone(),
        "save_post_template",
        json!({"title":"MCP","post":post(),"fields":[field()]}),
    )
    .await?;
    assert_eq!(
        mcp::call(
            state.clone(),
            "get_post_template",
            json!({"template_id":template["id"]})
        )
        .await?,
        template
    );
    let draft = mcp::call(
        state.clone(),
        "draft_from_template",
        json!({"template_id":template["id"],"version":1,"values":{"name":"MCP"}}),
    )
    .await?;
    assert!(
        draft["editor_url"]
            .as_str()
            .ok_or("editor_url")?
            .ends_with(&format!(
                "/?draft={}",
                draft["id"].as_str().ok_or("draft id")?
            ))
    );
    assert_eq!(draft["payload"]["rich_message"]["blocks"][0]["text"], "MCP");
    assert_eq!(mcp::definitions().len(), 4);
    for tool in mcp::definitions() {
        assert_eq!(tool["annotations"]["openWorldHint"], false);
    }
    assert!(
        mcp::call(
            state.clone(),
            "save_post_template",
            json!({"title":"missing fields","post":post()})
        )
        .await
        .is_err()
    );
    assert!(store::posts::list(&state.account)?.is_empty());
    Ok(())
}

#[test]
fn account_database_quota_does_not_damage_previous_template() -> TestResult {
    let dir = TempDir::new()?;
    let mut settings = (*state(&dir)?.settings).clone();
    settings.account_database_mb = 1.0;
    let context = AccountContext::account(&settings, 101, Role::Publish);
    let service = TemplateService::new(context);
    let first = service.save(
        None,
        input(
            json!({"rich_message":{"html":"x".repeat(500_000)}}),
            json!([]),
        )?,
    )?;
    let error = service.save(
        None,
        input(
            json!({"rich_message":{"html":"y".repeat(500_000)}}),
            json!([]),
        )?,
    );
    assert!(matches!(
        error,
        Err(TemplateError::Store(
            rich_studio::errors::StoreError::StorageFull
        ))
    ));
    assert_eq!(service.get(tid(&first)?)?, first);
    assert_eq!(service.list()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn reader_tokens_can_list_but_cannot_create_delete_or_fill_over_http_and_mcp() -> TestResult {
    let dir = TempDir::new()?;
    let state = state(&dir)?;
    let service = TemplateService::new(state.account.clone());
    let template = service.save(None, input(post(), json!([field()]))?)?;
    store::auth::add_token(
        &state.settings,
        "template reader",
        "test-reader-secret",
        "read",
    )?;
    let app = router(state.clone());
    let cases = [
        (
            "GET",
            "/api/templates".to_owned(),
            Value::Null,
            StatusCode::OK,
        ),
        (
            "POST",
            "/api/templates".to_owned(),
            json!({"title":"Denied","post":post(),"fields":[field()]}),
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            format!("/api/templates/{}/drafts", tid(&template)?),
            json!({"version":1,"values":{"name":"Denied"}}),
            StatusCode::FORBIDDEN,
        ),
        (
            "DELETE",
            format!("/api/templates/{}?version=1", tid(&template)?),
            Value::Null,
            StatusCode::FORBIDDEN,
        ),
    ];
    for (method, uri, body, expected) in cases {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-reader-secret")
            .body(Body::from(body.to_string()))?;
        assert_eq!(app.clone().oneshot(request).await?.status(), expected);
    }
    let request = Request::builder().method("POST").uri("/mcp").header("host", "localhost")
        .header("accept","application/json, text/event-stream").header("mcp-protocol-version","2025-06-18")
        .header("content-type", "application/json").header("authorization", "Bearer test-reader-secret")
        .body(Body::from(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"draft_from_template","arguments":{"template_id":template["id"],"version":1,"values":{"name":"Denied"}}}}).to_string()))?;
    let response = app.oneshot(request).await?;
    let bytes = axum::body::to_bytes(response.into_body(), 65536).await?;
    let result: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(service.get(tid(&template)?)?, template);
    assert!(store::drafts::list(&state.account)?.is_empty());
    Ok(())
}
