"""Templates stay account-local, preserve post data, and only create unpublished drafts."""

import copy
from html import unescape

import pytest

import settings
import store
from services import media
from store import templates as records
from test_access import call_as, token
from test_mcp import call
from test_public_accounts import login_as

FIELD = {"key": "name", "label": "Название", "type": "text", "required": True, "default": ""}
POST = {
    "rich_message": {
        "blocks": [
            {"type": "heading", "size": 1, "text": "{{name}}"},
            {"type": "paragraph", "text": {"type": "bold", "text": "О {{name}}"}},
            {"type": "photo", "photo": {"type": "photo", "media": "upload:cover.png"}},
        ],
        "future_flag": "keep",
    },
    "options": {"disable_notification": True},
    "reply_markup": {"inline_keyboard": [[{"text": "Открыть {{name}}", "url": "https://example.com"}]]},
    "future_option": 42,
    "_studio": {"buffers": {"json": "broken", "html": "stale"}},
}


def create(client, *, post=None, fields=None, title="Анонс"):
    result = client.post(
        "/api/templates",
        json={"title": title, "post": POST if post is None else post, "fields": [FIELD] if fields is None else fields},
    )
    assert result.status_code == 201, result.text
    return result.json()


def fill(client, template, values=None, **kwargs):
    return client.post(
        f"/api/templates/{template['id']}/drafts",
        json={
            "version": template["version"],
            "values": {"name": "Новый выпуск"} if values is None else values,
            **kwargs,
        },
    )


def test_template_fill_preserves_structures_without_publishing(client, tg):
    template = create(client)
    assert "_studio" not in template["payload"]
    assert template["version"] == 1
    assert client.get("/api/templates").json()[0]["fields"] == [FIELD]
    response = fill(client, template, {"name": "<тест> & {{literal}}"})
    assert response.status_code == 201, response.text
    draft = response.json()
    assert draft["post_id"] is None and not draft["chat_ids"]
    assert draft["payload"]["rich_message"]["blocks"][0]["text"] == "<тест> & {{literal}}"
    assert draft["payload"]["rich_message"]["blocks"][1]["text"]["text"] == "О <тест> & {{literal}}"
    assert draft["payload"]["reply_markup"]["inline_keyboard"][0][0]["text"] == "Открыть <тест> & {{literal}}"
    assert draft["payload"]["future_option"] == 42 and draft["payload"]["rich_message"]["future_flag"] == "keep"
    assert draft["payload"]["options"]["disable_notification"] is True
    assert fill(client, template).json()["id"] != draft["id"]
    assert client.get(f"/api/templates/{template['id']}").json() == template
    assert not tg.calls and not store.list_posts()


@pytest.mark.parametrize("format", ["html", "markdown"])
def test_values_are_literal_in_html_markdown_and_attributes(client, format):
    source = '<p>{{name}}</p><a href="{{link}}" title="{{name}}">link</a>'
    template = create(
        client, post={"rich_message": {format: source}}, fields=[FIELD, {"key": "link", "label": "URL", "type": "url"}]
    )
    response = fill(
        client, template, {"name": '"><img src=x onerror=alert(1)> **hi**', "link": "https://example.com/a(b)?x=1&y=2"}
    )
    assert response.status_code == 201, response.text
    result = response.json()["payload"]["rich_message"][format]
    assert "<img" not in result
    assert "&lt;img" in result if format == "html" else "&#60;img" in result
    decoded = unescape(result)
    assert 'href="https://example.com/a(b)?x=1&y=2"' in decoded
    assert "&#42;&#42;hi&#42;&#42;" in result if format == "markdown" else "**hi**" in result
    assert fill(client, template, {"name": "a", "link": "javascript:alert(1)"}).status_code == 422
    assert fill(client, template, {"name": "a", "link": 'https://example.com/" onerror="x'}).status_code == 422


@pytest.mark.parametrize(
    "html",
    [
        "<p title={{name}}>text</p>",
        "<{{name}}>text</{{name}}>",
        '<p {{name}}="value">text</p>',
        '<p onclick="{{name}}">text</p>',
        '<a href="{{name}}">text</a>',
    ],
)
def test_markup_structure_and_untyped_urls_are_not_template_fields(client, html):
    result = client.post(
        "/api/templates", json={"title": "bad", "post": {"rich_message": {"html": html}}, "fields": [FIELD]}
    )
    assert result.status_code == 422, result.text


