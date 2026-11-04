use std::collections::BTreeMap;

use axum::http::StatusCode;
use rich_studio::settings::Settings;
use rich_studio::store::auth;
use rich_studio::{AppState, build_router};
use tempfile::TempDir;

use crate::support::{forwarded_headers, login_from, settings};

#[tokio::test]
async fn trusted_proxy_keeps_visitor_login_budgets_separate_and_direct_spoofing_cannot_reset_them()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let config = settings(&dir, true);
    let app = build_router(AppState::new(config));
    for _ in 0..30 {
        assert_eq!(
            login_from(app.clone(), "127.0.0.1:5600", &["203.0.113.10"]).await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5601", &["203.0.113.10"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "127.0.0.1:5602", &["203.0.113.11"]).await?,
        StatusCode::UNAUTHORIZED
    );
    // An untrusted direct client cannot get fresh buckets by rotating XFF.
    for host in 1..=30 {
        assert_eq!(
            login_from(
                app.clone(),
                "198.51.100.20:5603",
                &[&format!("192.0.2.{host}")]
            )
            .await?,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(app.clone(), "198.51.100.20:5604", &["192.0.2.31"]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app, "198.51.100.21:5605", &["192.0.2.31"]).await?,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn proxy_chain_budget_stops_at_first_untrusted_hop_and_invalid_headers_use_peer()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let mut config = settings(&dir, true);
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8".parse()?;
    for key in ["login:203.0.113.9", "login:10.1.2.3"] {
        for _ in 0..30 {
            assert!(auth::take_budget(&config, key, 30, 60)?);
        }
    }
    let app = build_router(AppState::new(config));
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.1, 203.0.113.9", "10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(
            app.clone(),
            "10.1.2.3:5000",
            &["192.0.2.2, 203.0.113.9, 10.30.40.50"]
        )
        .await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(app.clone(), "10.1.2.3:5000", &["203.0.113.10, 10.30.40.50"]).await?,
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        "",
        "unknown",
        "192.0.2.1,",
        "203.0.113.10, bad-proxy",
        "999.1.1.1",
        "192.0.2.1:99999",
    ] {
        assert_eq!(
            login_from(app.clone(), "10.1.2.3:5001", &[invalid]).await?,
            StatusCode::TOO_MANY_REQUESTS,
            "invalid XFF: {invalid:?}"
        );
    }
    assert_eq!(
        login_from(app, "10.1.2.3:5002", &[]).await?,
        StatusCode::TOO_MANY_REQUESTS
    );
    Ok(())
}

#[test]
fn forwarded_address_policy_covers_ipv6_cidrs_wildcard_and_invalid_settings()
-> Result<(), Box<dyn std::error::Error>> {
    use rich_studio::security::client_ip;
    let dir = TempDir::new()?;
    let mut config = settings(&dir, false);
    assert!(config.forwarded_allow_ips.contains("127.0.0.1".parse()?));
    assert!(!config.forwarded_allow_ips.contains("::1".parse()?));
    config.forwarded_allow_ips = "10.0.0.0/8,fd00::/8,::1".parse()?;
    for (peer, forwarded, expected) in [
        (
            "10.1.2.3:5000",
            "192.0.2.1, 203.0.113.2, 10.20.30.40",
            "203.0.113.2",
        ),
        ("10.1.2.3:5000", "10.2.3.4, 10.3.4.5", "10.2.3.4"),
        ("[fd00::2]:5000", "2001:db8::4, fd00::3", "2001:db8::4"),
        (
            "[::1]:5000",
            "[2001:db8::5]:9876, [fd00::3]:4321",
            "2001:db8::5",
        ),
        ("[::1]:5000", "[2001:db8::5]", "2001:db8::5"),
        ("10.1.2.3:5000", "203.0.113.6:9876, 10.2.3.4", "203.0.113.6"),
        ("[fe00::1]:5000", "2001:db8::5", "fe00::1"),
    ] {
        assert_eq!(
            client_ip(
                &config,
                &forwarded_headers(&[forwarded])?,
                Some(peer.parse()?)
            ),
            Some(expected.parse()?),
            "{peer}: {forwarded}"
        );
    }
    let mut invalid = axum::http::HeaderMap::new();
    invalid.insert(
        "x-forwarded-for",
        axum::http::HeaderValue::from_bytes(&[0xff])?,
    );
    assert_eq!(
        client_ip(&config, &invalid, Some("10.1.2.3:5000".parse()?)),
        Some("10.1.2.3".parse()?)
    );
    config.forwarded_allow_ips = "*".parse()?;
    let headers = forwarded_headers(&["192.0.2.1, 203.0.113.2"])?;
    assert_eq!(
        client_ip(&config, &headers, Some("198.51.100.1:50".parse()?)),
        Some("192.0.2.1".parse()?)
    );
    assert_eq!(client_ip(&config, &headers, None), None);
    config.forwarded_allow_ips = "".parse()?;
    assert_eq!(
        client_ip(&config, &headers, Some("127.0.0.1:50".parse()?)),
        Some("127.0.0.1".parse()?)
    );
    for raw in [
        "127.0.0.1/33",
        "fd00::/129",
        "localhost",
        "10.0.0.0/bad",
        "*,127.0.0.1",
    ] {
        assert!(
            Settings::from_map(&BTreeMap::from([(
                "FORWARDED_ALLOW_IPS".to_owned(),
                raw.to_owned()
            )]))
            .is_err(),
            "accepted malformed proxy setting: {raw}"
        );
    }
    Ok(())
}
