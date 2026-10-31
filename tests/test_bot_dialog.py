"""The real update dispatcher and transport policy, with Telegram served by the local fake."""

import asyncio
import time

import pytest

import account_context
import settings
import store
import telegram
from errors import MayHaveGone, StudioError
from services import accounts, bot_dialog, bot_notifications, loops, publisher, scheduler, updates
from store import bot_dialog as db
from test_api import BLOCKS_POST


def message(text="/start", uid=777, **fields):
    return {
        "message": {
            "message_id": 1,
            "from": {"id": uid, "language_code": "ru"},
            "chat": {"id": uid, "type": "private"},
            "text": text,
            **fields,
        }
    }


def run(awaitable):
    return asyncio.run(awaitable)


def notices(tg):
    return [call["body"] for call in tg.calls if call["method"] == "sendMessage"]


def subscribe(uid=777, mode="all"):
    db.touch(uid, "ru", mode)


def test_start_help_menu_and_preferences(client, tg, monkeypatch):
    monkeypatch.setattr(settings.get(), "public_url", "https://studio.example")
    run(updates.handle_update(message("/help")))
    assert db.contact(777) is None  # help does not opt in or prevent a later /start
    assert "/notifications" in notices(tg)[-1]["text"]
    assert run(updates.handle_update(message())) == "bot_command"
    assert db.contact(777)["mode"] == "all"
    assert notices(tg)[-1]["reply_markup"]["inline_keyboard"][0][0]["url"] == "https://studio.example"
    for command in ("/status", "/studio", "/notifications", "/unknown", "Привет"):
        assert run(updates.handle_update(message(command))) == "bot_command"
    assert "<b>" not in notices(tg)[-1]["text"] or "Rich" in notices(tg)[-1]["text"]
    run(updates.handle_update(message("/stop")))
    run(updates.handle_update(message()))
    assert db.contact(777)["mode"] == "off"  # /start does not undo an explicit preference
    run(updates.handle_update(message("/notifications errors")))
    assert db.contact(777)["mode"] == "errors"
    store.init()  # migration/restart preserves subscription
    assert db.contact(777)["mode"] == "errors"
    run(bot_dialog.register_commands())
    commands = [c["body"] for c in tg.calls if c["method"] == "setMyCommands"]
    assert {c["language_code"] for c in commands} == {"", "ru", "en"}
    assert all(c["scope"] == {"type": "all_private_chats"} for c in commands)


@pytest.mark.parametrize(
    "change",
    [
        {"chat": {"id": -1009, "type": "supergroup"}},
        {"chat": {"id": 888, "type": "private"}},
        {"from": {"id": 777, "is_bot": True}},
        {"text": "/start@SomeOtherBot"},
        {"forward_origin": {"type": "hidden_user"}},
    ],
)
def test_commands_only_from_own_private_chat(client, tg, change):
    run(updates.handle_update(message(**change)))
    assert not notices(tg) and db.contact(777) is None


def test_owner_only_in_private_mode_and_mentions(client, tg):
    run(updates.handle_update(message(uid=888)))
    assert db.contact(888) is None and not notices(tg)
    run(updates.handle_update(message("/start@YourBot")))
    assert db.contact(777)["mode"] == "all"


def test_callback_preferences_and_rate_limit(client, tg):
    update = {
        "callback_query": {
            "id": "callback-1",
            "from": {"id": 777},
            "message": message()["message"],
            "data": "rsbot:notifications errors",
        }
    }
    assert run(updates.handle_update(update)) == "bot_command"
    assert db.contact(777)["mode"] == "errors"
    assert tg.last("answerCallbackQuery")["body"]["callback_query_id"] == "callback-1"
    for _ in range(20):
        run(updates.handle_update(message("/help")))
    assert len(notices(tg)) == 12
    run(updates.handle_update(message("/stop")))
    assert db.contact(777)["mode"] == "off"