def test_required_default_unknown_fields_and_url_validation(client):
    template = create(client)
    assert fill(client, template, {}).status_code == 422
    assert fill(client, template, {"name": " "}).status_code == 422
    assert fill(client, template, {"name": "a", "extra": "b"}).status_code == 422
    assert fill(client, template, {"name": "a" * 10001}).status_code == 422
    default = create(client, fields=[{**FIELD, "default": "По умолчанию"}])
    assert fill(client, default, {}).json()["payload"]["rich_message"]["blocks"][0]["text"] == "По умолчанию"
    optional = create(client, fields=[{**FIELD, "required": False}])
    assert fill(client, optional, {}).json()["payload"]["rich_message"]["blocks"][0]["text"] == ""


def test_duplicate_unused_missing_and_structural_variables_refused(client):
    for fields in ([], [FIELD, FIELD], [FIELD, {**FIELD, "key": "unused"}]):
        response = client.post("/api/templates", json={"title": "bad", "post": POST, "fields": fields})
        assert response.status_code == 422, response.text
    post = {"rich_message": {"blocks": [{"type": "paragraph", "{{name}}": "value"}]}}
    assert client.post("/api/templates", json={"title": "bad", "post": post, "fields": [FIELD]}).status_code == 422


@pytest.mark.parametrize(
    "source",
    [
        "`{{name}}`",
        "``a {{name}} b``",
        "```\n{{name}}\n```",
        "~~~\n{{name}}\n~~~",
        "    {{name}}",
        r"\\`{{name}}`",
        "> ~~~\n> {{name}}\n> ~~~",
        "- ~~~\n  {{name}}\n  ~~~",
        "> 1. ~~~~\n>    {{name}}\n>    ~~~~",
        "~~~\n- ~~~\n{{name}}\n~~~",
        "~~~\n~~~ not-a-closing-fence\n{{name}}\n~~~",
        ">     {{name}}",
        "-     {{name}}",
    ],
)
def test_markdown_code_fields_are_rejected_instead_of_corrupted(client, source):
    response = client.post(
        "/api/templates", json={"title": "Code", "post": {"rich_message": {"markdown": source}}, "fields": [FIELD]}
    )
    assert response.status_code == 422, response.text


@pytest.mark.parametrize(
    "source",
    [
        "$a+{{name}}$",
        "$$a+{{name}}$$",
        "$$\na+{{name}}\n$$",
        "> $$\n> {{name}}\n> $$",
        "- $$\n  {{name}}\n  $$",
        "$$a $ {{name}}$$",
        "$a+{{name}}$$",
    ],
)
def test_markdown_math_fields_are_rejected_instead_of_corrupted(client, source):
    response = client.post(
        "/api/templates", json={"title": "Math", "post": {"rich_message": {"markdown": source}}, "fields": [FIELD]}
    )
    assert response.status_code == 422, response.text
    assert "tg-math" in response.json()["detail"]


def test_escaped_dollars_and_html_math_fields_remain_available(client):
    for source in [r"\$a+{{name}}\$", "<tg-math>a+{{name}}</tg-math>", "$a$ {{name}} $b$", "$$a$$ {{name}} $$b$$"]:
        template = create(client, post={"rich_message": {"markdown": source}})
        assert fill(client, template).status_code == 201


def test_partial_upload_placeholder_is_refused_to_protect_default_media(client):
    post = copy.deepcopy(POST)
    post["rich_message"]["blocks"][2]["photo"]["media"] = "upload:{{name}}.png"
    response = client.post(
        "/api/templates", json={"title": "Partial", "post": post, "fields": [{**FIELD, "default": "cover"}]}
    )
    assert response.status_code == 422, response.text
    assert "всю ссылку" in response.json()["detail"]


def test_fragmented_defaults_cannot_hide_upload_reference_from_cleanup(client):
    post = copy.deepcopy(POST)
    post["rich_message"]["blocks"][2]["photo"]["media"] = "{{prefix}}{{name}}.png"
    response = client.post(
        "/api/templates",
        json={
            "title": "Fragmented",
            "post": post,
            "fields": [
                {**FIELD, "default": "cover"},
                {"key": "prefix", "label": "Prefix", "default": "upload:"},
            ],
        },
    )
    assert response.status_code == 422, response.text
    assert "всю ссылку" in response.json()["detail"]


def test_update_fill_and_delete_require_current_version(client):
    template = create(client)
    path = f"/api/templates/{template['id']}"
    body = {"title": "New", "post": POST, "fields": [FIELD]}
    assert client.put(path, json=body).status_code == 422
    new = client.put(path, json={**body, "version": template["version"]}).json()
    assert new["version"] == template["version"] + 1
    assert client.put(path, json={**body, "version": template["version"]}).status_code == 409
    assert fill(client, template).status_code == 409
    draft = fill(client, new).json()
    assert client.delete(path, params={"version": template["version"]}).status_code == 409
    assert client.delete(path, params={"version": new["version"]}).status_code == 200
    assert client.get(path).status_code == 404 and fill(client, new).status_code == 404
    assert client.get(f"/api/drafts/{draft['id']}").status_code == 200


