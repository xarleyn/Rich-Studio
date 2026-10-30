"""Incoming Telegram updates (getUpdates): button clicks, first comments, reactions, poll results, joins,
suggested posts."""

import asyncio
import json
import logging
import time
from dataclasses import asdict, dataclass

import account_context
import logs
import metrics
import settings
import store
import telegram as tg
from errors import StudioError
from store.meta import claim_webhook_update

from . import accounts, channels, invite_links, suggested_posts

log = logging.getLogger("rich-studio.updates")
# tests replace this one name; patching asyncio.sleep itself would also stop the fake Telegram server
_sleep = asyncio.sleep
OFFSET_KEY = "updates_offset"
PREVIEW_KEY = "preview_callbacks"
PREVIEWS_KEPT = 20  # button answers of the newest previews; older preview buttons get an empty answer
ALLOWED_UPDATES = [
    "message",
    "edited_message",
    "callback_query",
    "message_reaction_count",
    "poll",
    "chat_member",
    "chat_join_request",
    "my_chat_member",
]
# Telegram keeps undelivered updates for about a day; a forward older than that will never arrive
COMMENT_WAIT = 24 * 3600


@dataclass
class Status:
    enabled: bool = True
    state: str = "starting"  # starting | ok | off | no_token | conflict | error
    error: str | None = None
    last_at: int | None = None
    handled: int = 0

    def as_dict(self) -> dict:
        return asdict(self)


STATUS = Status()


def public_status() -> dict:
    status = STATUS.as_dict()
    status["mode"] = settings.get().updates_mode
    if account_context.user_id() is not None:
        status.update(error=None, last_at=None, handled=0)
    return status


def reset_status() -> None:
    on = settings.get().updates_on
    STATUS.__init__(enabled=on, state="starting" if on else "off")  # type: ignore[misc]


def remember_preview(chat_id, message_id: int, callbacks: dict) -> None:
    """Keep the answers to the buttons of a preview in the DM: previews are not posts, but clicks come all the same."""
    saved = json.loads(store.get_meta(PREVIEW_KEY) or "{}")
    saved[f"{chat_id}:{message_id}"] = callbacks
    store.set_meta(PREVIEW_KEY, json.dumps(dict(list(saved.items())[-PREVIEWS_KEPT:]), ensure_ascii=False))


def _preview_callbacks(chat_id, message_id) -> dict:
    return json.loads(store.get_meta(PREVIEW_KEY) or "{}").get(f"{chat_id}:{message_id}") or {}


def _reaction_key(r: dict) -> str:
    t = r.get("type") or {}
    return t.get("emoji") or ("custom:" + t["custom_emoji_id"] if t.get("custom_emoji_id") else t.get("type", "?"))


def poll_results(poll: dict) -> dict:
    return {
        "total": poll.get("total_voter_count", 0),
        "is_closed": poll.get("is_closed", False),
        "options": [{"text": o.get("text"), "voter_count": o.get("voter_count", 0)} for o in poll.get("options", [])],
    }


async def handle_update(u: dict) -> str | None:
    """Dispatch one bot update to the databases that own its post, channel or invite link."""
    from services import import_forward

    if import_forward.receive(u):
        return "import"
    if not settings.get().public:
        return await _handle_update(u)
    ids = accounts.all_ids()
    if cm := u.get("my_chat_member"):
        uid = (cm.get("from") or {}).get("id")
        chat_id = (cm.get("chat") or {}).get("id")
        if uid in ids and uid is not None and chat_id:
            with accounts.scope(uid):
                member = cm.get("new_chat_member") or {}
                if member.get("status") in ("administrator", "creator"):
                    await channels.check(chat_id)
                    return "channel_connected"
        return None
    handled = None
    for uid in ids:
        with accounts.scope(uid):
            if _owns_update(u):
                try:
                    # Comments check current rights at the final transport boundary and record a refusal.
                    message = u.get("message") or {}
                    if uid is not None and not message.get("is_automatic_forward"):
                        target = _update_chat(u)
                        if str(target) != str(uid):
                            target = accounts.linked_target(target)
                            channel = store.get_channel(target) or {}
                            await accounts.check_member(target, channel.get("type"))
                    handled = await _handle_update(u) or handled
                except Exception:  # one revoked/unreachable account must not stop other recipients
                    log.exception("account %s update refused or failed", uid)
                    continue
                if u.get("callback_query"):
                    return handled  # one answer, even if several admins linked the same channel
    if handled:
        return handled
    # Unknown callbacks still need an empty answer; legacy channels keep their existing event behavior.
    with accounts.scope(None):
        return await _handle_update(u)


