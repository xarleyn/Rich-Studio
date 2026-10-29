"""Public Telegram accounts: real signature flow, isolated data and final Bot API permission checks."""

import asyncio
import hashlib
import hmac
import io
import json
import time
import zipfile
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import parse_qs, urlsplit

import pytest

import account_context
import security
import settings
import store
from errors import StudioError
from services import accounts, scheduler, updates
from test_api import BLOCKS_POST
from test_mcp import rpc


@pytest.fixture
def public(client, monkeypatch, tg):
    client.cookies.clear()
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "public_url", "https://testserver")
    yield client
    client.cookies.clear()


def signed(uid=101, **extra):
    fields = {"id": str(uid), "first_name": "Пользователь", "auth_date": str(int(time.time())), **extra}
    check = "\n".join(f"{k}={v}" for k, v in sorted(fields.items()))
    key = hashlib.sha256(settings.get().bot_token.encode()).digest()
    return {**fields, "hash": hmac.new(key, check.encode(), hashlib.sha256).hexdigest()}


def challenge(client):
    result = client.post("/api/auth/telegram/start", json={})
    assert result.status_code == 200
    return parse_qs(urlsplit(result.json()["auth_url"]).query)["state"][0]


def login_as(client, uid=101):
    state = challenge(client)
    response = client.get("/auth/telegram/callback", params={"state": state, **signed(uid)}, follow_redirects=False)
    assert response.status_code == 303, response.text
    secret = client.cookies.get(security.SESSION_COOKIE)
    return {"Cookie": f"{security.SESSION_COOKIE}={secret}"}


def connect(client, headers):
    response = client.post("/api/channels", json={"chat_id": "@mine"}, headers=headers)
    assert response.status_code == 200, response.text
    return response.json()["channel"]


def test_public_is_closed_without_operator_keys(public):
    assert public.get("/api/drafts").status_code == 401
    assert public.get("/api/session").status_code == 401
    assert public.post("/api/send", json={"post": BLOCKS_POST}).status_code == 401
    assert public.get("/").status_code == 200
    assert public.get("/api/auth/config").json() == {"telegram": True, "bot_username": "YourBot"}


def test_signed_login_cookie_identity_and_logout(public):
    login_as(public)
    session = public.get("/api/session").json()
    assert session["user"]["telegram_id"] == 101 and session["level"] == "publish"
    assert session["name"] == "tg:101"
    assert public.get("/api/config").json()["default_chat"] == ""
    assert public.get("/api/config").json()["preview_chat"] == "101"
    assert account_context.user_id() is None
    assert public.delete("/api/session").status_code == 200
    assert public.get("/api/drafts").status_code == 401


@pytest.mark.parametrize("changes", [{"id": "999"}, {"hash": "подделка"}, {"first_name": "другое"}])
def test_forged_identity_is_refused(public, changes):
    state = challenge(public)
    response = public.get("/auth/telegram/callback", params={"state": state, **signed(), **changes})
    assert response.status_code == 401 and not store.accounts.ids()


@pytest.mark.parametrize("at", [lambda: time.time() - 600, lambda: time.time() + 600])
def test_old_and_future_signatures_are_refused(public, at):
    state = challenge(public)
    response = public.get("/auth/telegram/callback", params={"state": state, **signed(auth_date=str(int(at())))})
    assert response.status_code == 401


def test_login_requires_browser_challenge_and_cannot_be_replayed(public):
    state = challenge(public)
    params = {"state": state, **signed()}
    cookies = {"Cookie": f"rs_telegram_login={state}"}
    public.cookies.clear()
    assert public.get("/auth/telegram/callback", params=params).status_code == 401
    assert (
        public.get("/auth/telegram/callback", params=params, headers=cookies, follow_redirects=False).status_code == 303
    )
    assert public.get("/auth/telegram/callback", params=params, headers=cookies).status_code == 401
    pairs = [*params.items(), ("id", "999")]
    assert public.get("/auth/telegram/callback", params=pairs, headers=cookies).status_code == 401


