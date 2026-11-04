use rich_studio::account_context::Role;
use rich_studio::services::auth as service_auth;
use tempfile::TempDir;

use crate::support::{TestResult, cli_settings, run_cli};

#[test]
fn cli_makes_lists_and_revokes_tokens() -> TestResult {
    let dir = TempDir::new()?;
    let added = run_cli(&dir, &["token", "add", "ci-bot", "--level", "publish"]);
    assert_eq!(added.status.code(), Some(0));
    let secret = String::from_utf8_lossy(&added.stdout).trim().to_owned();
    assert!(secret.starts_with("rs_"));
    let config = cli_settings(&dir)?;
    let principal = service_auth::by_secret(&config, &secret)?.ok_or("token is missing")?;
    assert_eq!(principal.name.as_deref(), Some("ci-bot"));
    assert_eq!(principal.role, Role::Publish);
    let duplicate = run_cli(&dir, &["token", "add", "ci-bot"]);
    assert_eq!(duplicate.status.code(), Some(2));
    let invalid = run_cli(&dir, &["token", "add", "bad name"]);
    assert_eq!(invalid.status.code(), Some(2));
    let listed = run_cli(&dir, &["token", "list"]);
    assert_eq!(listed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("ci-bot"));
    let revoked = run_cli(&dir, &["token", "revoke", "ci-bot"]);
    assert_eq!(revoked.status.code(), Some(0));
    assert!(service_auth::by_secret(&config, &secret)?.is_none());
    let again = run_cli(&dir, &["token", "revoke", "ci-bot"]);
    assert_eq!(again.status.code(), Some(1));
    Ok(())
}
