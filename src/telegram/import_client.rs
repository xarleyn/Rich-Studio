//! Read-only MTProto transport and Telethon StringSession compatibility.
//! Authentication material is never formatted into errors or diagnostic output.

use crate::{errors::ImportError, settings::Settings};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use grammers_client::{
    Client, InvocationError,
    sender::{ConnectionParams, NoRetries, SenderPool, SenderPoolFatHandle},
};
use grammers_session::{Session, SessionData, storages::MemorySession};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

fn invalid_session() -> ImportError {
    ImportError::new(
        409,
        "Подключение Telegram для импорта требует повторного входа.",
    )
}
fn network() -> ImportError {
    ImportError::new(
        502,
        "Не удалось прочитать сообщение Telegram. Проверьте подключение и доступ к источнику.",
    )
}

pub fn decode_session(encoded: &str) -> Result<(SessionData, bool), ImportError> {
    let body = encoded
        .trim()
        .strip_prefix('1')
        .ok_or_else(invalid_session)?;
    let bytes = URL_SAFE
        .decode(body)
        .or_else(|_| URL_SAFE_NO_PAD.decode(body))
        .map_err(|_| invalid_session())?;
    let ip_len = match bytes.len() {
        263 => 4,
        275 => 16,
        _ => return Err(invalid_session()),
    };
    let dc = i32::from(bytes[0]);
    let mut data = SessionData::default();
    let mut option = data
        .dc_options
        .get(&dc)
        .cloned()
        .ok_or_else(invalid_session)?;
    let port = u16::from_be_bytes([bytes[1 + ip_len], bytes[2 + ip_len]]);
    if port == 0 {
        return Err(invalid_session());
    }
    if ip_len == 4 {
        option.ipv4 =
            SocketAddrV4::new(Ipv4Addr::new(bytes[1], bytes[2], bytes[3], bytes[4]), port);
    } else {
        let ip = <[u8; 16]>::try_from(&bytes[1..17]).map_err(|_| invalid_session())?;
        option.ipv6 = SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0);
    }
    let key = <[u8; 256]>::try_from(&bytes[3 + ip_len..]).map_err(|_| invalid_session())?;
    if key.iter().all(|byte| *byte == 0) {
        return Err(invalid_session());
    }
    option.auth_key = Some(key);
    data.home_dc = dc;
    data.dc_options.insert(dc, option);
    Ok((data, ip_len == 16))
}

pub fn encode_session(session: &MemorySession, ipv6: bool) -> Result<String, ImportError> {
    let dc = session.home_dc_id().map_err(|_| invalid_session())?;
    let option = session
        .dc_option(dc)
        .map_err(|_| invalid_session())?
        .ok_or_else(invalid_session)?;
    let key = option.auth_key.ok_or_else(invalid_session)?;
    let mut bytes = vec![u8::try_from(dc).map_err(|_| invalid_session())?];
    let port = if ipv6 {
        bytes.extend_from_slice(&option.ipv6.ip().octets());
        option.ipv6.port()
    } else {
        bytes.extend_from_slice(&option.ipv4.ip().octets());
        option.ipv4.port()
    };
    bytes.extend_from_slice(&port.to_be_bytes());
    bytes.extend_from_slice(&key);
    Ok(format!("1{}", URL_SAFE.encode(bytes)))
}

pub fn check_identity(
    own_session: bool,
    expected_uid: Option<i64>,
    actual_uid: i64,
    is_bot: bool,
) -> Result<(), ImportError> {
    if own_session && expected_uid.is_some_and(|uid| is_bot || uid != actual_uid) {
        return Err(ImportError::new(
            403,
            "Сессия импорта принадлежит другой учётке.",
        ));
    }
    if !own_session && !is_bot {
        return Err(ImportError::new(
            403,
            "Для общего подключения импорта требуется бот.",
        ));
    }
    Ok(())
}

pub fn rpc_error(error: InvocationError) -> ImportError {
    if let InvocationError::Rpc(ref rpc) = error
        && (rpc.name.starts_with("FLOOD_WAIT") || rpc.name.starts_with("FLOOD_PREMIUM_WAIT"))
    {
        return ImportError::new(
            429,
            format!(
                "Telegram просит подождать {} секунд перед импортом.",
                rpc.value.unwrap_or(0)
            ),
        );
    }
    network()
}

pub struct Connection {
    pub client: Client,
    pub session: Arc<MemorySession>,
    pub ipv6: bool,
    pub is_bot: bool,
    handle: SenderPoolFatHandle,
    runner: Option<JoinHandle<()>>,
    _proxy: Option<ProxyBridge>,
}