def _owns_update(u: dict) -> bool:
    return _update_chat(u) is not None


def _update_chat(u: dict):
    """Resolve the authorizing chat from this account's stored objects, including chat-less poll updates."""
    if cq := u.get("callback_query"):
        msg = cq.get("message") or {}
        chat, mid = (msg.get("chat") or {}).get("id"), msg.get("message_id")
        return chat if store.find_post(chat, mid) or _preview_callbacks(chat, mid) else None
    if poll := u.get("poll"):
        post = store.find_poll(poll.get("id")) or {}
        return post.get("chat_num") or post.get("chat_id")
    if rc := u.get("message_reaction_count"):
        chat = (rc.get("chat") or {}).get("id")
        return chat if store.find_post(chat, rc.get("message_id")) else None
    event = u.get("message") or u.get("edited_message") or u.get("chat_member") or u.get("chat_join_request") or {}
    if (origin := event.get("forward_origin")) and event.get("is_automatic_forward"):
        chat = (origin.get("chat") or {}).get("id")
        return chat if store.find_post(chat, origin.get("message_id")) else None
    if link := (event.get("invite_link") or {}).get("invite_link"):
        return (store.invite_links.get(link) or {}).get("chat_id")
    chat = (event.get("chat") or {}).get("id")
    return chat if chat is not None and store.get_channel(chat) else None


async def _handle_update(u: dict) -> str | None:
    """React to one Telegram update. Unknown updates are ignored. Returns what was handled."""
    if cq := u.get("callback_query"):
        msg = cq.get("message") or {}
        p = store.find_post((msg.get("chat") or {}).get("id"), msg.get("message_id")) if msg else None
        params = {"callback_query_id": cq["id"]}
        data = cq.get("data") or ""
        if p:
            store.bump_click(p["id"], data)
            callbacks = (p["payload"] or {}).get("callbacks") or {}
        else:
            callbacks = _preview_callbacks((msg.get("chat") or {}).get("id"), msg.get("message_id")) if msg else {}
        ans = callbacks.get(data)
        if isinstance(ans, str):
            ans = {"text": ans}
        if isinstance(ans, dict):
            params.update({k: v for k, v in ans.items() if k in ("text", "show_alert", "url", "cache_time")})
        try:
            await tg.call("answerCallbackQuery", params)  # always, or the button keeps spinning
        except tg.TelegramError as e:
            # the click came while the studio was off: Telegram no longer takes an answer, the click still counts
            if "query is too old" not in e.description:
                raise
            log.warning("button click came too late to answer: %s", e.description)
        return "callback"
    if (m := u.get("message") or u.get("edited_message")) and suggested_posts.on_message(m):
        return "suggested_post"
    if (m := u.get("message")) and suggested_posts.on_service(m):
        return "suggested_post_outcome"
    if (m := u.get("message")) and (topic := m.get("forum_topic_created") or m.get("forum_topic_edited")):
        # the Bot API cannot list a forum's topics: the ones created or renamed while the studio runs are learned here
        if topic.get("name"):
            channels.remember_topic((m.get("chat") or {}).get("id"), m.get("message_thread_id"), topic["name"])
            return "topic"
        return None
    if (m := u.get("message")) and m.get("is_automatic_forward"):
        origin = m.get("forward_origin") or {}
        if origin.get("type") != "channel":
            return None
        p = store.find_post(origin["chat"]["id"], origin.get("message_id"))
        if not p or p.get("comment_status") != "waiting":
            return None
        comment = p["payload"].get("comment")
        try:
            res = await accounts.send_comment(origin["chat"]["id"], m["chat"]["id"], m["message_id"], comment)
            store.update_post(p["id"], comment_status="sent", comment_message_id=res["message_id"])
        except (StudioError, tg.TelegramError) as e:
            store.update_post(p["id"], comment_status="failed", error=f"первый комментарий: {e}")
        except Exception as e:  # noqa: BLE001 - mid-send: the update is already confirmed, so say it once here
            store.update_post(
                p["id"],
                comment_status="failed",
                error=f"первый комментарий: {type(e).__name__}: {e}. Он мог уйти, проверьте обсуждение",
            )
        return "comment"
    if rc := u.get("message_reaction_count"):
        p = store.find_post(rc["chat"]["id"], rc["message_id"])
        if p:
            store.update_post(
                p["id"], reactions={_reaction_key(r): r.get("total_count", 0) for r in rc.get("reactions", [])}
            )
        return "reactions"
    if poll := u.get("poll"):
        p = store.find_poll(poll["id"])
        if p:
            store.update_post(p["id"], poll_results=poll_results(poll))
        return "poll"
    if cm := u.get("chat_member"):
        return "join" if invite_links.on_member(cm) else None
    if jr := u.get("chat_join_request"):
        return "join_request" if invite_links.on_join_request(jr) else None
    return None


