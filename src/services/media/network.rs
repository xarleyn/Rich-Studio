//! Сетевая политика: внутренние имена, системный прокси, только публичные адреса.

use std::net::IpAddr;

use super::MediaError;

const INTERNAL_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".home.arpa",
    ".corp",
    ".private",
];

pub fn internal_name(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return false;
    }
    let name = host.to_lowercase();
    let name = name.trim_end_matches('.');
    !name.contains('.')
        || INTERNAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

pub(crate) fn system_proxy(scheme: &str, host: &str) -> bool {
    let variable = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .unwrap_or_default()
    };
    let no_proxy = variable("NO_PROXY");
    let host = host.trim_end_matches('.').to_lowercase();
    let bypassed = no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let entry = entry
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_lowercase();
            entry == "*" || host == entry || host.ends_with(&format!(".{entry}"))
        });
    !bypassed
        && (!variable(if scheme == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        })
        .is_empty()
            || !variable("ALL_PROXY").is_empty())
}

pub fn public_address(addresses: &[IpAddr], host: &str) -> Result<IpAddr, MediaError> {
    if addresses.is_empty() {
        return Err(MediaError::Fetch(format!(
            "не удалось скачать: адрес {host} не найден"
        )));
    }
    for address in addresses {
        let address = match address {
            IpAddr::V6(address) => address
                .to_ipv4_mapped()
                .map_or(IpAddr::V6(*address), IpAddr::V4),
            address => *address,
        };
        let non_public = match address {
            IpAddr::V4(address) => {
                let octets = address.octets();
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_broadcast()
                    || address.is_documentation()
                    || address.is_unspecified()
                    || address.is_multicast()
                    || octets[0] == 0
                    || octets[0] >= 240
                    || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                    || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                    || (octets[0] == 198 && (18..=19).contains(&octets[1]))
            }
            IpAddr::V6(address) => {
                address.is_unspecified()
                    || address.is_loopback()
                    || address.is_multicast()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
                    || (address.segments()[0] & 0xffc0) == 0xfe80
                    || address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8
            }
        };
        if non_public {
            return Err(refused(host));
        }
    }
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .copied()
        .or_else(|| addresses.first().copied())
        .ok_or_else(|| MediaError::Fetch(format!("не удалось скачать: адрес {host} не найден")))
}

pub(super) fn refused(host: &str) -> MediaError {
    MediaError::BadRequest(format!(
        "url: {host} — локальный или внутренний адрес, студия туда не ходит (свой сервер в локальной сети: MEDIA_FETCH_PRIVATE=1)"
    ))
}