impl Connection {
    /// Also used by the explicit interactive import-login CLI; web handlers never prompt.
    pub async fn start(settings: &Settings, session: &str) -> Result<Self, ImportError> {
        let (data, ipv6) = if session.is_empty() {
            (SessionData::default(), false)
        } else {
            decode_session(session)?
        };
        let session = Arc::new(MemorySession::from(data));
        let proxy = match proxy_config(&settings.import_proxy)? {
            Some(config) => Some(ProxyBridge::start(config).await?),
            None => None,
        };
        let params = ConnectionParams {
            retry_policy: Box::new(NoRetries),
            use_ipv6: ipv6,
            proxy_url: proxy.as_ref().map(|bridge| bridge.url.clone()),
            ..Default::default()
        };
        let api_id = i32::try_from(settings.import_api_id)
            .map_err(|_| ImportError::new(409, "Не настроен TG_IMPORT_API_ID."))?;
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::with_configuration(Arc::clone(&session), api_id, params);
        drop(updates);
        let client = Client::new(handle.clone());
        let runner = Some(tokio::spawn(runner.run()));
        Ok(Self {
            client,
            session,
            ipv6,
            is_bot: false,
            handle,
            runner,
            _proxy: proxy,
        })
    }
    pub async fn connect(
        settings: &Settings,
        session: &str,
        expected_uid: Option<i64>,
    ) -> Result<Self, ImportError> {
        let mut connection = Self::start(settings, session).await?;
        let authenticate = async {
            if !connection.client.is_authorized().await.map_err(rpc_error)? {
                if !session.is_empty() || settings.bot_token.is_empty() {
                    return Err(invalid_session());
                }
                connection
                    .client
                    .bot_sign_in(&settings.bot_token, &settings.import_api_hash)
                    .await
                    .map_err(rpc_error)?;
            }
            let me = connection.client.get_me().await.map_err(rpc_error)?;
            check_identity(
                !session.is_empty(),
                expected_uid,
                me.id().bare_id().unwrap_or(0),
                me.is_bot(),
            )?;
            Ok(me.is_bot())
        };
        connection.is_bot = tokio::time::timeout(Duration::from_secs(15), authenticate)
            .await
            .map_err(|_| network())??;
        Ok(connection)
    }
    pub async fn close(mut self) {
        self.handle.quit();
        if let Some(runner) = self.runner.take() {
            let _ = runner.await;
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.handle.quit();
        if let Some(runner) = self.runner.take() {
            runner.abort();
        }
    }
}

#[derive(Clone)]
pub struct ProxyConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}
impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("scheme", &self.scheme)
            .field("configured", &true)
            .finish()
    }
}

fn unquote(value: &str) -> Result<String, ImportError> {
    let mut output = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(network());
            }
            let raw = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| network())?;
            output.push(u8::from_str_radix(raw, 16).map_err(|_| network())?);
            i += 3;
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(output).map_err(|_| network())
}
pub fn proxy_config(raw: &str) -> Result<Option<ProxyConfig>, ImportError> {
    if raw.is_empty() {
        return Ok(None);
    }
    let url =
        url::Url::parse(raw).map_err(|_| ImportError::new(422, "Неверный TG_IMPORT_PROXY_URL."))?;
    if !["http", "socks5"].contains(&url.scheme()) {
        return Err(ImportError::new(422, "Неверный TG_IMPORT_PROXY_URL."));
    }
    let host = url
        .host_str()
        .ok_or_else(network)?
        .trim_matches(['[', ']'])
        .to_owned();
    let username = (!url.username().is_empty())
        .then(|| unquote(url.username()))
        .transpose()?;
    let password = url.password().map(unquote).transpose()?;
    // Url::port() omits an explicitly specified default HTTP :80; Python preserves it.
    let explicit_port = raw
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .and_then(|authority| authority.rsplit_once(':'))
        .and_then(|(_, port)| port.parse::<u16>().ok());
    Ok(Some(ProxyConfig {
        scheme: url.scheme().to_owned(),
        host,
        port: explicit_port
            .or_else(|| url.port())
            .unwrap_or(if url.scheme() == "socks5" { 1080 } else { 8080 }),
        username,
        password,
    }))
}

