"""Only Bot API uses TG_PROXY_URL; media downloads keep their independent transport."""

import asyncio
import importlib.util
import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import settings
import telegram

_spec = importlib.util.spec_from_file_location("proxy_config", Path(__file__).parents[1] / "deploy/proxy_config.py")
assert _spec and _spec.loader
profile = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(profile)


@contextmanager
def server():
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append((self.path, self.rfile.read(int(self.headers.get("Content-Length", 0)))))
            body = json.dumps({"ok": True, "result": {"id": 123, "is_bot": True}}).encode()
            self.send_response(200)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            requests.append((self.path, b""))
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.end_headers()
            self.wfile.write(b"fixture-media")

        def log_message(self, *args):
            pass

    http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=http.serve_forever)
    thread.start()
    try:
        yield f"http://127.0.0.1:{http.server_port}", requests
    finally:
        http.shutdown()
        thread.join()
        http.server_close()


def test_bot_api_reaches_explicit_proxy_even_without_destination_dns(monkeypatch, fresh_data):
    with server() as (proxy, requests):
        monkeypatch.setattr(settings.get(), "api_base", "http://bot-api.invalid")
        monkeypatch.setattr(settings.get(), "telegram_proxy", proxy)
        assert asyncio.run(telegram.call("getMe"))["id"] == 123
        # Multipart uploads go through the same proxy without rewriting the Bot API payload.
        (settings.get().media_dir / "photo.png").write_bytes(b"upload-content")
        asyncio.run(telegram.call("sendPhoto", {"chat_id": "123", "photo": "upload:photo.png"}))
        assert requests[0][0] == "http://bot-api.invalid/bot123:TEST/getMe"
        assert requests[1][0].endswith("/sendPhoto") and b"upload-content" in requests[1][1]


def test_media_fetch_does_not_use_telegram_proxy(client, monkeypatch):
    with server() as (proxy, proxied), server() as (origin, direct):
        monkeypatch.setattr(settings.get(), "telegram_proxy", proxy)
        monkeypatch.setattr(settings.get(), "media_fetch_private", True)
        response = client.post("/api/media/fetch", json={"url": origin + "/photo.png"})
        assert response.status_code == 200, response.text
        assert direct == [("/photo.png", b"")] and proxied == []


def test_proxy_settings_support_secret_files_and_do_not_echo_invalid_credentials(tmp_path):
    secret = tmp_path / "proxy"
    secret.write_text("http://user:private-password@egress:3128\n")
    assert settings.Settings.from_env({"TG_PROXY_URL_FILE": str(secret)}).telegram_proxy == secret.read_text().strip()
    for uri in ("socks5://user:private-password@egress:1080", "http://egress:wrong/private-password"):
        with pytest.raises(settings.SettingsError) as error:
            settings.Settings.from_env({"TG_PROXY_URL": uri})
        assert "TG_PROXY_URL" in str(error.value) and "private-password" not in str(error.value)


@pytest.mark.parametrize("kind", ["tcp", "ws", "grpc", "h2"])
def test_trojan_import_keeps_tls_and_selective_domain_routes(kind):
    config = profile.build(
        f"trojan://pass%3Aword%40test@vpn.example.com:8443?type={kind}&sni=tls.example.com&host=cdn.example.com"
        "&path=%2Ftunnel&serviceName=rpc&alpn=h2,http%2F1.1&fp=chrome"
    )
    outbound = config["outbounds"][0]
    assert outbound["password"] == "pass:word@test"
    assert outbound["tls"] == {
        "enabled": True,
        "server_name": "tls.example.com",
        "alpn": ["h2", "http/1.1"],
        "utls": {"enabled": True, "fingerprint": "chrome"},
    }
    if kind == "ws":
        assert outbound["transport"] == {"type": "ws", "path": "/tunnel", "headers": {"Host": "cdn.example.com"}}
    elif kind == "grpc":
        assert outbound["transport"] == {"type": "grpc", "service_name": "rpc"}
    elif kind == "h2":
        assert outbound["transport"] == {"type": "http", "path": "/tunnel", "host": ["cdn.example.com"]}
    else:
        assert "transport" not in outbound
    assert config["route"]["final"] == "direct"
    assert config["route"]["rules"] == [
        {"domain": ["api.telegram.org"], "action": "route", "outbound": "vpn"},
        {"domain_suffix": ["letsencrypt.org", "lencr.org"], "action": "route", "outbound": "vpn"},
    ]
    assert config["log"]["disabled"] is True


@pytest.mark.parametrize(
    "suffix",
    [
        "?allowInsecure=1",
        "?security=reality",
        "?security=none&sni=tls.example.com",
        "?type=unknown",
        "?mux=1",
        "?sni=a&sni=b",
        "?unknown=value",
        ":wrong",
    ],
)
def test_unsupported_profile_never_echoes_credentials(suffix):
    with pytest.raises(profile.ProfileError) as error:
        profile.build("trojan://private-password@vpn.example.com:443" + suffix)
    assert "private-password" not in str(error.value) and "vpn.example.com" not in str(error.value)


@pytest.mark.parametrize("port", ["0", "65536", "wrong"])
def test_invalid_trojan_port_is_rejected(port):
    with pytest.raises(profile.ProfileError):
        profile.build(f"trojan://fixture-password@vpn.example.com:{port}")
