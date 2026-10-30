"""Regressions found in the architecture review (rich-studio-arch/architecture-review.md, P0/P1)."""

import asyncio
import time
from concurrent.futures import ThreadPoolExecutor

import httpx
import pytest

import settings
import store
from services import accounts, scheduler, updates
from test_api import BLOCKS_POST
from test_mcp import rpc
from test_public_accounts import connect, login_as
from test_public_accounts import public as public


def test_preview_never_falls_back_to_the_channel(client, tg, monkeypatch):
    monkeypatch.setattr(settings.get(), "preview_chat", "")
    r = client.post("/api/preview-send", json={"post": BLOCKS_POST})
    assert r.status_code == 400 and "TG_PREVIEW_CHAT" in r.json()["detail"]
    assert not [c for c in tg.calls if c["method"] == "sendRichMessage"]


def test_scheduled_post_stays_sent_when_pin_hits_network_error(client, tg, monkeypatch):
    import telegram

    real = telegram.call

    async def flaky(method, params=None, timeout=60):
        if method == "pinChatMessage":
            raise httpx.ReadTimeout("timeout")
        return await real(method, params, timeout)

    p = client.post("/api/schedule", json={"post": {**BLOCKS_POST, "pin": True}, "at": int(time.time())}).json()
    monkeypatch.setattr(telegram, "call", flaky)
    asyncio.run(scheduler.run_due_posts())
    rec = client.get("/api/posts/" + p["id"]).json()
    assert rec["status"] == "sent" and rec["message_id"]
    assert "закрепить" in rec["error"]


def test_network_error_in_one_channel_does_not_stop_the_rest(client, tg, monkeypatch):
    import telegram

    real = telegram.call
    n = {"sends": 0}

    async def flaky(method, params=None, timeout=60):
        if method == "sendRichMessage":
            n["sends"] += 1
            if n["sends"] == 2:
                raise httpx.ConnectError("down")
        return await real(method, params, timeout)

    monkeypatch.setattr(telegram, "call", flaky)
    r = client.post("/api/send", json={"post": BLOCKS_POST, "chat_ids": ["@a", "@b", "@c"]})
    assert r.status_code == 200
    oks = [x["ok"] for x in r.json()["results"]]
    assert oks == [True, False, True]


def test_single_channel_network_error_is_a_clear_502(client, tg, monkeypatch):
    import telegram

    async def down(method, params=None, timeout=60):
        raise httpx.ConnectError("down")

    monkeypatch.setattr(telegram, "call", down)
    r = client.post("/api/send", json={"post": BLOCKS_POST})
    assert r.status_code == 502 and "мог уйти" in r.json()["detail"]


def test_scheduler_reports_missing_chat_without_scaring(client, tg, monkeypatch):
    import store

    p = store.add_post("", BLOCKS_POST, None, "scheduled", scheduled_at=int(time.time()))
    monkeypatch.setattr(settings.get(), "default_chat", "")
    asyncio.run(scheduler.run_due_posts())
    rec = store.get_post(p["id"])
    assert rec["status"] == "failed" and "TG_DEFAULT_CHAT" in rec["error"] and "мог уйти" not in rec["error"]


def test_healthcheck_works_behind_api_key(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "api_key", "secret")
    assert client.get("/healthz").status_code == 200
    assert client.get("/api/config").status_code == 401


def test_favicon_is_not_a_404(client):
    assert client.get("/favicon.ico").status_code == 204


def test_poll_is_checked_like_a_poll_after_a_post(client, tg):
    """A scheduled poll Telegram would refuse is refused now, not at its time (prod review #8)."""
    import store

    poll = {"question": "Какой?", "options": [f"вариант {i}" for i in range(13)]}
    for body in ({"poll": poll}, {"poll": poll, "at": int(time.time()) + 3600}):
        r = client.post("/api/poll", json=body)
        assert r.status_code == 422 and "больше 12 вариантов" in str(r.json()["detail"])
    assert store.list_posts() == []
    assert not [c for c in tg.calls if c["method"] == "sendPoll"]