/// Grammers accepts SOCKS5 only. This authenticated loopback adapter also preserves HTTP
/// CONNECT and decoded credentials supported by the Python importer. It dies with its owner.
struct ProxyBridge {
    url: String,
    runner: JoinHandle<()>,
}
impl ProxyBridge {
    async fn start(config: ProxyConfig) -> Result<Self, ImportError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| network())?;
        let address = listener.local_addr().map_err(|_| network())?;
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let url = format!("socks5://import:{secret}@{address}");
        let runner = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{let Ok((socket,_))=accepted else{break};if tasks.len()>=16{continue;}let config=config.clone();let secret=secret.clone();tasks.spawn(async move{let _=tunnel(socket,&config,&secret).await;});},
                    _=tasks.join_next(),if !tasks.is_empty()=>{}
                }
            }
        });
        Ok(Self { url, runner })
    }
}
impl Drop for ProxyBridge {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

async fn address(socket: &mut TcpStream, kind: u8) -> std::io::Result<(String, u16)> {
    let host = match kind {
        1 => {
            let mut bytes = [0; 4];
            socket.read_exact(&mut bytes).await?;
            Ipv4Addr::from(bytes).to_string()
        }
        4 => {
            let mut bytes = [0; 16];
            socket.read_exact(&mut bytes).await?;
            Ipv6Addr::from(bytes).to_string()
        }
        3 => {
            let len = socket.read_u8().await?;
            let mut bytes = vec![0; usize::from(len)];
            socket.read_exact(&mut bytes).await?;
            String::from_utf8(bytes).map_err(|_| std::io::ErrorKind::InvalidData)?
        }
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    };
    Ok((host, socket.read_u16().await?))
}
async fn tunnel(mut local: TcpStream, config: &ProxyConfig, secret: &str) -> std::io::Result<()> {
    let mut remote = tokio::time::timeout(Duration::from_secs(15), async {
        if local.read_u8().await? != 5 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let count = local.read_u8().await?;
        let mut methods = vec![0; usize::from(count)];
        local.read_exact(&mut methods).await?;
        if !methods.contains(&2) {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        local.write_all(&[5, 2]).await?;
        if local.read_u8().await? != 1 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let len = local.read_u8().await?;
        let mut user = vec![0; usize::from(len)];
        local.read_exact(&mut user).await?;
        let len = local.read_u8().await?;
        let mut password = vec![0; usize::from(len)];
        local.read_exact(&mut password).await?;
        if user != b"import" || password != secret.as_bytes() {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        local.write_all(&[1, 0]).await?;
        let mut header = [0; 4];
        local.read_exact(&mut header).await?;
        if header[..3] != [5, 1, 0] {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let (host, port) = address(&mut local, header[3]).await?;
        let mut remote = connect_proxy(config, &host, port).await?;
        local.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        remote.flush().await?;
        Ok::<_, std::io::Error>(remote)
    })
    .await
    .map_err(|_| std::io::ErrorKind::TimedOut)??;
    tokio::io::copy_bidirectional(&mut local, &mut remote).await?;
    Ok(())
}

pub async fn connect_proxy(
    config: &ProxyConfig,
    host: &str,
    port: u16,
) -> std::io::Result<TcpStream> {
    let mut remote = TcpStream::connect((config.host.as_str(), config.port)).await?;
    if config.scheme == "http" {
        if host.bytes().any(|byte| byte <= 32 || byte == 127) || host.contains(['\r', '\n']) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        let destination = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let auth = config
            .username
            .as_ref()
            .map(|user| {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    STANDARD.encode(format!(
                        "{user}:{}",
                        config.password.as_deref().unwrap_or("")
                    ))
                )
            })
            .unwrap_or_default();
        remote
            .write_all(
                format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n{auth}\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() >= 16384 {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            response.push(remote.read_u8().await?);
        }
        if std::str::from_utf8(&response)
            .ok()
            .and_then(|header| header.lines().next())
            .and_then(|status| status.split_whitespace().nth(1))
            != Some("200")
        {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
    } else {
        let auth = config.username.is_some();
        remote.write_all(&[5, 1, if auth { 2 } else { 0 }]).await?;
        let mut response = [0; 2];
        remote.read_exact(&mut response).await?;
        if response != [5, if auth { 2 } else { 0 }] {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        if let Some(user) = &config.username {
            let password = config.password.as_deref().unwrap_or("");
            let mut packet = vec![
                1,
                u8::try_from(user.len()).map_err(|_| std::io::ErrorKind::InvalidInput)?,
            ];
            packet.extend_from_slice(user.as_bytes());
            packet
                .push(u8::try_from(password.len()).map_err(|_| std::io::ErrorKind::InvalidInput)?);
            packet.extend_from_slice(password.as_bytes());
            remote.write_all(&packet).await?;
            remote.read_exact(&mut response).await?;
            if response != [1, 0] {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
        }
        let mut packet = vec![5, 1, 0];
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => {
                packet.push(1);
                packet.extend_from_slice(&ip.octets());
            }
            Ok(IpAddr::V6(ip)) => {
                packet.push(4);
                packet.extend_from_slice(&ip.octets());
            }
            Err(_) => {
                packet.push(3);
                packet
                    .push(u8::try_from(host.len()).map_err(|_| std::io::ErrorKind::InvalidInput)?);
                packet.extend_from_slice(host.as_bytes());
            }
        }
        packet.extend_from_slice(&port.to_be_bytes());
        remote.write_all(&packet).await?;
        let mut header = [0; 4];
        remote.read_exact(&mut header).await?;
        if header[..3] != [5, 0, 0] {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        let _ = address(&mut remote, header[3]).await?;
    }
    Ok(remote)
}
