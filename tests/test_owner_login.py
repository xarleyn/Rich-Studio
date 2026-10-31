"""The service owner's Telegram login opens the root workspace (admin) without creating a public account."""

import pytest

import settings
import store
from test_public_accounts import challenge, signed


@pytest.fixture
def public(client, monkeypatch, tg):
    client.cookies.clear()
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "public_url", "https://testserver")
    yield client
    client.cookies.clear()


def telegram_login(client, uid):
    state = challenge(client)
    response = client.get("/auth/telegram/callback", params={"state": state, **signed(uid)}, follow_redirects=False)
    assert response.status_code == 303, response.text
    return response


def test_owner_login_opens_root_admin_session(public, monkeypatch):
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 555)
    telegram_login(public, 555)
    session = public.get("/api/session")
    assert session.status_code == 200
    body = session.json()
    assert body["name"] is None and body["level"] == "admin"
    assert "user" not in body
    # the root workspace answers: the whole-export endpoint is admin-only
    assert public.get("/api/export").status_code == 200
    # no public account appeared: not in the registry, no folder, no account session
    assert store.accounts.session_by_id(555) is None
    assert 555 not in store.accounts.ids()
    assert not (settings.get().data_dir / "accounts" / "555").exists()


def test_regular_user_alongside_configured_owner(public, monkeypatch):
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 555)
    telegram_login(public, 101)
    body = public.get("/api/session").json()
    assert body["level"] == "publish" and body["user"]["telegram_id"] == 101
    assert public.get("/api/export").status_code == 403


def test_without_owner_setting_the_same_id_is_a_regular_account(public):
    telegram_login(public, 555)
    body = public.get("/api/session").json()
    assert body["level"] == "publish" and body["user"]["telegram_id"] == 555
    assert 555 in store.accounts.ids()


def test_owner_session_dies_when_setting_changes(public, monkeypatch):
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 555)
    telegram_login(public, 555)
    assert public.get("/api/session").status_code == 200
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 0)
    assert public.get("/api/session").status_code == 401
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 777)
    assert public.get("/api/session").status_code == 401


def test_config_reports_owner_login(public, monkeypatch):
    assert public.get("/api/auth/config").json()["owner_login"] is False
    monkeypatch.setattr(settings.get(), "owner_telegram_id", 555)
    assert public.get("/api/auth/config").json()["owner_login"] is True
