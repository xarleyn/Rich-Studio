"""Installation assets are public; enabling installation never exposes account data."""

import struct

import settings


def test_pwa_assets_available_before_login(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    client.cookies.clear()
    assert client.get("/api/drafts").status_code == 401
    assert client.get("/media/private.png").status_code == 401

    worker = client.get("/sw.js")
    assert worker.status_code == 200
    assert worker.headers["cache-control"] == "no-cache"
    assert worker.headers["service-worker-allowed"] == "/"
    assert "javascript" in worker.headers["content-type"]

    manifest = client.get("/static/manifest.webmanifest")
    assert manifest.status_code == 200
    data = manifest.json()
    assert data["id"] == data["start_url"] == data["scope"] == "/"
    assert data["display"] == "standalone"
    assert {i["sizes"] for i in data["icons"] if i["purpose"] == "any"} >= {"192x192", "512x512"}
    assert any(i["purpose"] == "maskable" for i in data["icons"])
    for icon in data["icons"]:
        response = client.get(icon["src"])
        assert response.status_code == 200
        assert response.content[:8] == b"\x89PNG\r\n\x1a\n"
        width, height = struct.unpack("!II", response.content[16:24])
        assert f"{width}x{height}" == icon["sizes"]
    assert client.get("/static/icons/apple-touch-icon.png").status_code == 200

    offline = client.get("/static/offline.html")
    assert offline.status_code == 200
    assert "Нет подключения" in offline.text
    assert "You're offline" in offline.text
    assert "<script" not in offline.text


def test_pwa_linked_from_editor(client):
    page = client.get("/").text
    assert 'rel="manifest" href="/static/manifest.webmanifest"' in page
    assert 'rel="apple-touch-icon"' in page
    assert '<rs-install id="install-app"' in page