def test_removed_admin_receives_no_new_private_topics(public, tg):
    who = login_as(public)
    tg.queue["getChat"] = [{"ok": True, "result": {"id": -1001, "type": "supergroup", "is_forum": True}}]
    connect(public, who)
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "left"}}]
    asyncio.run(
        updates.handle_update(
            {
                "message": {
                    "chat": {"id": -1001},
                    "message_thread_id": 77,
                    "forum_topic_created": {"name": "Private new topic"},
                }
            }
        )
    )
    assert public.get("/api/channels", headers=who).json()[0]["topics"] == []


@pytest.mark.parametrize("allowed,linked", [(False, True), (True, False), (True, True)])
def test_first_comment_requires_current_source_rights_and_linked_discussion(public, tg, monkeypatch, allowed, linked):
    who = login_as(public)
    connect(public, who)
    monkeypatch.setattr(settings.get(), "updates_on", True)
    post = {**BLOCKS_POST, "comment": {"html": "<p>First comment</p>"}}
    response = public.post("/api/send", headers=who, json={"chat_id": "-1001", "post": post})
    assert response.status_code == 200
    saved = response.json()["post"]
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "creator" if allowed else "left"}}]
    tg.queue["getChat"] = [
        {
            "ok": True,
            "result": {
                "id": -1001,
                "type": "channel",
                "linked_chat_id": -1002 if linked else -1003,
            },
        }
    ]
    tg.calls.clear()
    asyncio.run(
        updates.handle_update(
            {
                "message": {
                    "chat": {"id": -1002},
                    "message_id": 900,
                    "is_automatic_forward": True,
                    "forward_origin": {"type": "channel", "chat": {"id": -1001}, "message_id": saved["message_id"]},
                }
            }
        )
    )
    sends = [c for c in tg.calls if c["method"] == "sendRichMessage"]
    assert len(sends) == int(allowed and linked)
    result = public.get("/api/posts/" + saved["id"], headers=who).json()
    assert result["comment_status"] == ("sent" if allowed and linked else "failed")


def test_public_database_growth_is_bounded_and_accounts_are_independent(public, monkeypatch):
    who = login_as(public)
    monkeypatch.setattr(settings.get(), "account_database_mb", 1, raising=False)
    payload = {"rich_message": {"html": "x" * 200_000}}
    statuses = []
    for i in range(8):
        r = public.post("/api/drafts", headers=who, json={"title": str(i), "post": payload})
        statuses.append(r.status_code)
    assert 413 in statuses and set(statuses) == {200, 413}
    with accounts.scope(101), store.conn() as c:
        assert c.execute("pragma page_count").fetchone()[0] * c.execute("pragma page_size").fetchone()[0] <= 1024**2
    other = login_as(public, 202)
    assert public.post("/api/drafts", headers=other, json={"post": payload}).status_code == 200


@pytest.mark.parametrize("status", ["left", "member", "administrator", "unavailable"])
def test_revoked_account_does_not_receive_reactions_or_block_other_admin(public, tg, status):
    first, second = login_as(public, 101), login_as(public, 202)
    records = []
    for uid in (101, 202):
        with accounts.scope(uid):
            store.save_channel("-1001", chat_num=-1001, type="channel")
            records.append(store.add_post("-1001", BLOCKS_POST, status="sent", message_id=77))
            store.update_post(records[-1]["id"], chat_num=-1001)
    tg.queue["getChatMember"] = [
        {"ok": False, "error_code": 403, "description": "Forbidden"}
        if status == "unavailable"
        else {"ok": True, "result": {"status": status, "can_post_messages": False}},
        {"ok": True, "result": {"status": "creator"}},
    ]
    asyncio.run(
        updates.handle_update(
            {
                "message_reaction_count": {
                    "chat": {"id": -1001},
                    "message_id": 77,
                    "reactions": [{"type": {"type": "emoji", "emoji": "👍"}, "total_count": 3}],
                }
            }
        )
    )
    assert not public.get("/api/posts/" + records[0]["id"], headers=first).json()["reactions"]
    assert public.get("/api/posts/" + records[1]["id"], headers=second).json()["reactions"] == {"👍": 3}


