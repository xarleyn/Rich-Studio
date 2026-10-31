"""Durable personal publishing reports. Delivery failures never affect a channel publication."""

import asyncio
import html
import logging
import time

import logs
import metrics
import settings
import store
import telegram as tg
from store import bot_dialog as db

from . import accounts, bot_dialog, listing

log = logging.getLogger("rich-studio.bot-notifications")
INTERVAL = 10
_sleep = asyncio.sleep


def record(post: str | dict, event: str | None = None, detail: str | None = None) -> None:
    """Save one outcome in the account outbox; even a full disk must not reclassify a successful send."""
    try:
        uid = bot_dialog.recipient()
        prefs = db.contact(uid) if uid is not None else None
        if uid is None or not prefs or prefs["mode"] == "off":
            return
        p = store.get_post(post) if isinstance(post, str) else post
        if not p:
            return
        event = event or p["status"]
        detail = detail or p.get("error") or ""
        category = "success" if event == "sent" and not detail else "error"
        if prefs["mode"] == "errors" and category == "success":
            return
        chat = store.get_channel(p["chat_id"]) or {}
        body = logs.scrub_all(
            {
                "event": event,
                "chat": str(chat.get("title") or p["chat_id"])[:160],
                "title": listing.title(p["payload"], p.get("kind")),
                "detail": detail[:700],
                "link": listing.link(p["chat_id"], p.get("message_id")),
                "post_id": p["id"],
                "subscription": prefs["revision"],
                "scheduled_at": p.get("scheduled_at"),
            }
        )
        key = f"{p['id']}:{event}"
        if event in {"missed", "retry"}:
            key += f":{p.get('scheduled_at')}"
        db.enqueue(key, uid, category, body)
    except Exception:
        log.exception("could not queue publishing notification", extra={"event": "bot.enqueue_failed"})


def expire_comments(before: int, error: str) -> None:
    waiting = db.waiting_comments(before)
    store.expire_comments(before, error)
    for pid in waiting:
        record(pid, "comment")


def _text(body: dict, language: str) -> str:
    titles = {
        "sent": ("✅ Пост опубликован", "✅ Post published"),
        "failed": ("❌ Не удалось опубликовать пост", "❌ Could not publish the post"),
        "uncertain": ("⚠️ Пост мог уйти в канал", "⚠️ The post may have been sent"),
        "missed": ("⏰ Публикация пропущена", "⏰ Scheduled post missed"),
        "retry": ("⏳ Публикация задерживается", "⏳ Publication delayed"),
        "comment": ("⚠️ Не удалось отправить первый комментарий", "⚠️ First comment failed"),
    }
    title = bot_dialog.phrase(language, *titles.get(body["event"], titles["failed"]))
    if body["event"] == "sent" and body["detail"]:
        title = bot_dialog.phrase(language, "⚠️ Пост опубликован с предупреждением", "⚠️ Post published with a warning")
    lines = [f"<b>{title}</b>", html.escape(body["chat"])]
    if body["title"]:
        lines += ["", html.escape(body["title"])]
    if body["detail"]:
        lines += ["", html.escape(body["detail"])]
    if body["event"] == "uncertain":
        lines += [
            "",
            bot_dialog.phrase(
                language,
                "Проверьте канал перед повтором. Автоматически повторять эту отправку не буду.",
                "Check the channel before retrying. This publication will not be retried automatically.",
            ),
        ]
    elif body["event"] in {"failed", "missed", "comment"}:
        lines += [
            "",
            bot_dialog.phrase(
                language,
                "Откройте «Посты» в студии, чтобы проверить подробности.",
                "Open Posts in the studio to check the details.",
            ),
        ]
    lines += ["", bot_dialog.phrase(language, "Настройки: /notifications", "Settings: /notifications")]
    return "\n".join(lines)


async def deliver() -> None:
    """Small per-account batches. A recipient is derived again before delivery, never trusted from storage alone."""
    for uid in accounts.all_ids():
        try:
            with accounts.scope(uid):
                for event in db.due():
                    await _deliver(event)
        except Exception:
            log.exception("account %s notification delivery failed", uid, extra={"event": "bot.delivery_failed"})


async def _deliver(event: dict) -> None:
    key, recipient = event["event_key"], event["recipient"]
    prefs = db.contact(recipient)
    if (
        recipient != bot_dialog.recipient()
        or not prefs
        or prefs["mode"] == "off"
        or event["body"].get("subscription") != prefs["revision"]
        or (prefs["mode"] == "errors" and event["category"] == "success")
    ):
        db.finish(key, "skipped")
        return
    outcome = event["body"]["event"]
    if outcome in {"retry", "missed"}:
        post = store.get_post(event["body"]["post_id"]) or {}
        expected = "scheduled" if outcome == "retry" else "missed"
        if (
            post.get("status") != expected
            or post.get("scheduled_at") != event["body"].get("scheduled_at")
            or (outcome == "retry" and not post.get("retry_at"))
        ):
            db.finish(key, "skipped")
            return
    if not db.claim(key):
        return
    try:
        await bot_dialog.send_private(
            recipient,
            _text(event["body"], prefs["language"]),
            bot_dialog.keyboard(prefs["language"], link=event["body"]["link"]),
        )
    except tg.TelegramError as e:
        if e.code == 403:
            db.disable(recipient)
        retry = isinstance(e, tg.Unreachable) or (e.code == 429 and e.retry_after)
        if retry and event["attempts"] < 4:
            db.finish(key, "pending", retry_at=int(time.time()) + max(60, e.retry_after or 0))
        else:
            db.finish(key, "failed")
        log.warning("notification refused: %s", e, extra={"event": "bot.delivery_failed"})
    except Exception:
        # Timeout, 5xx or a lost response: the DM may already exist. Leave the persistent attempted mark.
        log.exception("notification may have been delivered; not retrying", extra={"event": "bot.delivery_uncertain"})
    else:
        db.finish(key, "sent")


async def loop() -> None:
    registered = False
    while True:
        error = None
        try:
            if settings.get().bot_token:
                if not registered:
                    try:
                        await bot_dialog.register_commands()
                        registered = True
                    except Exception:
                        log.exception("could not register bot commands", extra={"event": "bot.menu_failed"})
                await deliver()
        except Exception as e:
            log.exception("notification iteration failed")
            error = type(e).__name__
        metrics.beat("bot_notifications", INTERVAL, error)
        await _sleep(INTERVAL)