def test_session_is_httponly_secure_and_challenge_is_lax(public):
    response = public.post("/api/auth/telegram/start", json={}, headers={"X-Forwarded-Proto": "https"})
    assert all(flag in response.headers["set-cookie"] for flag in ("HttpOnly", "Secure", "SameSite=lax"))
    state = parse_qs(urlsplit(response.json()["auth_url"]).query)["state"][0]
    response = public.get(
        "/auth/telegram/callback",
        params={"state": state, **signed()},
        headers={"Cookie": f"rs_telegram_login={state}", "X-Forwarded-Proto": "https"},
        follow_redirects=False,
    )
    assert all(flag in response.headers["set-cookie"] for flag in ("HttpOnly", "Secure", "SameSite=strict"))
    assert response.headers["cache-control"] == "no-store"


def test_accounts_and_operator_have_separate_drafts_and_files(public):
    store.save_draft(None, "Личный черновик владельца", BLOCKS_POST)
    first = login_as(public, 101)
    created = public.post("/api/drafts", headers=first, json={"title": "Первый", "post": BLOCKS_POST}).json()
    assert len(public.get("/api/drafts", headers=first).json()) == 1
    public.post("/api/media", headers=first, files={"file": ("mine.txt", b"first")})
    second = login_as(public, 202)
    assert public.get("/api/drafts", headers=second).json() == []
    assert public.get("/api/media", headers=second).json() == []
    assert public.get(f"/api/drafts/{created['id']}", headers=second).status_code == 404
    assert public.get(f"/render/draft/{created['id']}", headers=second).status_code == 404
    assert public.get("/media/mine.txt", headers=second).status_code == 404
    public.post("/api/media", headers=second, files={"file": ("mine.txt", b"second")})
    assert public.get("/media/mine.txt", headers=first).content == b"first"
    assert public.get("/media/mine.txt", headers=second).content == b"second"
    assert public.get("/media/..%2F..%2Fstudio.db", headers=second).status_code == 404
    assert [d["title"] for d in store.list_drafts()] == ["Личный черновик владельца"]


def test_only_administrators_can_connect_channels(public, tg):
    who = login_as(public)
    tg.queue["getChatMember"] = [
        {"ok": True, "result": {"status": "administrator", "can_post_messages": True}},
        {"ok": True, "result": {"status": "member"}},
    ]
    assert public.post("/api/channels", json={"chat_id": "@someone"}, headers=who).status_code == 403
    assert public.get("/api/channels", headers=who).json() == []
    channel = connect(public, who)
    assert channel["chat_id"] == "-1001"
    other = login_as(public, 202)
    assert public.get("/api/channels", headers=other).json() == []


def test_bot_needs_admin_posting_rights(public, tg):
    who = login_as(public)
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "member"}}]
    assert public.post("/api/channels", json={"chat_id": "@mine"}, headers=who).status_code == 403
    assert public.get("/api/channels", headers=who).json() == []


def test_public_publish_cannot_use_operator_default_or_unlinked_chat(public, tg):
    who = login_as(public)
    assert public.post("/api/send", json={"post": BLOCKS_POST}, headers=who).status_code == 400
    for path, body in [
        ("/api/send", {"post": BLOCKS_POST}),
        ("/api/edit", {"post": BLOCKS_POST, "message_id": 100}),
        ("/api/schedule", {"post": BLOCKS_POST, "at": int(time.time()) + 120}),
        ("/api/preview-send", {"post": BLOCKS_POST}),
    ]:
        assert public.post(path, json={**body, "chat_id": "@stranger"}, headers=who).status_code == 403
    assert not any(c["method"].startswith("send") or c["method"].startswith("edit") for c in tg.calls)


def test_send_rechecks_rights_and_uses_numeric_channel(public, tg):
    who = login_as(public)
    connect(public, who)
    response = public.post("/api/send", json={"post": BLOCKS_POST, "chat_id": "-1001"}, headers=who)
    assert response.status_code == 200, response.text
    assert tg.last("sendRichMessage")["body"]["chat_id"] == -1001
    tg.calls.clear()
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "member"}}]
    assert public.post("/api/send", json={"post": BLOCKS_POST, "chat_id": "-1001"}, headers=who).status_code == 403
    assert not any(c["method"] == "sendRichMessage" for c in tg.calls)


def test_preview_goes_to_the_signed_in_user(public, tg):
    who = login_as(public)
    assert public.post("/api/preview-send", json={"post": BLOCKS_POST}, headers=who).status_code == 200
    assert tg.last("sendRichMessage")["body"]["chat_id"] == 101