def test_database_full_after_a_send_never_frees_idempotency_key(public, tg, monkeypatch):
    from errors import StorageFull

    who = login_as(public)
    connect(public, who)
    update = store.update_post

    def full(pid, **fields):
        if fields.get("status") == "sent":
            raise StorageFull()
        return update(pid, **fields)

    monkeypatch.setattr(store, "update_post", full)
    headers = {**who, "Idempotency-Key": "quota-after-send"}
    body = {"chat_id": "-1001", "post": BLOCKS_POST}
    assert public.post("/api/send", headers=headers, json=body).status_code == 413
    monkeypatch.setattr(store, "update_post", update)
    assert public.post("/api/send", headers=headers, json=body).status_code == 502
    assert sum(c["method"] == "sendRichMessage" for c in tg.calls) == 1


def test_draft_count_limit_serializes_writers_and_allows_cleanup(public, monkeypatch):
    from store import drafts

    who = login_as(public)
    monkeypatch.setattr(drafts, "MAX_DRAFTS", 2)

    def create(i):
        return public.post("/api/drafts", headers=who, json={"title": str(i), "post": BLOCKS_POST})

    with ThreadPoolExecutor(max_workers=4) as pool:
        responses = list(pool.map(create, range(8)))
    assert sorted(r.status_code for r in responses) == [200, 200, 413, 413, 413, 413, 413, 413]
    did = next(r.json()["id"] for r in responses if r.status_code == 200)
    assert public.put("/api/drafts/" + did, headers=who, json={"post": BLOCKS_POST}).status_code == 200
    assert public.delete("/api/drafts/" + did, headers=who).status_code == 200
    assert create(9).status_code == 200


@pytest.mark.parametrize("field", ["title", "post", "post_id", "chat_ids"])
def test_public_draft_byte_limit_covers_all_stored_fields(public, field):
    who = login_as(public)
    huge = "a" * (1024**2 + 1)
    value = {"rich_message": {"html": huge}} if field == "post" else (["@" + huge] if field == "chat_ids" else huge)
    assert public.post("/api/drafts", headers=who, json={"post": BLOCKS_POST, field: value}).status_code == 413


def test_mcp_cannot_bypass_public_database_quota(public, monkeypatch):
    who = login_as(public)
    monkeypatch.setattr(settings.get(), "account_database_mb", 1)
    payload = {"rich_message": {"html": "x" * 200_000}}
    for _ in range(8):
        public.post("/api/drafts", headers=who, json={"post": payload})
    result = rpc(
        public, "tools/call", {"name": "save_draft", "arguments": {"title": "Over quota", "post": payload}}, headers=who
    )["result"]
    assert result["isError"] and "Лимит базы" in str(result)
    drafts = public.get("/api/drafts", headers=who).json()
    for draft in drafts:
        public.delete("/api/drafts/" + draft["id"], headers=who)
    assert public.post("/api/drafts", headers=who, json={"post": payload}).status_code == 200


def test_database_quota_also_limits_post_history_and_preserves_operator(public, monkeypatch):
    from errors import StorageFull

    login_as(public)
    monkeypatch.setattr(settings.get(), "account_database_mb", 1)
    large = {"html": "x" * 200_000}
    with accounts.scope(101):
        post = store.add_post("-1001", BLOCKS_POST)
        with pytest.raises(StorageFull):
            for _ in range(8):
                store.add_version(post["id"], large, "ui")
    # The operator's database has no public-account quota.
    for _ in range(8):
        store.save_draft(None, "Operator", large)


def test_health_error_order_does_not_depend_on_clock_resolution(client, monkeypatch):
    import metrics
    from services import health

    metrics.reset()
    monkeypatch.setattr(metrics.time, "time", lambda: 100.0)
    metrics.telegram_call("getMe", True)
    metrics.telegram_call("getUpdates", False, "ConnectError", quiet=True)
    assert health.telegram()["status"] == "warn"
    metrics.telegram_call("getUpdates", True, quiet=True)
    assert health.telegram()["status"] == "ok"


def test_lowered_database_quota_keeps_existing_data_readable_and_deletable(public, monkeypatch):
    from errors import StorageFull

    login_as(public)
    with accounts.scope(101):
        saved = [store.save_draft(None, str(i), {"html": "x" * 200_000}) for i in range(8)]
        monkeypatch.setattr(settings.get(), "account_database_mb", 1)
        assert store.get_draft(saved[0]["id"]) == saved[0]
        with pytest.raises(StorageFull):
            store.save_draft(None, "No further growth", {"html": "y" * 200_000})
        store.delete_draft(saved[0]["id"])
        assert store.save_draft(None, "Reuses freed pages", {"html": "z" * 100_000})