def test_size_count_limits_and_expansion_limit(client, monkeypatch):
    monkeypatch.setattr(records, "MAX_TEMPLATES", 1)
    template = create(client)
    assert client.post("/api/templates", json={"title": "Another", "post": POST, "fields": [FIELD]}).status_code == 413
    monkeypatch.setattr(records, "MAX_TEMPLATES", 200)
    large = {"rich_message": {"html": "x" * (1024 * 1024)}}
    assert client.post("/api/templates", json={"title": "Huge", "post": large}).status_code == 413
    repeated = create(client, post={"rich_message": {"html": "{{name}}" * 200}})
    assert fill(client, repeated, {"name": "x" * 10000}).status_code == 413
    assert fill(client, template).status_code == 201


def test_expansion_is_measured_before_any_large_string_is_built(client, monkeypatch):
    from errors import StudioError
    from services import templates

    template = create(client, post={"rich_message": {"html": "{{name}}" * 120000}})
    regex = templates.VARIABLE

    class NoSubstitution:
        def search(self, text):
            return regex.search(text)

        def finditer(self, text):
            return regex.finditer(text)

        def sub(self, replace, text):
            pytest.fail("Expansion ran before the 1 MB limit was checked")

    monkeypatch.setattr(templates, "VARIABLE", NoSubstitution())
    with pytest.raises(StudioError, match="больше 1 МБ"):
        templates.create_draft(template["id"], {"name": "<&" * 5000}, template["version"])


def test_templates_keep_media_and_field_defaults_in_cleanup(client, monkeypatch):
    template = create(client)
    settings.get().media_dir.joinpath("cover.png").write_bytes(b"fake")
    assert "cover.png" in store.backup.upload_refs()
    assert records.templates_using("upload:cover.png") == [{"id": template["id"], "title": template["title"]}]
    assert any("шаблон" in label for label in media.used_by("cover.png"))
    assert media.delete_unused() == []
    assert client.delete("/api/media/cover.png").status_code == 409
    source = copy.deepcopy(POST)
    source["rich_message"]["blocks"][2]["photo"]["media"] = "{{file}}"
    create(client, post=source, fields=[FIELD, {"key": "file", "label": "Медиа", "default": "upload:default.png"}])
    settings.get().media_dir.joinpath("default.png").write_bytes(b"fake")
    assert "default.png" in store.backup.upload_refs() and media.delete_unused() == []


def test_read_tokens_cannot_write_templates_over_http_or_mcp(client):
    template = create(client)
    headers = token("reader", "read")
    assert client.get("/api/templates", headers=headers).status_code == 200
    assert client.post("/api/templates", headers=headers, json={"title": "X", "post": POST}).status_code == 403
    assert (
        client.post(f"/api/templates/{template['id']}/drafts", headers=headers, json={"version": 1}).status_code == 403
    )
    assert client.delete(f"/api/templates/{template['id']}?version=1", headers=headers).status_code == 403
    result, _ = call_as(
        client, headers, "draft_from_template", template_id=template["id"], version=1, values={"name": "x"}
    )
    assert result["isError"]


def test_templates_are_isolated_across_public_accounts(client, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", True)
    monkeypatch.setattr(settings.get(), "public_url", "https://testserver")
    client.cookies.clear()
    try:
        login_as(client, 101)
        first = create(client)
        login_as(client, 202)
        assert client.get("/api/templates").json() == []
        path = f"/api/templates/{first['id']}"
        assert client.get(path).status_code == 404
        assert fill(client, first).status_code == 404
        assert client.delete(path, params={"version": 1}).status_code == 404
        assert (
            client.put(path, json={"title": "attack", "post": POST, "fields": [FIELD], "version": 1}).status_code == 404
        )
        login_as(client, 101)
        assert client.get(path).json() == first
        assert fill(client, first).status_code == 201
    finally:
        client.cookies.clear()


def test_mcp_template_workflow_creates_a_draft_without_sending(client, tg):
    result, template, _ = call(client, "save_post_template", title="MCP", post=POST, fields=[FIELD])
    assert not result.get("isError"), result
    result, got, _ = call(client, "get_post_template", template_id=template["id"])
    assert got == template
    result, draft, _ = call(
        client, "draft_from_template", template_id=template["id"], version=1, values={"name": "MCP"}
    )
    assert not result.get("isError"), result
    assert draft["editor_url"].endswith("/?draft=" + draft["id"])
    assert store.get_draft(draft["id"])["payload"]["rich_message"]["blocks"][0]["text"] == "MCP"
    assert not tg.calls