@pytest.mark.parametrize("path", ["/api/send", "/api/preview-send"])
@pytest.mark.parametrize("encoded", [False, True])
def test_cross_account_reply_source_is_refused(public, tg, path, encoded):
    who = login_as(public)
    connect(public, who)
    reply = {"chat_id": -100999, "message_id": 1}
    post = {**BLOCKS_POST, "options": {"reply_parameters": json.dumps(reply) if encoded else reply}}
    tg.calls.clear()
    response = public.post(path, headers=who, json={"post": post, "chat_id": "-1001"})
    assert response.status_code == 403
    assert not any(c["method"].startswith("send") for c in tg.calls)


@pytest.mark.parametrize("method", ["sendPoll", "sendLivePhoto", "sendPaidMedia"])
def test_reply_source_guard_applies_to_every_transport_method(public, method):
    login_as(public)

    with accounts.scope(101), pytest.raises(StudioError):
        asyncio.run(accounts.authorize(method, {"chat_id": 101, "reply_parameters": {"chat_id": -100999}}))


@pytest.mark.parametrize("source", [-1001, "@mine"])
def test_reply_to_own_linked_channel_is_allowed_and_rechecks_rights(public, tg, source):
    who = login_as(public)
    tg.queue["getChat"] = [{"ok": True, "result": {"id": -1001, "type": "channel", "title": "Мой", "username": "mine"}}]
    connect(public, who)
    post = {**BLOCKS_POST, "options": {"reply_parameters": {"chat_id": source, "message_id": 1}}}
    assert public.post("/api/preview-send", headers=who, json={"post": post}).status_code == 200
    assert tg.last("sendRichMessage")["body"]["reply_parameters"]["chat_id"] == -1001
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "member"}}]
    tg.calls.clear()
    assert public.post("/api/preview-send", headers=who, json={"post": post}).status_code == 403
    assert not any(c["method"].startswith("send") for c in tg.calls)


@pytest.mark.parametrize(
    ("method", "right"),
    [
        ("createChatInviteLink", "can_invite_users"),
        ("editMessageText", "can_edit_messages"),
        ("deleteMessage", "can_delete_messages"),
        ("pinChatMessage", "can_edit_messages"),
    ],
)
def test_user_operation_right_is_required_even_when_bot_has_it(public, tg, method, right):
    who = login_as(public)
    connect(public, who)
    member = {"status": "administrator", "can_post_messages": True}
    tg.queue["getChatMember"] = [{"ok": True, "result": member}]
    with accounts.scope(101), pytest.raises(StudioError, match=right):
        asyncio.run(accounts.authorize(method, {"chat_id": -1001}))
    tg.queue["getChatMember"] = [{"ok": True, "result": {**member, right: True}}]
    with accounts.scope(101):
        assert asyncio.run(accounts.authorize(method, {"chat_id": -1001}))["chat_id"] == -1001


def test_operator_endpoints_and_logs_are_unavailable_to_users(public):
    who = login_as(public)
    for path in ("/api/logs", "/api/export"):
        assert public.get(path, headers=who).status_code == 403
    assert public.post("/api/raw/getMe", json={}, headers=who).status_code == 403
    assert public.post("/api/spec/refresh", json={}, headers=who).status_code == 403
    health = public.get("/api/health", headers=who).json()
    assert health["metrics"] == {} and health["recent_problems"] == []
    assert {c["name"] for c in health["checks"]} == {"scheduler", "posts", "sends", "spec"}
    result = rpc(public, "tools/call", {"name": "studio_logs", "arguments": {}}, headers=who)["result"]
    assert result["isError"]


def test_paid_broadcast_is_refused_before_sending(public, tg):
    who = login_as(public)
    connect(public, who)
    post = {**BLOCKS_POST, "options": {"allow_paid_broadcast": True}}
    assert public.post("/api/send", json={"post": post, "chat_id": "-1001"}, headers=who).status_code == 403
    assert not any(c["method"] == "sendRichMessage" for c in tg.calls)


