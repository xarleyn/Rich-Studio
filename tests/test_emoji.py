"""Emoji packs stay account-local, reuse the Bot API only, and cache pictures outside media/."""

import settings
from mock_telegram import EMOJI_FILES, EMOJI_SET, WEBP_1PX
from test_access import token
from test_public_accounts import login_as

SET = EMOJI_SET["name"]
ID1, ID2, ID3 = (st["custom_emoji_id"] for st in EMOJI_SET["stickers"])
LINK = "https://t.me/addemoji/" + SET


def add(client, name=SET):
    return client.post("/api/emoji/packs", json={"name": name})


def test_pack_add_list_remove_roundtrip(client, tg):
    result = add(client)
    assert result.status_code == 200, result.text
    added = result.json()["added"]
    assert added["short_name"] == SET and added["title"] == "RichPost Test"
    assert [s["emoji"] for s in added["stickers"]] == ["🚀", "🔥", "🎉"]
    assert tg.last("getStickerSet")["body"] == {"name": SET}
    assert [p["short_name"] for p in client.get("/api/emoji/packs").json()["packs"]] == [SET]
    # the t.me link normalizes to the same short name: no duplicate
    assert add(client, LINK).status_code == 200
    assert [p["short_name"] for p in client.get("/api/emoji/packs").json()["packs"]] == [SET]
    assert client.delete(f"/api/emoji/packs/{SET}").json()["packs"] == []
    assert client.delete(f"/api/emoji/packs/{SET}").status_code == 404
    assert not any(c["method"].startswith("send") for c in tg.calls)


def test_pack_add_errors_never_touch_the_channel(client, tg):
    assert add(client, "no_such_set_emoji").status_code == 404
    assert add(client, "x" * 100).status_code == 422
    assert add(client, "../etc/passwd").status_code == 422
    assert add(client, "").status_code == 422
    # a real sticker set, not custom emoji: refused with an explanation
    tg.queue["getStickerSet"] = [
        {
            "ok": True,
            "result": {"name": "cats", "title": "Cats", "sticker_type": "sticker", "stickers": []},
        }
    ]
    bad = add(client, "cats")
    assert bad.status_code == 422 and "не набор кастомных эмодзи" in bad.json()["detail"]
    assert client.get("/api/emoji/packs").json()["packs"] == []
    assert not any(c["method"].startswith("send") for c in tg.calls)


def test_pictures_download_once_and_are_cached(client, tg):
    add(client)

    def downloads():
        return len([c for c in tg.calls if c["method"] in ("getCustomEmojiStickers", "getFile")])

    static = client.get(f"/api/emoji/img/{ID1}")
    assert static.status_code == 200 and static.headers["content-type"].startswith("image/webp")
    assert static.content == WEBP_1PX
    assert "immutable" in static.headers["Cache-Control"]
    assert static.headers["Content-Security-Policy"] == "sandbox; default-src 'none'"
    mark = downloads()
    assert client.get(f"/api/emoji/img/{ID1}").content == WEBP_1PX
    assert client.get(f"/api/emoji/img/{ID2}").content == WEBP_1PX  # the video's thumbnail
    assert downloads() == mark + 2
    assert client.get(f"/api/emoji/img/{ID2}").status_code == 200
    assert downloads() == mark + 2
    video = client.get(f"/api/emoji/file/{ID2}")
    assert video.status_code == 200 and video.headers["content-type"].startswith("video/webm")
    anim = client.get(f"/api/emoji/file/{ID3}")
    assert anim.status_code == 200 and anim.headers["content-type"].startswith("application/gzip")
    assert video.content == EMOJI_FILES["emoji-file-2"][1] and anim.content == EMOJI_FILES["emoji-file-3"][1]
    assert client.get(f"/api/emoji/img/{'9' * 31}").status_code == 404
    assert client.get("/api/emoji/img/not-an-id").status_code == 404
    cache = list(settings.get().emoji_cache_dir.glob("*"))
    assert cache and all(p.parent.name == "emoji-cache" for p in cache)
    assert not list(settings.get().media_dir.glob("*"))


def test_pack_isolated_across_public_accounts(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "public_url", "https://testserver")
    client.cookies.clear()
    try:
        login_as(client, 101)
        assert add(client).status_code == 200
        assert client.get(f"/api/emoji/img/{ID1}").status_code == 200
        login_as(client, 202)
        assert client.get("/api/emoji/packs").json()["packs"] == []
        assert client.delete(f"/api/emoji/packs/{SET}").status_code == 404
    finally:
        client.cookies.clear()


def test_read_token_cannot_edit_packs(client):
    headers = token("reader", "read")
    assert client.get("/api/emoji/packs", headers=headers).status_code == 200
    assert client.post("/api/emoji/packs", headers=headers, json={"name": SET}).status_code == 403
    assert client.delete(f"/api/emoji/packs/{SET}", headers=headers).status_code == 403