@pytest.mark.parametrize("event", ["poll", "invite"])
def test_revoked_account_does_not_receive_poll_or_invite_updates(public, tg, event):
    login_as(public)
    with accounts.scope(101):
        store.save_channel("-1001", chat_num=-1001, type="channel")
        post = store.add_post("-1001", BLOCKS_POST, status="sent", message_id=77)
        store.update_post(post["id"], poll_id="private-poll", chat_num=-1001)
        store.invite_links.save("test-invite", "-1001")
    tg.queue["getChatMember"] = [{"ok": True, "result": {"status": "left"}}]
    update = (
        {"poll": {"id": "private-poll", "total_voter_count": 3, "options": []}}
        if event == "poll"
        else {"chat_join_request": {"chat": {"id": -1001}, "invite_link": {"invite_link": "test-invite"}}}
    )
    asyncio.run(updates.handle_update(update))
    with accounts.scope(101):
        assert not store.get_post(post["id"])["poll_results"]
        assert store.invite_links.get("test-invite")["requests"] == 0
    assert any(c["method"] == "getChatMember" for c in tg.calls)


def test_own_dm_preview_callback_needs_no_channel_membership(public, tg):
    login_as(public)
    with accounts.scope(101):
        updates.remember_preview(101, 77, {"button": "Private preview answer"})
    tg.calls.clear()
    asyncio.run(
        updates.handle_update(
            {"callback_query": {"id": "query", "data": "button", "message": {"chat": {"id": 101}, "message_id": 77}}}
        )
    )
    answers = [c for c in tg.calls if c["method"] == "answerCallbackQuery"]
    assert len(answers) == 1 and answers[0]["body"]["text"] == "Private preview answer"
    assert not any(c["method"] == "getChatMember" for c in tg.calls)


@pytest.mark.parametrize("refused", [False, True])
def test_comment_exception_restores_normal_transport_policy(public, tg, refused):
    import account_context
    from errors import StudioError

    who = login_as(public)
    connect(public, who)
    tg.queue["getChat"] = [{"ok": True, "result": {"id": -1001, "type": "channel", "linked_chat_id": -1002}}]
    if refused:
        tg.queue["sendRichMessage"] = [{"ok": False, "error_code": 403, "description": "Forbidden"}]

    async def check():
        with accounts.scope(101):
            if refused:
                import telegram as tg_client

                with pytest.raises(tg_client.TelegramError):
                    await accounts.send_comment(-1001, -1002, 77, {"html": "Comment"})
            else:
                await accounts.send_comment(-1001, -1002, 77, {"html": "Comment"})
            with pytest.raises(StudioError):
                await account_context.authorize(
                    "sendRichMessage", {"chat_id": -1002, "rich_message": {"html": "Other"}}
                )
        assert account_context.user_id() is None

    asyncio.run(check())


def test_real_database_full_recording_send_result_leaves_key_pending(public, tg, monkeypatch):
    import sqlite3

    import telegram as tg_client
    from errors import Conflict, MayHaveGone, StorageFull
    from services import idempotency

    who = login_as(public)
    connect(public, who)
    monkeypatch.setattr(settings.get(), "account_database_mb", 1)

    async def action():
        await tg_client.call("sendRichMessage", {"chat_id": -1001, "rich_message": {"html": "Once"}})
        return {"response": "x" * (2 * 1024**2)}

    async def check():
        with accounts.scope(101):
            with pytest.raises(StorageFull) as error:
                await idempotency.run("full-result", "send", {}, action)
            assert error.value.__cause__.sqlite_errorcode == sqlite3.SQLITE_FULL
            with store.conn() as c:
                assert c.execute("select status from idempotency where key='full-result'").fetchone()[0] == "pending"
            with pytest.raises(Conflict):
                await idempotency.run("full-result", "send", {}, action)
            now = time.time()
            monkeypatch.setattr(idempotency.time, "time", lambda: now + idempotency.STALE_PENDING + 1)
            with pytest.raises(MayHaveGone):
                await idempotency.run("full-result", "send", {}, action)

    asyncio.run(check())
    assert sum(c["method"] == "sendRichMessage" for c in tg.calls) == 1
