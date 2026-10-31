"""Offline import contract: hostile URLs, complete messages, ownership, rollback and no publication."""

import asyncio
import json
from contextlib import asynccontextmanager
from pathlib import Path

import pytest
from telethon.tl import types

import settings
import spec
import store
from errors import Invalid, StudioError
from services import accounts, import_forward, imports, updates
from services.import_links import parse_link
from services.import_web import Widget
from spec.import_content import Converter, entities_text, plain, representations
from store import import_inbox
from telegram import import_client

CASES = json.loads(Path(__file__).with_name("import_links.json").read_text("utf-8"))


@pytest.mark.parametrize("case", CASES, ids=lambda c: c["url"])
def test_links(case):
    if not case["valid"]:
        with pytest.raises(Invalid):
            parse_link(case["url"])
        return
    link = parse_link(case["url"])
    assert link.message_id == case["message_id"]
    assert link.topic_id == case["topic_id"]
    assert parse_link(link.url) == link


def test_reject_url_before_network(client, monkeypatch, tg):
    async def unexpected(*args):
        pytest.fail("Invalid link reached Telegram")

    monkeypatch.setattr(imports, "_mtproto", unexpected)
    monkeypatch.setattr(imports.import_web, "fetch_widget", unexpected)
    r = client.post("/api/imports/telegram", json={"url": "https://t.me.evil.org/a/1"})
    assert r.status_code == 422
    assert not tg.calls


def test_import_new_draft_keeps_existing_and_all_formats(client, monkeypatch, tg):
    old = store.save_draft(None, "Existing", {"rich_message": {"html": "Keep"}})
    monkeypatch.setattr(settings.get(), "import_api_id", 1)
    monkeypatch.setattr(settings.get(), "import_api_hash", "test")

    async def receive(link, saved):
        assert link.topic_id == 2 and link.message_id == 3450
        return [{"type": "heading", "size": 2, "text": "Title"}, {"type": "paragraph", "text": "Body"}], []

    monkeypatch.setattr(imports, "_mtproto", receive)
    r = client.post("/api/imports/telegram", json={"url": "https://t.me/mcoadvanced/2/3450"})
    assert r.status_code == 200, r.text
    d = r.json()["draft"]
    assert d["id"] != old["id"]
    assert store.get_draft(old["id"])["payload"]["rich_message"]["html"] == "Keep"
    assert d["payload"]["options"] == {}
    buffers = d["payload"]["_studio"]["buffers"]
    assert "<h2>Title</h2>" in buffers["html"]
    assert "## Title" in buffers["markdown"]
    assert not store.list_posts() and not tg.calls


def test_download_failure_rolls_back_files_and_draft(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "import_api_id", 1)
    monkeypatch.setattr(settings.get(), "import_api_hash", "test")

    async def receive(link, saved):
        path = settings.get().media_dir / "partial.jpg"
        path.write_bytes(b"downloaded")
        saved.append(path)
        raise StudioError("unavailable", status=502)

    monkeypatch.setattr(imports, "_mtproto", receive)
    assert client.post("/api/imports/telegram", json={"url": CASES[0]["url"]}).status_code == 502
    assert not list(settings.get().media_dir.iterdir())
    assert not store.list_drafts()


def test_widget_never_treats_placeholder_as_text():
    w = Widget("simple_agi/203")
    w.feed(
        '<div class="tgme_widget_message" data-post="simple_agi/203"><div class="message_media_not_supported">Please open Telegram</div></div>'
    )
    assert w.matched and w.unsupported and not w.root
    w = Widget("mcoadvanced/3450")
    w.feed(
        '<div class="tgme_widget_message" data-post="mcoadvanced/2"><div class="tgme_widget_message_text">Wrong post</div></div>'
    )
    assert not w.matched and not w.root


def test_widget_formatting_drops_scripts_and_unsafe_links():
    w = Widget("simple_agi/203")
    w.feed(
        '<div class="tgme_widget_message" data-post="simple_agi/203"><div class="tgme_widget_message_text">A<b>bold</b><br><a href="javascript:alert(1)">bad</a><script>secret()</script></div></div>'
    )
    html, _, _ = representations([{"type": "paragraph", "text": w.root}], [])
    assert "<b>bold</b>" in html and "<br>" in html
    assert "javascript:" not in html and "secret()" not in html


