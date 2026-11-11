use std::collections::BTreeSet;

use axum::http::StatusCode;
use rich_studio::build_router;
use serde_json::json;
use tempfile::TempDir;

use crate::support::{TestResult, call, raw_server, settings, state, telegram};

#[tokio::test]
async fn spec_summary_and_type_lookup_match_the_editor() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let summary = call(app.clone(), "GET", "/api/spec", None, &[]).await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.text);
    let spec = summary.value;
    assert_eq!(spec["version"], "Bot API 10.3");
    let subtypes = spec["types"]["InputRichBlock"]["subtypes"]
        .as_array()
        .ok_or("subtypes")?;
    assert_eq!(subtypes.len(), 24);
    let constants = subtypes
        .iter()
        .map(|name| spec["types"][name.as_str().unwrap_or_default()]["const"].clone())
        .collect::<Vec<_>>();
    for expected in [
        "paragraph",
        "heading",
        "table",
        "buttons",
        "document",
        "map",
        "slideshow",
    ] {
        assert!(
            constants.iter().any(|value| value == expected),
            "missing {expected}"
        );
    }
    assert!(constants.iter().all(|value| !value.is_null()));
    assert_eq!(
        spec["types"]["RichMessageButton"]["fields"][1]["enum"],
        json!(["danger", "success", "primary", "link"])
    );

    let definition = call(
        app.clone(),
        "GET",
        "/api/spec/type/sendRichMessage",
        None,
        &[],
    )
    .await;
    assert_eq!(definition.status, StatusCode::OK, "{}", definition.text);
    assert!(
        definition.value["fields"]
            .as_array()
            .is_some_and(|fields| !fields.is_empty())
    );
    assert_eq!(
        call(app, "GET", "/api/spec/type/NoSuchType", None, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[test]
fn diff_reports_added_subtype_type_field_and_method() {
    use rich_studio::spec::diff::diff_specs;
    let old = json!({
        "types": {
            "InputRichBlock": {"subtypes": ["InputRichBlockParagraph"]},
            "InputRichBlockTable": {"fields": [{"name": "cells", "types": ["X"]}]}
        },
        "methods": {}
    });
    let new = json!({
        "types": {
            "InputRichBlock": {"subtypes": ["InputRichBlockParagraph", "InputRichBlockPoll"]},
            "InputRichBlockPoll": {"description": ["A poll block."], "fields": []},
            "InputRichBlockTable": {
                "fields": [
                    {"name": "cells", "types": ["X"]},
                    {"name": "is_compact", "types": ["Boolean"]}
                ]
            }
        },
        "methods": {"sendRichPoll": {"description": ["Sends a poll."], "fields": []}}
    });
    let kinds = diff_specs(&old, &new)
        .into_iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap_or_default().to_owned(),
                item["name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<BTreeSet<_>>();
    for expected in [
        ("subtype_added", "InputRichBlock → InputRichBlockPoll"),
        ("type_added", "InputRichBlockPoll"),
        ("field_added", "InputRichBlockTable.is_compact"),
        ("method_added", "sendRichPoll"),
    ] {
        assert!(
            kinds.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "missing {expected:?}: {kinds:?}"
        );
    }
}

#[tokio::test]
async fn spec_refresh_refuses_bad_downloads() -> TestResult {
    use rich_studio::spec::source::Spec;
    let dir = TempDir::new()?;
    let base = raw_server(vec![
        json!({}),
        json!({"error":"rate limited"}),
        json!({"version":"Bot API 9.0","types":{},"methods":{}}),
    ])
    .await;
    let config = settings(
        &dir,
        "http://127.0.0.1:9",
        &[("SPEC_URL", &format!("{base}/api.json"))],
    );
    let mut spec = Spec::load_initial(&config);
    let good_version = spec.version().to_owned();
    for _ in 0..3 {
        let report = spec.refresh(&config, true).await;
        assert_eq!(report["updated"], false, "{report}");
        assert!(report["error"].as_str().is_some(), "{report}");
        assert_eq!(spec.version(), good_version);
    }
    Ok(())
}

#[tokio::test]
async fn spec_refresh_endpoint_reports_gaps_and_failure() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _telegram) = telegram().await;
    let app = build_router(state(
        &dir,
        &base,
        &[("SPEC_URL", "http://127.0.0.1:9/unavailable")],
    ));
    let summary = call(app.clone(), "GET", "/api/spec", None, &[]).await.value;
    assert_eq!(summary["gaps"], json!([]));
    let refreshed = call(app.clone(), "POST", "/api/spec/refresh", None, &[]).await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.text);
    assert_eq!(refreshed.value["updated"], false);
    assert!(
        refreshed.value["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{}",
        refreshed.text
    );
    let check = call(app, "GET", "/api/spec", None, &[]).await.value["last_check"].clone();
    assert!(check["at"].as_i64().unwrap_or_default() > 0, "{check}");
    assert!(
        check["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    Ok(())
}
