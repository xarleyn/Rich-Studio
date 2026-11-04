use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use grammers_session::{Session, storages::MemorySession};
use grammers_tl_types as tl;
use rich_studio::{
    account_context::{AccountContext, Role},
    services::{import_links::parse_link, imports},
    telegram::import_client,
};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support::*;

#[test]
fn flood_wait_surfaces_retry_delay_without_automatic_sleep() -> TestResult {
    let rpc = grammers_client::sender::RpcError::from(tl::types::RpcError {
        error_code: 420,
        error_message: "FLOOD_WAIT_31".into(),
    });
    let error = import_client::rpc_error(grammers_client::InvocationError::Rpc(rpc));
    assert_eq!(error.status, 429);
    assert!(error.to_string().contains("31"));
    use grammers_client::sender::RetryPolicy;
    let policy = grammers_client::sender::NoRetries;
    let context = grammers_client::sender::RetryContext {
        fail_count: std::num::NonZeroU32::new(1).ok_or("count")?,
        slept_so_far: Duration::ZERO,
        error: grammers_client::InvocationError::Rpc(grammers_client::sender::RpcError::from(
            tl::types::RpcError {
                error_code: 420,
                error_message: "FLOOD_WAIT_31".into(),
            },
        )),
    };
    assert!(policy.should_retry(&context).is_break());
    Ok(())
}

#[test]
fn strict_reference_matches_shared_python_corpus() -> TestResult {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../../import_links.json"))?;
    for case in cases {
        let url = case["url"].as_str().ok_or("url missing")?;
        let result = parse_link(url);
        assert_eq!(result.is_ok(), case["valid"] == true, "{url}");
        if let Ok(link) = result {
            assert_eq!(json!(link.message_id), case["message_id"]);
            assert_eq!(json!(link.topic_id), case["topic_id"]);
            assert_eq!(parse_link(&link.url())?, link, "{url}");
        }
    }
    Ok(())
}

#[test]
fn telethon_sessions_decode_both_address_families_round_trip_without_key_debug() -> TestResult {
    for ipv6 in [false, true] {
        let encoded = session(ipv6);
        let (data, is_ipv6) = import_client::decode_session(&encoded)?;
        assert_eq!(data.home_dc, 2);
        assert_eq!(is_ipv6, ipv6);
        let storage = MemorySession::from(data);
        assert_eq!(import_client::encode_session(&storage, ipv6)?, encoded);
        assert_eq!(
            storage.dc_option(2)?.ok_or("missing dc")?.auth_key,
            Some([0x42; 256])
        );
    }
    for broken in [
        String::new(),
        "2bad".into(),
        "1bad".into(),
        format!("1{}", URL_SAFE.encode(vec![0; 279])),
    ] {
        assert_eq!(
            import_client::decode_session(&broken)
                .err()
                .ok_or("accepted invalid")?
                .status,
            409
        );
    }
    Ok(())
}

#[test]
fn session_identity_requires_account_user_and_bot_only_for_shared_transport() -> TestResult {
    let cross = import_client::check_identity(true, Some(111), 999, false)
        .err()
        .ok_or("cross uid")?;
    assert_eq!(cross.status, 403);
    assert!(cross.to_string().contains("другой учётке"), "{cross}");
    assert!(import_client::check_identity(true, Some(111), 111, true).is_err());
    assert!(import_client::check_identity(false, None, 111, false).is_err());
    import_client::check_identity(true, Some(111), 111, false)?;
    import_client::check_identity(false, None, 111, true)?;
    let dir = TempDir::new()?;
    let settings = settings(&dir)?;
    let owner = AccountContext::root(&settings);
    std::fs::create_dir_all(&settings.data_dir)?;
    std::fs::write(imports::session_path(&settings, &owner), session(false))?;
    assert_eq!(imports::status(&settings, &owner)["own_session"], true);
    for uid in [111, 222] {
        let account = AccountContext::account(&settings, uid, Role::Owner);
        assert_eq!(imports::status(&settings, &account)["own_session"], false);
        assert_ne!(
            imports::session_path(&settings, &owner),
            imports::session_path(&settings, &account)
        );
    }
    Ok(())
}

#[test]
fn proxies_decode_credentials_defaults_and_never_print_them() -> TestResult {
    for (scheme, port) in [("socks5", 1080), ("http", 8080)] {
        let config = import_client::proxy_config(&format!("{scheme}://u%40ser:p%3Ass@127.0.0.1"))?
            .ok_or("missing proxy")?;
        assert_eq!(config.port, port);
        assert_eq!(config.username.as_deref(), Some("u@ser"));
        assert_eq!(config.password.as_deref(), Some("p:ss"));
        let debug = format!("{config:?}");
        assert!(!debug.contains("u@ser") && !debug.contains("p:ss"));
    }
    assert!(import_client::proxy_config("file:///secret").is_err());
    Ok(())
}

#[tokio::test]
async fn http_connect_proxy_uses_decoded_auth_and_preserves_tunnel_bytes() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(socket.read_u8().await?);
        }
        let header = String::from_utf8(header).map_err(std::io::Error::other)?;
        assert!(header.starts_with("CONNECT 149.154.167.51:443 HTTP/1.1\r\n"));
        assert!(header.contains("Proxy-Authorization: Basic dUBzZXI6cDpzcw=="));
        socket
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nhello")
            .await?;
        Ok::<_, std::io::Error>(())
    });
    let config = import_client::proxy_config(&format!("http://u%40ser:p%3Ass@{address}"))?
        .ok_or("missing proxy")?;
    let mut socket = import_client::connect_proxy(&config, "149.154.167.51", 443).await?;
    let mut data = [0; 5];
    socket.read_exact(&mut data).await?;
    assert_eq!(&data, b"hello");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn socks_proxy_auth_and_remote_dns_are_preserved() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut header = [0; 3];
        socket.read_exact(&mut header).await?;
        assert_eq!(header, [5, 1, 2]);
        socket.write_all(&[5, 2]).await?;
        assert_eq!(socket.read_u8().await?, 1);
        let n = socket.read_u8().await?;
        let mut user = vec![0; usize::from(n)];
        socket.read_exact(&mut user).await?;
        assert_eq!(user, b"u@ser");
        let n = socket.read_u8().await?;
        let mut password = vec![0; usize::from(n)];
        socket.read_exact(&mut password).await?;
        assert_eq!(password, b"p:ss");
        socket.write_all(&[1, 0]).await?;
        let mut request = [0; 4];
        socket.read_exact(&mut request).await?;
        assert_eq!(request, [5, 1, 0, 3]);
        let n = socket.read_u8().await?;
        let mut host = vec![0; usize::from(n)];
        socket.read_exact(&mut host).await?;
        assert_eq!(host, b"dc.example.invalid");
        assert_eq!(socket.read_u16().await?, 443);
        socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        socket.write_all(b"ok").await?;
        Ok::<_, std::io::Error>(())
    });
    let config = import_client::proxy_config(&format!("socks5://u%40ser:p%3Ass@{address}"))?
        .ok_or("missing proxy")?;
    let mut socket = import_client::connect_proxy(&config, "dc.example.invalid", 443).await?;
    let mut data = [0; 2];
    socket.read_exact(&mut data).await?;
    assert_eq!(&data, b"ok");
    server.await??;
    Ok(())
}