async def loop() -> None:
    # Telegram re-sends updates that were not confirmed by the next getUpdates call;
    # remembering the offset keeps a restart from counting the same clicks twice
    saved = store.get_meta(OFFSET_KEY)
    offset = int(saved) if saved else None
    while True:
        # one round is a long poll (up to 40 s) or a wait after an error (up to 60 s)
        metrics.beat("updates", 60, STATUS.error if STATUS.state in ("error", "conflict") else None)
        if not settings.get().bot_token:
            STATUS.state = "no_token"
            await _sleep(30)
            continue
        try:
            ups = await tg.call(
                "getUpdates", {"offset": offset, "timeout": 25, "allowed_updates": ALLOWED_UPDATES}, timeout=40
            )
            STATUS.state, STATUS.error = "ok", None
            for uid in accounts.all_ids():
                with accounts.scope(uid):
                    store.expire_comments(
                        int(time.time()) - COMMENT_WAIT, "первый комментарий: пересылка в обсуждение не пришла за сутки"
                    )
            for u in ups:
                offset = u["update_id"] + 1
                store.set_meta(OFFSET_KEY, str(offset))
                with logs.bind(f"update-{u['update_id']}"):  # Telegram's own id: easy to find again
                    try:
                        if await handle_update(u):
                            STATUS.handled += 1
                            STATUS.last_at = int(time.time())
                    except Exception:
                        log.exception("update %s failed", u.get("update_id"))
        except tg.TelegramError as e:
            # 409: a webhook is set or another process polls the same bot
            STATUS.state = "conflict" if e.code == 409 else "error"
            STATUS.error = e.description
            await _sleep(60 if e.code == 409 else 10)
        except Exception as e:  # noqa: BLE001 - keep polling whatever happens
            STATUS.state, STATUS.error = "error", f"{type(e).__name__}: {e}"
            await _sleep(10)


async def receive_webhook(update: dict) -> None:
    """Claim before processing, as polling does before confirming an update to Telegram."""
    uid = update["update_id"]
    if not claim_webhook_update(uid):
        return
    with logs.bind(f"update-{uid}"):
        try:
            if await handle_update(update):
                STATUS.handled += 1
                STATUS.last_at = int(time.time())
        except Exception:
            # A handler may already have sent a comment: never repeat an uncertain side effect.
            log.exception("update %s failed", uid)


async def webhook_loop() -> None:
    """Register the configured webhook under the background lease and keep housekeeping alive."""
    registered = False
    while True:
        metrics.beat("updates", 60, STATUS.error if STATUS.state == "error" else None)
        if not settings.get().bot_token:
            STATUS.state = "no_token"
            await _sleep(30)
            continue
        try:
            if not registered:
                cfg = settings.get()
                await tg.call(
                    "setWebhook",
                    {
                        "url": cfg.public_url + "/api/telegram/webhook",
                        "secret_token": cfg.webhook_secret,
                        "allowed_updates": ALLOWED_UPDATES,
                        "max_connections": 1,
                    },
                )
                registered = True
            STATUS.state, STATUS.error = "ok", None
            for uid in accounts.all_ids():
                with accounts.scope(uid):
                    store.expire_comments(
                        int(time.time()) - COMMENT_WAIT, "первый комментарий: пересылка в обсуждение не пришла за сутки"
                    )
            await _sleep(60)
        except Exception as e:  # noqa: BLE001 - retry registration or housekeeping without killing the loop
            STATUS.state, STATUS.error = "error", f"{type(e).__name__}: {e}"
            await _sleep(10)