def test_scheduler_handles_accounts_and_revoked_rights(public, tg):
    first = login_as(public, 101)
    connect(public, first)
    second = login_as(public, 202)
    connect(public, second)
    for who in (first, second):
        response = public.post(
            "/api/schedule", headers=who, json={"chat_id": "-1001", "post": BLOCKS_POST, "at": int(time.time())}
        )
        assert response.status_code == 200
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "member"}}]
    assert asyncio.run(scheduler.run_due_posts()) == 2
    assert public.get("/api/posts", headers=first).json()[0]["status"] == "failed"
    assert public.get("/api/posts", headers=second).json()[0]["status"] == "sent"
    assert store.list_posts() == []


def test_telegram_events_reach_only_the_owning_account(public, tg):
    first = login_as(public, 101)
    connect(public, first)
    response = public.post("/api/send", headers=first, json={"chat_id": "-1001", "post": BLOCKS_POST}).json()
    p = response["post"]
    second = login_as(public, 202)
    event = {
        "message_reaction_count": {
            "chat": {"id": -1001},
            "message_id": p["message_id"],
            "reactions": [{"type": {"type": "emoji", "emoji": "👍"}, "total_count": 3}],
        }
    }
    assert asyncio.run(updates.handle_update(event)) == "reactions"
    assert public.get(f"/api/posts/{p['id']}", headers=first).json()["reactions"] == {"👍": 3}
    assert public.get("/api/posts", headers=second).json() == []
    assert store.list_posts() == []


def test_adding_bot_connects_channel_to_the_actor(public):
    first = login_as(public, 101)
    second = login_as(public, 202)
    event = {
        "my_chat_member": {"from": {"id": 101}, "chat": {"id": -1001}, "new_chat_member": {"status": "administrator"}}
    }
    assert asyncio.run(updates.handle_update(event)) == "channel_connected"
    assert len(public.get("/api/channels", headers=first).json()) == 1
    assert public.get("/api/channels", headers=second).json() == []
    assert store.list_channels() == []


def test_parallel_requests_keep_their_account_context(public):
    first, second = login_as(public, 101), login_as(public, 202)

    def save(i):
        who = first if i % 2 else second
        response = public.post("/api/drafts", headers=who, json={"title": str(i % 2), "post": BLOCKS_POST})
        assert response.status_code == 200

    with ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(save, range(12)))
    assert [d["title"] for d in public.get("/api/drafts", headers=first).json()] == ["1"] * 6
    assert [d["title"] for d in public.get("/api/drafts", headers=second).json()] == ["0"] * 6
    assert store.list_drafts() == []


@pytest.mark.parametrize("first_revoked", [False, True])
def test_partial_send_keeps_success_and_idempotency_after_rights_are_revoked(public, tg, first_revoked):
    who = login_as(public)
    connect(public, who)
    tg.queue["getChat"] = [{"ok": True, "result": {"id": -1002, "type": "channel", "title": "Второй"}}]
    assert public.post("/api/channels", headers=who, json={"chat_id": "@second"}).status_code == 200
    tg.queue["getChatMember"] = [
        {"ok": True, "result": {"status": "administrator", "can_post_messages": True}},
        {"ok": True, "result": {"status": "member"}},
    ]
    if first_revoked:
        tg.queue["getChatMember"].reverse()
    headers = {**who, "Idempotency-Key": "partial"}
    body = {"post": BLOCKS_POST, "chat_ids": ["-1001", "-1002"]}
    response = public.post("/api/send", headers=headers, json=body)
    assert response.status_code == 200
    assert [r["ok"] for r in response.json()["results"]] == ([False, True] if first_revoked else [True, False])
    count = sum(c["method"] == "sendRichMessage" for c in tg.calls)
    assert public.post("/api/send", headers=headers, json=body).json() == response.json()
    assert sum(c["method"] == "sendRichMessage" for c in tg.calls) == count == 1


def test_quota_removes_partial_upload_and_other_accounts_have_their_own_quota(public, monkeypatch):
    monkeypatch.setattr(settings.get(), "account_storage_mb", 4 / 1024**2)
    first = login_as(public, 101)
    assert public.post("/api/media", headers=first, files={"file": ("one.txt", b"1234")}).status_code == 200
    assert public.post("/api/media", headers=first, files={"file": ("two.txt", b"x")}).status_code == 413
    assert [f["name"] for f in public.get("/api/media", headers=first).json()] == ["one.txt"]
    second = login_as(public, 202)
    assert public.post("/api/media", headers=second, files={"file": ("two.txt", b"1234")}).status_code == 200