def test_rich_converter_preserves_nested_blocks_and_reports_unknown(client):
    c = Converter({"7": {"type": "photo", "media": "upload:photo.jpg"}})
    blocks = c.blocks(
        [
            types.PageBlockHeading2(types.TextBold(types.TextPlain("Heading"))).to_dict(),
            types.PageBlockDetails(
                [types.PageBlockParagraph(types.TextPlain("inside"))], types.TextPlain("Open"), open=True
            ).to_dict(),
            types.PageBlockPhoto(
                7, types.PageCaption(types.TextPlain("caption"), types.TextPlain("credit")), spoiler=True
            ).to_dict(),
            types.PageBlockTable(
                types.TextPlain("table"),
                [types.PageTableRow([types.PageTableCell(text=types.TextPlain("cell"), header=True)])],
                bordered=True,
            ).to_dict(),
            {"_": "FutureBlock"},
        ]
    )
    assert blocks[0]["text"]["type"] == "bold"
    assert blocks[1]["blocks"][0]["text"] == "inside"
    assert blocks[2]["photo"]["has_spoiler"]
    assert c.warnings and "FutureBlock" in c.warnings[0]
    errors = spec.validate_rich_message(spec.current, {"blocks": blocks})
    assert errors["ok"], errors
    html, markdown, media = representations(blocks, c.warnings)
    assert "<details open>" in html and "<table bordered>" in html
    assert "tg://photo?id=import1" in html and media[0]["media"]["media"] == "upload:photo.jpg"
    assert markdown and "caption" in html


def test_utf16_entities_and_html_escaping():
    warnings = []
    result = entities_text("😀bold<script>", [{"type": "bold", "offset": 2, "length": 4}], warnings)
    assert plain(result) == "😀bold<script>"
    html, _, _ = representations([{"type": "paragraph", "text": result}], warnings)
    assert "<b>bold</b>" in html and "&lt;script&gt;" in html
    assert not warnings


def test_scoped_session_never_inherits_owner(monkeypatch):
    owner_path = imports.session_path()
    owner_path.write_text("owner secret")
    for uid in (111, 222):
        with accounts.scope(uid):
            assert imports.session_path() != owner_path
            assert not imports.status()["own_session"]


def test_forward_is_armed_by_same_user_and_keeps_other_account_out(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "updates_on", True)
    for uid in (111, 222):
        store.accounts.save({"id": uid, "first_name": "User"})
        accounts.initialize(uid)
    with accounts.scope(111):
        ticket = import_forward.begin(CASES[0]["url"])

    def event(sender, **fields):
        return {
            "message": {"message_id": 90, "from": {"id": sender}, "chat": {"id": sender, "type": "private"}, **fields}
        }

    arm = "/start import_" + ticket["id"]
    assert not import_forward.receive(event(222, text=arm))
    forward = {
        "forward_origin": {"type": "channel", "chat": {"username": "simple_agi"}, "message_id": 203},
        "text": "Imported",
    }
    assert not import_forward.receive(event(111, **forward))
    assert import_forward.receive(event(111, text=arm))
    assert import_forward.receive(event(111, **forward))
    assert not import_forward.receive(event(111, **forward))  # one use
    with accounts.scope(111):
        assert import_forward.get(ticket["id"])["state"] == "done"
        assert len(store.list_drafts()) == 1
    with accounts.scope(222):
        assert not store.list_drafts()
        assert import_inbox.get(ticket["id"]) is None
    assert not store.list_drafts()


def test_mtproto_fetches_full_message_before_import(client, monkeypatch):
    peer = types.Channel(8, "Channel", types.ChatPhotoEmpty(), None, username="simple_agi")
    empty = types.RichMessage([], [], [], part=True)
    full = types.RichMessage([types.PageBlockParagraph(types.TextPlain("Complete"))], [], [])
    initial = types.Message(203, types.PeerChannel(8), None, "", rich_message=empty)
    complete = types.Message(203, types.PeerChannel(8), None, "", rich_message=full)

    class Fake:
        async def get_entity(self, ref):
            return peer

        async def get_messages(self, peer, ids):
            return initial

        async def __call__(self, request):
            assert request.id == 203
            return types.messages.Messages([complete], [], [], [])

    @asynccontextmanager
    async def connection(session, uid):
        assert not session and uid is None
        yield Fake(), True

    monkeypatch.setattr(import_client, "connection", connection)
    blocks, warnings = asyncio.run(imports._mtproto(parse_link(CASES[0]["url"]), []))
    assert blocks == [{"type": "paragraph", "text": "Complete"}]
    assert not warnings


def test_forward_update_only_acknowledges_in_own_private_chat(client, monkeypatch, tg):
    monkeypatch.setattr(settings.get(), "updates_on", True)
    ticket = import_forward.begin(CASES[0]["url"])
    base = {"message_id": 1, "from": {"id": 777}, "chat": {"id": 777, "type": "private"}}
    assert (
        asyncio.run(updates.handle_update({"message": {**base, "text": "/start import_" + ticket["id"]}})) == "import"
    )
    assert (
        asyncio.run(
            updates.handle_update({"message": {**base, "text": "Hello", "forward_origin": {"type": "hidden_user"}}})
        )
        == "import"
    )
    assert len(tg.calls) == 2
    assert all(c["method"] == "sendMessage" and c["body"]["chat_id"] == 777 for c in tg.calls)
    assert "перешлите" in tg.calls[0]["body"]["text"]
    assert "обработана" in tg.calls[1]["body"]["text"]


def test_import_requires_draft_permission(client):
    from test_access import token

    headers = token("reader", "read")
    assert client.get("/api/imports/telegram", headers=headers).status_code == 200
    for route in ("/api/imports/telegram", "/api/imports/forward"):
        assert client.post(route, headers=headers, json={"url": CASES[0]["url"]}).status_code == 403


