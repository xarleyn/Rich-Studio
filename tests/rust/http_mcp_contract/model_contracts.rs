use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, app, blocks_post, call, rpc, studio_with_fake};

#[test]
fn python_model_coercions_defaults_and_open_telegram_payloads_match() -> TestResult {
    use rich_studio::services::contracts;
    let document = contracts::http_document()?;
    let schema = &document["components"]["schemas"]["PublishReq"];
    let rich = json!({"markdown":"hello","future_field":{"pin":"true","values":[null,"1",1],"large_integer":u64::MAX}});
    let options = json!({"disable_notification":"false","future_option":["0",false]});
    let normalized = contracts::normalize(
        document,
        schema,
        &json!({"post":{"rich_message":rich,"options":options,"pin":"TRUE","ignored_outer":1},"chat_id":"-1001","dry_run":"yes","ignored_request":1}),
    )?;
    assert_eq!(
        normalized,
        json!({"post":{"rich_message":rich,"options":options,"pin":true,"reply_markup":null,"callbacks":{},"comment":null,"poll_after":null},"chat_id":"-1001","chat_ids":null,"draft_id":null,"dry_run":true,"as_new":false})
    );
    assert_eq!(
        contracts::parse_post(&json!({"rich_message":{},"pin":"off"}))?["pin"],
        false
    );
    for bad in [
        json!({}),
        json!({"rich_message":{},"options":[]}),
        json!({"rich_message":{},"callbacks":1}),
        json!({"rich_message":{},"pin":"bad"}),
    ] {
        assert!(contracts::parse_post(&bad).is_err(), "{bad}");
    }
    let draft = &document["components"]["schemas"]["DraftReq"];
    for version in [json!("1"), json!(1.0), json!(true), json!("1.0")] {
        assert_eq!(
            contracts::normalize(
                document,
                draft,
                &json!({"post":{"arbitrary":[1,"2"]},"version":version})
            )?["version"],
            1
        );
    }
    let token = &document["components"]["schemas"]["TokenReq"];
    for expiry in [json!("1"), json!(1.0), json!(true)] {
        assert!(
            contracts::normalize(
                document,
                token,
                &json!({"name":"client","expires_in_days":expiry})
            )
            .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_post_model_is_checked_before_save_publish_and_preview() -> TestResult {
    let dir = TempDir::new()?;
    let app = app(&dir, true);
    for name in [
        "validate_post",
        "save_draft",
        "preview_post",
        "send_preview_to_me",
        "publish_post",
    ] {
        let result=rpc(app.clone(),"tools/call",json!({"name":name,"arguments":{"post":{"rich_message":{"markdown":"hello"},"options":[]},"dry_run":true}})).await;
        assert_eq!(result["result"]["isError"], true, "{name}: {result}");
    }
    let listed = rpc(
        app.clone(),
        "tools/call",
        json!({"name":"list_drafts","arguments":{}}),
    )
    .await;
    assert_eq!(listed["result"]["content"], json!([]));
    let saved=rpc(app.clone(),"tools/call",json!({"name":"save_draft","arguments":{"post":{"rich_message":{"markdown":"hello"},"pin":"true"}}})).await;
    let id = saved["result"]["structuredContent"]["id"]
        .as_str()
        .ok_or("saved id")?;
    let both=rpc(app.clone(),"tools/call",json!({"name":"preview_post","arguments":{"draft_id":id,"post":{"rich_message":{"markdown":"different"}}}})).await;
    assert_eq!(both["result"]["isError"], true);
    let shown = rpc(
        app,
        "tools/call",
        json!({"name":"preview_post","arguments":{"draft_id":id}}),
    )
    .await;
    assert_eq!(shown["result"]["structuredContent"]["post"]["pin"], true);
    assert_eq!(
        shown["result"]["structuredContent"]["post"]["callbacks"],
        json!({})
    );
    Ok(())
}

#[test]
fn normalized_http_request_preserves_python_idempotency_fingerprint() -> TestResult {
    use rich_studio::services::{contracts, idempotency};
    let document = contracts::http_document()?;
    let input = json!({"post":{"rich_message":{"markdown":"hello"},"pin":"false","_studio":{"ignored":true}},"chat_id":"@channel","dry_run":0,"ignored_request":"discarded"});
    let normalized = contracts::normalize(
        document,
        &document["components"]["schemas"]["PublishReq"],
        &input,
    )?;
    // Exact Post.model_dump + PublishReq.model_dump from Python, including defaults.
    let python = json!({"post":{"rich_message":{"markdown":"hello"},"reply_markup":null,"options":{},"pin":false,"comment":null,"callbacks":{},"poll_after":null},"chat_id":"@channel","chat_ids":null,"draft_id":null,"dry_run":false,"as_new":false});
    assert_eq!(normalized, python);
    assert_eq!(
        idempotency::fingerprint("send", &normalized),
        idempotency::fingerprint("send", &python)
    );
    Ok(())
}

#[tokio::test]
async fn mcp_validate_and_describe_never_touch_telegram() -> TestResult {
    let dir = TempDir::new()?;
    let (_base, tg, _state, app) = studio_with_fake(&dir, &[]).await;
    let (result, data, text) =
        call(app.clone(), "validate_post", json!({"post":blocks_post()})).await;
    assert_eq!(result["isError"], false, "{text}");
    assert_eq!(data["ok"], true);
    let (_, described, _) = call(
        app.clone(),
        "describe_type",
        json!({"name":"InputRichBlockTable"}),
    )
    .await;
    assert_eq!(described["name"], "InputRichBlockTable");
    let (result, _, text) = call(app, "describe_type", json!({"name":"NoSuchType"})).await;
    assert_eq!(result["isError"], true);
    assert!(text.contains("NoSuchType"), "{text}");
    assert!(tg.all_calls().is_empty(), "{:?}", tg.all_calls());
    Ok(())
}