def test_login_flood_is_limited_and_logout_still_works(public):
    login_as(public)
    for _ in range(30):
        response = public.post("/api/auth/telegram/start", json={})
    assert response.status_code == 429 and response.headers["retry-after"] == "60"
    assert public.delete("/api/session").status_code == 200


def test_public_requires_https_server_configuration():
    with pytest.raises(settings.SettingsError, match="STUDIO_MODE=server"):
        settings.Settings.from_env({"STUDIO_PUBLIC": "on"})
    cfg = settings.Settings.from_env(
        {"STUDIO_PUBLIC": "on", "STUDIO_MODE": "server", "STUDIO_URL": "https://studio.example", "TG_BOT_TOKEN": "1:x"}
    )
    assert cfg.public and cfg.bot_username == "YourBot"


def test_operator_export_includes_account_databases_and_media(public):
    who = login_as(public)
    public.post("/api/drafts", headers=who, json={"title": "Мой пост", "post": BLOCKS_POST})
    public.post("/api/media", headers=who, files={"file": ("mine.txt", b"mine")})
    store.add_token("owner", "owner-secret", "admin")
    response = public.get("/api/export", headers={"Authorization": "Bearer owner-secret"})
    assert response.status_code == 200
    with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
        assert {"studio.db", "accounts/101/studio.db", "accounts/101/media/mine.txt"} <= set(archive.namelist())
        assert archive.read("accounts/101/media/mine.txt") == b"mine"


def test_landing_host_and_app_host_are_separate(public, monkeypatch):
    cfg = settings.get()
    monkeypatch.setattr(cfg, "landing_url", "https://example.com")
    monkeypatch.setattr(cfg, "public_url", "https://app.example.com")
    monkeypatch.setattr(cfg, "allowed_hosts", {*cfg.allowed_hosts, "example.com", "app.example.com"})
    landing = public.get("/", headers={"Host": "example.com"})
    assert landing.status_code == 200 and "Хороший пост." in landing.text
    assert 'href="https://app.example.com"' in landing.text
    assert 'id="title"' not in landing.text
    assert "https://telegram.org" not in landing.headers["content-security-policy"]
    app = public.get("/", headers={"Host": "app.example.com"})
    assert app.status_code == 200 and 'id="title"' in app.text
    assert "https://telegram.org" in app.headers["content-security-policy"]
    response = public.get("/api/session", headers={"Host": "example.com"}, follow_redirects=False)
    assert response.status_code == 307 and response.headers["location"] == "https://app.example.com/api/session"
    assert public.post("/api/auth/telegram/start", headers={"Host": "example.com"}, json={}).status_code == 403
    response = public.post("/api/auth/telegram/start", headers={"Host": "app.example.com"}, json={})
    assert response.json()["auth_url"].startswith("https://app.example.com/auth/telegram/callback?")
    assert "Domain=" not in response.headers["set-cookie"]


def test_landing_configuration_requires_a_separate_https_host():
    base = {
        "STUDIO_MODE": "server",
        "STUDIO_URL": "https://app.example.com",
        "TG_BOT_TOKEN": "1:x",
        "STUDIO_PUBLIC": "on",
    }
    cfg = settings.Settings.from_env({**base, "STUDIO_LANDING_URL": "https://example.com"})
    assert {"example.com", "app.example.com"} <= cfg.allowed_hosts
    assert cfg.is_landing_host("example.com") and not cfg.is_landing_host("app.example.com")
    for url in ("http://example.com", "https://app.example.com", "https://example.com/sub", "https://user@example.com"):
        with pytest.raises(settings.SettingsError, match="STUDIO_LANDING_URL"):
            settings.Settings.from_env({**base, "STUDIO_LANDING_URL": url})


def test_username_and_numeric_id_of_one_channel_do_not_send_twice(public, tg):
    who = login_as(public)
    tg.queue["getChat"] = [
        {"ok": True, "result": {"id": -1001, "type": "channel", "title": "Мой канал", "username": "mine"}}
    ]
    connect(public, who)
    response = public.post("/api/send", headers=who, json={"post": BLOCKS_POST, "chat_ids": ["@mine", "-1001"]})
    assert response.status_code == 200
    assert len(response.json()["results"]) == 1
    assert sum(c["method"] == "sendRichMessage" for c in tg.calls) == 1