def test_mcp_import_uses_same_service(client, monkeypatch, tg):
    from test_mcp import call

    monkeypatch.setattr(settings.get(), "import_api_id", 1)
    monkeypatch.setattr(settings.get(), "import_api_hash", "test")

    async def receive(link, saved):
        return [{"type": "paragraph", "text": "From MCP"}], ["Check source"]

    monkeypatch.setattr(imports, "_mtproto", receive)
    result, data, _ = call(client, "import_telegram_post", url=CASES[0]["url"])
    assert not result.get("isError") and data["warnings"] == ["Check source"]
    assert store.get_draft(data["draft"]["id"]) and not tg.calls


def test_import_media_always_denies_private_addresses(monkeypatch):
    from services import media

    monkeypatch.setattr(settings.get(), "media_fetch_private", True)

    async def resolve(host, port):
        return ["127.0.0.1"]

    monkeypatch.setattr(media, "_resolve", resolve)
    with pytest.raises(StudioError):
        asyncio.run(media._public_only("public.example", 443, force=True))


def test_download_limit_cleans_partial_file(monkeypatch):
    monkeypatch.setattr(settings.get(), "max_upload_mb", 0.00001)

    class Fake:
        async def iter_download(self, item):
            yield b"12345"
            yield b"x" * 100

    photo = types.Photo(1, 2, b"", None, [], 1)
    saved = []
    with pytest.raises(StudioError):
        asyncio.run(imports._download(Fake(), photo, saved))
    assert not saved and not list(settings.get().media_dir.iterdir())


def test_private_link_without_own_session_never_connects(monkeypatch):
    def unexpected(*args):
        pytest.fail("Private source reached the shared connection")

    monkeypatch.setattr(import_client, "connection", unexpected)
    with pytest.raises(StudioError, match="собственн"):
        asyncio.run(imports._mtproto(parse_link("https://t.me/c/12345/203"), []))


def test_session_identity_checked_before_read(monkeypatch):
    class Me:
        id = 999
        bot = False

    class Fake:
        async def connect(self):
            pass

        async def disconnect(self):
            pass

        async def is_user_authorized(self):
            return True

        async def get_me(self):
            return Me()

    monkeypatch.setattr(import_client, "make_client", lambda session: Fake())

    async def read():
        async with import_client.connection("session", 111):
            pytest.fail("Cross-account session was accepted")

    with pytest.raises(StudioError, match="другой учётке"):
        asyncio.run(read())


def test_different_forward_fails_without_creating_draft(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "updates_on", True)
    ticket = import_forward.begin(CASES[0]["url"])
    import_inbox.arm(ticket["id"], 777)
    message = {
        "from": {"id": 777},
        "chat": {"id": 777, "type": "private"},
        "text": "Wrong",
        "forward_origin": {"type": "channel", "chat": {"username": "different"}, "message_id": 203},
    }
    assert import_forward.receive({"message": message})
    assert import_forward.get(ticket["id"])["state"] == "failed"
    assert not store.list_drafts()


def test_forward_warning_is_persisted_and_buttons_removed(client):
    result = imports.from_forward(
        {"rich_message": {"blocks": [{"type": "buttons", "buttons": []}]}},
        parse_link(CASES[0]["url"]),
        ["Origin not verified"],
    )
    saved = store.get_draft(result["draft"]["id"])["payload"]
    assert saved["_studio"]["import_warnings"][0] == "Origin not verified"
    assert saved["rich_message"]["blocks"][0]["type"] == "paragraph"


def test_forward_database_limit_finishes_ticket_with_error(client, monkeypatch):
    from errors import StorageFull

    monkeypatch.setattr(settings.get(), "updates_on", True)
    ticket = import_forward.begin(CASES[0]["url"])
    import_inbox.arm(ticket["id"], 777)

    def full(*args):
        raise StorageFull()

    monkeypatch.setattr(imports, "from_forward", full)
    message = {
        "from": {"id": 777},
        "chat": {"id": 777, "type": "private"},
        "text": "Forwarded",
        "forward_origin": {
            "type": "channel",
            "chat": {"username": parse_link(CASES[0]["url"]).username},
            "message_id": parse_link(CASES[0]["url"]).message_id,
        },
    }
    assert import_forward.receive({"message": message})
    result = import_forward.get(ticket["id"])
    assert result["state"] == "failed" and "Лимит базы" in result["error"]
    assert not store.list_drafts()


def test_configuration_is_optional_and_validated():
    assert not settings.Settings.from_env({}).import_api_id
    with pytest.raises(settings.SettingsError, match="задаются вместе"):
        settings.Settings.from_env({"TG_IMPORT_API_ID": "123"})
    with pytest.raises(settings.SettingsError, match="TG_IMPORT_PROXY_URL"):
        settings.Settings.from_env({"TG_IMPORT_PROXY_URL": "file:///secret"})
