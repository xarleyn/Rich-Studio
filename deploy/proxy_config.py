#!/usr/bin/env python3
"""Import a private Trojan share link into a selective sing-box configuration. Never log the link."""

import argparse
import json
import os
import tempfile
from pathlib import Path
from urllib.parse import parse_qsl, unquote, urlsplit


class ProfileError(ValueError):
    """An unsupported profile; messages contain no user-supplied values."""


def build(uri: str) -> dict:
    try:
        link = urlsplit(uri.strip())
        host = link.hostname
        port = 443 if link.port is None else link.port
    except ValueError:
        raise ProfileError("Повреждена ссылка Trojan.") from None
    if link.scheme != "trojan" or not host or "@" not in link.netloc or link.path not in {"", "/"}:
        raise ProfileError("Нужна ссылка одного узла trojan:// из NekoBox.")
    password = unquote(link.netloc.rsplit("@", 1)[0])
    if not password or not 1 <= port <= 65535:
        raise ProfileError("В ссылке нет пароля или порта Trojan.")
    pairs = parse_qsl(link.query, keep_blank_values=True)
    if len({key for key, _ in pairs}) != len(pairs):
        raise ProfileError("В ссылке повторяются параметры.")
    query = dict(pairs)
    known = {
        "security",
        "type",
        "sni",
        "peer",
        "host",
        "path",
        "serviceName",
        "service_name",
        "alpn",
        "fp",
        "allowInsecure",
        "insecure",
        "remarks",
        "encryption",
        "headerType",
        "tfo",
        "udp",
        "packetEncoding",
        "mux",
    }
    if any(value for key, value in pairs if key not in known):
        raise ProfileError("Ссылка содержит неподдерживаемые параметры. Нужен экспорт узла sing-box.")
    if any(query.get(key, "").lower() in {"1", "true", "yes"} for key in ("allowInsecure", "insecure")):
        raise ProfileError("Профиль отключает проверку TLS. Нужен профиль с проверкой сертификата.")
    if query.get("security", "tls") not in {"", "tls"} or query.get("headerType", "none") not in {"", "none"}:
        raise ProfileError("Этот режим TLS или заголовков пока не поддержан.")
    if query.get("mux", "").lower() not in {"", "0", "false"}:
        raise ProfileError("Экспортируйте узел без multiplex или в формате sing-box.")
    tls: dict = {"enabled": True, "server_name": query.get("sni") or query.get("peer") or host}
    if query.get("alpn"):
        tls["alpn"] = [part.strip() for part in query["alpn"].split(",") if part.strip()]
    if query.get("fp"):
        tls["utls"] = {"enabled": True, "fingerprint": query["fp"]}
    outbound: dict = {
        "type": "trojan",
        "tag": "vpn",
        "server": host,
        "server_port": port,
        "password": password,
        "network": "tcp",
        "tls": tls,
    }
    transport = query.get("type", "tcp") or "tcp"
    if transport in {"tcp", "raw", "none"}:
        pass
    elif transport == "ws":
        outbound["transport"] = {"type": "ws", "path": query.get("path") or "/"}
        if query.get("host"):
            outbound["transport"]["headers"] = {"Host": query["host"]}
    elif transport == "grpc":
        outbound["transport"] = {
            "type": "grpc",
            "service_name": query.get("serviceName") or query.get("service_name", ""),
        }
    elif transport in {"http", "h2"}:
        outbound["transport"] = {"type": "http", "path": query.get("path") or "/"}
        if query.get("host"):
            outbound["transport"]["host"] = [query["host"]]
    else:
        raise ProfileError("Транспорт узла пока не поддержан. Нужен экспорт sing-box.")
    if query.get("tfo", "").lower() in {"1", "true"}:
        outbound["tcp_fast_open"] = True
    return {
        "log": {"disabled": True},
        "dns": {"servers": [{"type": "local", "tag": "local"}]},
        # Internal Docker network only; compose does not publish the proxy port.
        "inbounds": [{"type": "http", "tag": "http-in", "listen": "0.0.0.0", "listen_port": 3128}],  # noqa: S104
        "outbounds": [outbound, {"type": "direct", "tag": "direct"}],
        "route": {
            "default_domain_resolver": {"server": "local", "strategy": "prefer_ipv4"},
            "rules": [
                {"domain": ["api.telegram.org"], "action": "route", "outbound": "vpn"},
                {"domain_suffix": ["letsencrypt.org", "lencr.org"], "action": "route", "outbound": "vpn"},
            ],
            "final": "direct",
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    temporary = None
    try:
        config = build(args.input.read_text())
        with tempfile.NamedTemporaryFile(mode="w", dir=args.output.parent, delete=False) as file:
            temporary = Path(file.name)
            json.dump(config, file)
            file.flush()
            os.fchmod(file.fileno(), 0o400)
            if os.geteuid() == 0:
                os.fchown(file.fileno(), 10002, 10002)
        temporary.replace(args.output)
    except ProfileError as error:
        print(error)
        return 1
    except (OSError, UnicodeError):
        print("Не удалось прочитать или сохранить приватный профиль.")
        return 1
    finally:
        if temporary:
            temporary.unlink(missing_ok=True)
    print("Приватная конфигурация подготовлена: Telegram и Let's Encrypt через Trojan.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
