"""Browser-issued credentials stay inside the Telegram account over HTTP and real MCP."""

import time

import pytest

import settings
import store
from test_access import call_as, token
from test_api import BLOCKS_POST
from test_public_accounts import connect, login_as


@pytest.fixture
def public(client, monkeypatch, tg):
    client.cookies.clear()
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "public_url", "https://testserver")
    yield client
    client.cookies.clear()


def issue(client, level="draft", **extra):
    response = client.post("/api/account/tokens", json={"name": "Claude laptop", "level": level, **extra})
    assert response.status_code == 201, response.text
    assert response.headers["cache-control"] == "no-store"
    return response.json()


def bearer(issued):
    return {"Authorization": "Bearer " + issued["secret"]}


def test_browser_can_issue_list_and_revoke_own_token(public):
    login_as(public)
    created = issue(public)
    secret = created["secret"]
    listed = public.get("/api/account/tokens").json()
    assert listed["mcp_url"] == "https://testserver/mcp"
    assert listed["tokens"] == [created["token"]]
    assert secret not in str(listed) and "hash" not in str(listed)
    assert created["token"]["expires_at"] > time.time()
    with store.conn(root=True) as c:
        row = c.execute("select * from account_tokens").fetchone()
        assert row["hash"] == store.hash_secret(secret) and secret not in str(tuple(row))
    public.cookies.clear()
    assert public.get("/api/session", headers=bearer(created)).json()["user"]["telegram_id"] == 101
    login_as(public)
    assert public.get("/api/account/tokens").json()["tokens"][0]["last_used_at"]
    tid = created["token"]["id"]
    assert public.delete(f"/api/account/tokens/{tid}").status_code == 200
    assert public.delete(f"/api/account/tokens/{tid}").status_code == 200
    assert public.get("/api/drafts", headers=bearer(created)).status_code == 401
    assert public.get("/api/session").status_code == 200  # browser login survives revoke


def test_tokens_cannot_manage_credentials_or_become_browser_sessions(public):
    login_as(public)
    for level in ("read", "draft", "publish"):
        created = issue(public, level)
        headers = bearer(created)  # even alongside a valid Telegram cookie
        assert public.get("/api/account/tokens", headers=headers).status_code == 403
        assert public.post("/api/account/tokens", headers=headers, json={"name": "escalate"}).status_code == 403
        assert public.delete(f"/api/account/tokens/{created['token']['id']}", headers=headers).status_code == 403
        assert public.post("/api/session", json={"key": created["secret"]}).status_code == 403
    assert public.get("/api/session").json()["level"] == "publish"


def test_account_isolation_including_root_tokens_and_token_ids(public):
    root = token("operator", "admin")
    a = login_as(public, 101)
    first = issue(public)
    public.post("/api/drafts", json={"title": "A private", "post": BLOCKS_POST}, headers=a)
    login_as(public, 202)
    second = issue(public)  # same display name in another account
    assert public.get("/api/account/tokens").json()["tokens"] == [second["token"]]
    assert public.delete(f"/api/account/tokens/{first['token']['id']}").status_code == 404
    assert public.get("/api/account/tokens", headers=root).status_code == 403
    public.cookies.clear()
    _, own = call_as(public, bearer(first), "list_drafts")
    _, other = call_as(public, bearer(second), "list_drafts")
    _, operator = call_as(public, root, "list_drafts")
    assert "A private" in own and "A private" not in other and "A private" not in operator
    assert public.get("/api/drafts", headers={"X-API-Key": first["secret"]}).status_code == 200


def test_mcp_and_http_enforce_roles_and_current_telegram_rights(public, tg):
    login_as(public)
    tg.queue["getChat"] = [{"ok": True, "result": {"id": -1001, "type": "channel", "title": "Мой", "username": "mine"}}]
    connect(public, {})
    read, draft, publish_token = (bearer(issue(public, level)) for level in ("read", "draft", "publish"))
    public.cookies.clear()
    assert not call_as(public, read, "list_drafts")[0].get("isError")
    assert call_as(public, read, "save_draft", post=BLOCKS_POST)[0]["isError"]
    assert not call_as(public, draft, "save_draft", post=BLOCKS_POST)[0].get("isError")
    assert call_as(public, draft, "publish_post", post=BLOCKS_POST, chat_id="@mine")[0]["isError"]
    assert public.post("/api/send", json={"post": BLOCKS_POST}, headers=draft).status_code == 403
    for headers in (read, draft, publish_token):
        assert public.get("/api/logs", headers=headers).status_code == 403
        assert public.post("/api/raw/getMe", json={}, headers=headers).status_code == 403
        assert call_as(public, headers, "studio_logs")[0]["isError"]
    assert not call_as(public, publish_token, "publish_post", post=BLOCKS_POST, chat_id="@mine")[0].get("isError")
    tg.calls.clear()
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "member"}}]
    assert call_as(public, publish_token, "publish_post", post=BLOCKS_POST, chat_id="@mine", idempotency_key="new")[0][
        "isError"
    ]
    assert not any(c["method"].startswith("send") for c in tg.calls)


def test_expiration_revocation_and_public_mode_are_checked_per_request(public, monkeypatch):
    login_as(public)
    created = issue(public)
    public.cookies.clear()
    headers = bearer(created)
    assert public.get("/api/drafts", headers=headers).status_code == 200
    monkeypatch.setattr(settings.get(), "public", False)
    # In personal mode these credentials must not grant the operator's workspace.
    token("keep-auth-on", "read")
    assert public.get("/api/drafts", headers=headers).status_code == 401
    monkeypatch.setattr(settings.get(), "public", True)
    with store.conn(root=True) as c:
        c.execute("update account_tokens set expires_at=?", (int(time.time()),))
    assert public.get("/api/drafts", headers=headers).status_code == 401


@pytest.mark.parametrize(
    "body",
    [
        {"level": "admin"},
        {"name": "   "},
        {"name": "a\nb"},
        {"name": "x" * 81},
        {"expires_in_days": 0},
        {"expires_in_days": 366},
        {"expires_in_days": True},
        {"telegram_id": 202},
        {"secret": "chosen"},
    ],
)
def test_bad_or_privileged_input_is_rejected(public, body):
    login_as(public)
    r = public.post("/api/account/tokens", json={"name": "example", "level": "draft", **body})
    assert r.status_code == 422, r.text


def test_token_management_requires_telegram_login_and_same_origin(public):
    assert public.get("/api/account/tokens").status_code == 401
    login_as(public)
    assert (
        public.post("/api/account/tokens", json={"name": "x"}, headers={"Origin": "https://evil.example"}).status_code
        == 403
    )
    assert public.delete("/api/account/tokens/unknown", headers={"Sec-Fetch-Site": "cross-site"}).status_code == 403


def test_active_token_limit_and_revoke_frees_slot(public):
    from services import account_tokens

    login_as(public)
    created = [issue(public) for _ in range(account_tokens.MAX_ACTIVE)]
    assert public.post("/api/account/tokens", json={"name": "overflow"}).status_code == 409
    public.delete(f"/api/account/tokens/{created[0]['token']['id']}")
    assert issue(public, expires_in_days=None)["token"]["expires_at"] is None
