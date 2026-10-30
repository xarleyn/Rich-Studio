"""Telegram webhook authentication, registration and delivery semantics."""

import asyncio

import pytest

import settings
from services import updates


@pytest.fixture
def webhook(monkeypatch):
    cfg = settings.get()
    monkeypatch.setattr(cfg, "updates_on", True)
    monkeypatch.setattr(cfg, "updates_mode", "webhook")
    monkeypatch.setattr(cfg, "webhook_secret", "test_secret-123")
    monkeypatch.setattr(cfg, "api_key", "studio-key")
    return cfg


def test_webhook_requires_telegram_secret_and_deduplicates(client, webhook, monkeypatch):
    seen = []

    async def handle(update):
        seen.append(update["update_id"])
        return "callback"

    monkeypatch.setattr(updates, "handle_update", handle)
    path = "/api/telegram/webhook"
    assert client.post(path, json={"update_id": 42}).status_code == 403
    assert (
        client.post(path, json={"update_id": 42}, headers={"X-Telegram-Bot-Api-Secret-Token": "wrong"}).status_code
        == 403
    )
    headers = {"X-Telegram-Bot-Api-Secret-Token": "test_secret-123"}
    assert client.post(path, json={"update_id": True}, headers=headers).status_code == 400
    assert client.post(path, json={"update_id": 42}, headers=headers).status_code == 200
    assert client.post(path, json={"update_id": 42}, headers=headers).status_code == 200
    assert seen == [42]
    monkeypatch.setattr(webhook, "updates_mode", "polling")
    assert client.post(path, json={"update_id": 43}, headers=headers).status_code == 404


def test_webhook_configuration_validation():
    with pytest.raises(settings.SettingsError):
        settings.Settings.from_env({"TG_UPDATES_MODE": "webhook", "STUDIO_URL": "https://example.org"})
    with pytest.raises(settings.SettingsError):
        settings.Settings.from_env(
            {"TG_UPDATES_MODE": "webhook", "TG_WEBHOOK_SECRET": "valid", "STUDIO_URL": "http://example.org"}
        )
    cfg = settings.Settings.from_env(
        {"TG_UPDATES_MODE": "webhook", "TG_WEBHOOK_SECRET": "valid", "STUDIO_URL": "https://example.org"}
    )
    assert cfg.updates_mode == "webhook"


def test_webhook_registration(webhook, monkeypatch):
    calls = []
    monkeypatch.setattr(webhook, "public_url", "https://example.org")

    async def call(method, params):
        calls.append((method, params))
        return True

    async def stop(_):
        raise asyncio.CancelledError

    monkeypatch.setattr(updates.tg, "call", call)
    monkeypatch.setattr(updates, "_sleep", stop)
    with pytest.raises(asyncio.CancelledError):
        asyncio.run(updates.webhook_loop())
    assert calls == [
        (
            "setWebhook",
            {
                "url": "https://example.org/api/telegram/webhook",
                "secret_token": "test_secret-123",
                "allowed_updates": updates.ALLOWED_UPDATES,
                "max_connections": 1,
            },
        )
    ]


def test_failed_handler_is_not_repeated(client, webhook, monkeypatch):
    seen = []

    async def handle(update):
        seen.append(update["update_id"])
        raise RuntimeError("failure after a possible side effect")

    monkeypatch.setattr(updates, "handle_update", handle)
    headers = {"X-Telegram-Bot-Api-Secret-Token": "test_secret-123"}
    for _ in range(2):
        assert client.post("/api/telegram/webhook", json={"update_id": 44}, headers=headers).status_code == 200
    assert seen == [44]
    assert updates.public_status()["mode"] == "webhook"