def test_public_commands_do_not_create_or_read_another_account(client, tg, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    store.add_post("@operator", BLOCKS_POST)
    assert run(updates.handle_update(message("/start", uid=101))) == "bot_command"
    run(updates.handle_update(message("/status 777", uid=101)))
    assert "Сначала" in notices(tg)[-1]["text"]
    assert not store.accounts.ids()
    assert not (settings.get().data_dir / "accounts" / "101").exists()
    for uid in (101, 202):
        store.accounts.save({"id": uid, "first_name": "Person"})
        accounts.initialize(uid)
        with accounts.scope(uid):
            store.add_post("@own", BLOCKS_POST, status="failed")
            if uid == 202:
                store.add_post("@secret", BLOCKS_POST, status="failed")
    run(updates.handle_update(message("/status 202", uid=101)))
    assert "Требуют проверки: 1" in notices(tg)[-1]["text"]
    assert "Опубликовано: 0" in notices(tg)[-1]["text"]
    assert account_context.user_id() is None


def test_narrow_dm_policy_restored_and_no_arbitrary_send(client, tg):
    async def check():
        with accounts.scope(101):
            with pytest.raises(StudioError):
                await bot_dialog.send_private(202, "secret", {})
            await bot_dialog.send_private(101, "hello", {})
            with pytest.raises(StudioError):
                await telegram.call("sendMessage", {"chat_id": 101, "text": "arbitrary"})

    run(check())
    assert len(notices(tg)) == 1 and notices(tg)[0]["chat_id"] == 101


def test_public_notifications_are_scoped_and_private(client, tg, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    for uid in (101, 202):
        store.accounts.save({"id": uid, "first_name": "Person"})
        accounts.initialize(uid)
        subscribe(uid)
        with accounts.scope(uid):
            p = store.add_post(f"@channel_{uid}", BLOCKS_POST, message_id=10)
            bot_notifications.record(p)
            if uid == 101:  # corrupted destination cannot leak this account's report
                db.enqueue("forged", 202, "success", {"title": "private"})
    run(bot_notifications.deliver())
    assert {m["chat_id"] for m in notices(tg)} == {101, 202}
    for m in notices(tg):
        assert f"@channel_{m['chat_id']}" in m["text"]
        assert "private" not in m["text"]


def test_publish_report_deduplicated_and_linked(client, tg, monkeypatch):
    monkeypatch.setattr(settings.get(), "public_url", "https://studio.example")
    subscribe()
    results, _ = run(publisher.publish(["@test_channel"], BLOCKS_POST))
    post = results[0]["post"]
    assert post["status"] == "sent" and not notices(tg)  # outbox, no extra network in publish
    bot_notifications.record(post)
    run(bot_notifications.deliver())
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1
    assert "Пост опубликован" in notices(tg)[0]["text"]
    assert notices(tg)[0]["reply_markup"]["inline_keyboard"][0][0]["url"].startswith("https://t.me/test_channel/")


def test_notifications_off_errors_only_and_no_historical_replay(client, tg):
    run(publisher.publish(["@old"], BLOCKS_POST))
    subscribe(mode="errors")
    run(publisher.publish(["@ok"], BLOCKS_POST))
    tg.fail["sendRichMessage"] = "Bad Request: no posting rights"
    with pytest.raises(telegram.TelegramError):
        run(publisher.publish(["@refused"], BLOCKS_POST))
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1 and "no posting rights" in notices(tg)[0]["text"]
    subscribe()
    tg.fail.clear()
    run(publisher.publish(["@queued"], BLOCKS_POST))
    db.disable(777)
    run(bot_notifications.deliver())
    subscribe()
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1


def test_may_have_gone_is_not_a_refusal_or_a_retry(client, tg):
    subscribe()
    tg.fail["sendRichMessage"] = "gateway down"
    tg.fail_code["sendRichMessage"] = 502
    with pytest.raises(MayHaveGone):
        run(publisher.publish(["@maybe"], BLOCKS_POST))
    run(bot_notifications.deliver())
    assert "Пост мог уйти" in notices(tg)[0]["text"]
    assert "Проверьте канал перед повтором" in notices(tg)[0]["text"]
    assert len([c for c in tg.calls if c["method"] == "sendRichMessage"]) == 1


@pytest.mark.parametrize("error,code", [("blocked", 403), ("gateway", 502)])
def test_dm_failure_never_changes_or_duplicates_publication(client, tg, error, code):
    subscribe()
    results, _ = run(publisher.publish(["@ok"], BLOCKS_POST))
    tg.fail["sendMessage"], tg.fail_code["sendMessage"] = error, code
    run(bot_notifications.deliver())
    run(bot_notifications.deliver())
    assert store.get_post(results[0]["post"]["id"])["status"] == "sent"
    assert len(notices(tg)) == 1
    assert db.contact(777)["mode"] == ("off" if code == 403 else "all")


def test_only_definite_dm_failure_is_retried(client, tg, monkeypatch):
    subscribe()
    run(publisher.publish(["@ok"], BLOCKS_POST))
    original = telegram.call

    async def offline(method, params=None, timeout=60):
        if method == "sendMessage":
            raise telegram.Unreachable("no connection")
        return await original(method, params, timeout)

    monkeypatch.setattr(telegram, "call", offline)
    run(bot_notifications.deliver())
    with store.conn() as c:
        event = dict(c.execute("select * from bot_outbox").fetchone())
    assert event["state"] == "pending" and event["retry_at"] > time.time()
    monkeypatch.setattr(telegram, "call", original)
    db.finish(event["event_key"], "pending")
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1


def test_post_send_warning_and_secret_redaction(client, tg):
    subscribe()
    tg.fail["pinChatMessage"] = "<bad> " + settings.get().bot_token
    results, warnings = run(publisher.publish(["@ok"], {**BLOCKS_POST, "pin": True}))
    assert warnings and results[0]["post"]["status"] == "sent"
    run(bot_notifications.deliver())
    text = notices(tg)[0]["text"]
    assert "опубликован с предупреждением" in text and "&lt;bad&gt;" in text
    assert settings.get().bot_token not in text
    with store.conn() as c:
        assert settings.get().bot_token not in c.execute("select body from bot_outbox").fetchone()[0]


def test_queue_failure_cannot_change_sent_result(client, tg, monkeypatch):
    subscribe()

    def full(*args):
        raise OSError("disk full")

    monkeypatch.setattr(db, "enqueue", full)
    result, _ = run(publisher.publish(["@ok"], BLOCKS_POST))
    assert result[0]["post"]["status"] == "sent"


def test_scheduler_reports_sent_missed_retry_and_terminal_error(client, tg, monkeypatch):
    subscribe()
    monkeypatch.setattr(settings.get(), "schedule_late_min", 1)
    sent = store.add_post("@sent", BLOCKS_POST, status="scheduled", scheduled_at=int(time.time()) - 1)
    missed = store.add_post("@missed", BLOCKS_POST, status="scheduled", scheduled_at=int(time.time()) - 1000)
    run(scheduler.run_due_posts())
    assert store.get_post(sent["id"])["status"] == "sent"
    assert store.get_post(missed["id"])["status"] == "missed"
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 2
    retry = store.add_post("@retry", BLOCKS_POST, status="scheduled", scheduled_at=int(time.time()) - 1)
    tg.queue["sendRichMessage"] = [
        {"ok": False, "error_code": 429, "description": "wait", "parameters": {"retry_after": 120}}
    ]
    run(scheduler.run_due_posts())
    run(bot_notifications.deliver())
    assert "задерживается" in notices(tg)[-1]["text"]
    store.update_post(retry["id"], retry_at=0)
    tg.fail["sendRichMessage"] = "forbidden"
    run(scheduler.run_due_posts())
    run(bot_notifications.deliver())
    assert "Не удалось" in notices(tg)[-1]["text"]


def test_crash_recovery_and_expired_comment_report(client, tg):
    subscribe()
    p = store.add_post("@uncertain", BLOCKS_POST, status="sending")
    runner = loops.Loops([])
    previous = loops.STATE.role
    try:
        runner._lead()
    finally:
        loops.STATE.role = previous
    assert store.get_post(p["id"])["status"] == "failed"
    p = store.add_post("@comment", BLOCKS_POST, message_id=42)
    store.update_post(p["id"], comment_status="waiting", sent_at=1)
    bot_notifications.expire_comments(100, "Пересылка не пришла")
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 2
    assert any("мог уйти" in m["text"] for m in notices(tg))
    assert any("комментарий" in m["text"] for m in notices(tg))


def test_import_start_is_not_replaced_by_welcome(client, tg):
    from services import import_forward
    from store import import_inbox

    ticket = import_inbox.create("https://t.me/test_channel/10", 777)
    assert run(updates.handle_update(message(f"/start import_{ticket['id']}"))) == "import"
    assert "перешлите" in notices(tg)[-1]["text"]
    assert import_forward.get(ticket["id"])["state"] == "armed"
    assert db.contact(777) is None
    assert run(updates.handle_update(message("/start import_missing"))) == "bot_command"
    assert "устарел" in notices(tg)[-1]["text"]


def test_webhook_deduplicates_commands(client, tg):
    update = {"update_id": 42, **message()}
    run(updates.receive_webhook(update))
    run(updates.receive_webhook(update))
    assert len(notices(tg)) == 1


def test_english_commands_and_reports(client, tg):
    run(updates.handle_update(message(**{"from": {"id": 777, "language_code": "en-US"}})))
    assert "Hi!" in notices(tg)[-1]["text"]
    run(publisher.publish(["@ok"], BLOCKS_POST))
    run(bot_notifications.deliver())
    assert "Post published" in notices(tg)[-1]["text"]


def test_reply_transport_failure_restores_account_policy(client, tg, monkeypatch):
    async def fail(*args, **kwargs):
        raise telegram.NoAnswer("lost response")

    monkeypatch.setattr(telegram, "call", fail)

    async def check():
        with accounts.scope(101):
            with pytest.raises(telegram.NoAnswer):
                await bot_dialog.send_private(101, "hello", {})
            with pytest.raises(StudioError):
                await account_context.authorize("sendMessage", {"chat_id": 101, "text": "leak"})

    run(check())


def test_preview_never_queues_publication_notice(client, tg):
    from services import posts

    subscribe()
    run(posts.preview(BLOCKS_POST))
    assert db.due() == []


def test_partial_publish_reports_each_channel_once(client, tg):
    subscribe()
    tg.queue["sendRichMessage"] = [{"ok": False, "error_code": 400, "description": "refused"}]
    results, _ = run(publisher.publish(["@bad", "@good"], BLOCKS_POST))
    assert [r["ok"] for r in results] == [False, True]
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 2
    assert any("@bad" in m["text"] and "Не удалось" in m["text"] for m in notices(tg))
    assert any("@good" in m["text"] and "опубликован" in m["text"] for m in notices(tg))


def test_outbox_claim_survives_restart_and_is_atomic(client, tg):
    from concurrent.futures import ThreadPoolExecutor

    subscribe()
    run(publisher.publish(["@ok"], BLOCKS_POST))
    event = db.due()[0]
    with ThreadPoolExecutor(max_workers=2) as pool:
        assert sorted(pool.map(db.claim, [event["event_key"], event["event_key"]])) == [False, True]
    store.init()
    run(bot_notifications.deliver())
    assert not notices(tg)  # an interrupted attempted send is not replayed


def test_real_time_comment_failure_report(client, tg, monkeypatch):
    subscribe()
    monkeypatch.setattr(settings.get(), "updates_on", True)
    results, _ = run(publisher.publish(["@channel"], {**BLOCKS_POST, "comment": BLOCKS_POST["rich_message"]}))
    p = results[0]["post"]
    tg.fail["sendRichMessage"] = "discussion unavailable"
    update = {
        "message": {
            "message_id": 999,
            "chat": {"id": -500, "type": "supergroup"},
            "is_automatic_forward": True,
            "forward_origin": {
                "type": "channel",
                "chat": {"id": p["chat_num"]},
                "message_id": p["message_id"],
            },
        }
    }
    assert run(updates.handle_update(update)) == "comment"
    run(bot_notifications.deliver())
    assert any("первый комментарий" in m["text"] for m in notices(tg))


def test_failure_of_after_send_keeps_successful_post_and_warns(client, tg, monkeypatch):
    subscribe()

    async def fail(*args):
        raise OSError("disk full")

    monkeypatch.setattr(publisher, "after_send", fail)
    results, warnings = run(publisher.publish(["@one", "@two"], BLOCKS_POST))
    assert len(results) == 2 and all(r["ok"] for r in results) and warnings
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 2 and all("с предупреждением" in m["text"] for m in notices(tg))


def test_stop_then_on_before_worker_does_not_replay_old_reports(client, tg):
    subscribe()
    run(publisher.publish(["@old"], BLOCKS_POST))
    run(updates.handle_update(message("/stop")))
    run(updates.handle_update(message("/notifications all")))
    tg.calls.clear()
    run(publisher.publish(["@new"], BLOCKS_POST))
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1 and "@new" in notices(tg)[0]["text"]


def test_delayed_retry_notice_is_discarded_after_publication(client, tg):
    subscribe()
    p = store.add_post("@channel", BLOCKS_POST, status="scheduled", scheduled_at=int(time.time()) - 1)
    tg.queue["sendRichMessage"] = [
        {"ok": False, "error_code": 429, "description": "wait", "parameters": {"retry_after": 120}}
    ]
    run(scheduler.run_due_posts())
    retry = db.due()[0]
    assert retry["body"]["event"] == "retry"
    store.update_post(p["id"], retry_at=0)
    run(scheduler.run_due_posts())
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1 and "опубликован" in notices(tg)[0]["text"]


def test_new_schedule_can_report_a_new_missed_deadline(client, tg):
    subscribe()
    p = store.add_post("@channel", BLOCKS_POST, status="scheduled", scheduled_at=100)
    for at in (100, 200):
        if at != 100:
            assert store.reschedule(p["id"], at)
        assert store.mark_missed(p["id"], "missed")
        bot_notifications.record(p["id"])
        bot_notifications.record(p["id"])
        run(bot_notifications.deliver())
    assert len(notices(tg)) == 2


def test_rescheduled_post_discards_old_missed_report(client, tg):
    subscribe()
    p = store.add_post("@channel", BLOCKS_POST, status="missed", scheduled_at=100)
    bot_notifications.record(p)
    assert store.reschedule(p["id"], 200)
    assert store.mark_missed(p["id"], "new deadline")
    bot_notifications.record(p["id"])
    run(bot_notifications.deliver())
    assert len(notices(tg)) == 1 and "new deadline" in notices(tg)[0]["text"]


@pytest.mark.parametrize("public", [False, True])
@pytest.mark.parametrize("data", ["rsbot:help", "rsbot:notifications off"])
def test_existing_preview_callbacks_take_priority_over_bot_commands(client, tg, monkeypatch, public, data):
    monkeypatch.setattr(settings.get(), "public", public)
    subscribe()
    if public:
        store.accounts.save({"id": 777, "first_name": "Person"})
        accounts.initialize(777)
    with accounts.scope(777 if public else None):
        updates.remember_preview(777, 42, {data: {"text": "custom preview answer"}})
    update = {
        "callback_query": {
            "id": "preview",
            "from": {"id": 777},
            "data": data,
            "message": {"message_id": 42, "chat": {"id": 777, "type": "private"}},
        }
    }
    assert run(updates.handle_update(update)) == "callback"
    assert tg.last("answerCallbackQuery")["body"]["text"] == "custom preview answer"
    assert not notices(tg) and db.contact(777)["mode"] == "all"
