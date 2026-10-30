"""Public agent onboarding stays discoverable, accurate and separate from account access."""

import json
import re
import tomllib
from urllib.parse import urlsplit

import pytest

import public_site
import settings
import store
from test_access import call_as
from test_account_tokens import bearer, issue
from test_api import BLOCKS_POST
from test_public_accounts import login_as


@pytest.fixture
def site(client, monkeypatch):
    client.cookies.clear()
    cfg = settings.get()
    monkeypatch.setattr(cfg, "public", True)
    monkeypatch.setattr(cfg, "landing_url", "https://example.com")
    monkeypatch.setattr(cfg, "public_url", "https://app.example.com")
    monkeypatch.setattr(cfg, "allowed_hosts", {*cfg.allowed_hosts, "example.com", "app.example.com", "mirror.example"})
    yield client
    client.cookies.clear()


@pytest.mark.parametrize("host", ["example.com", "app.example.com"])
@pytest.mark.parametrize("path", sorted(public_site.AGENT_PATHS))
def test_agent_documents_are_public_on_both_origins(site, host, path):
    response = site.get(path, headers={"Host": host}, follow_redirects=False)
    assert response.status_code == 200
    assert response.headers["content-type"].startswith("application/json" if path.endswith(".json") else "text/")
    assert "describedby" in response.headers["link"]
    assert response.headers["cache-control"] == "public, max-age=300"
    assert not response.text.startswith("\ufeff")
    assert not re.search(r"__[A-Z_]+__", response.text)
    head = site.head(path, headers={"Host": host})
    assert head.status_code == 200 and head.content == b""
    assert head.headers["content-type"] == response.headers["content-type"]
    assert "set-cookie" not in response.headers


def test_discovery_links_resolve_and_all_views_agree(site):
    text = site.get("/llms.txt").text
    assert text.startswith("# RichPost\n")
    urls = re.findall(r"\]\((https://(?:app\.)?example\.com[^)]+)\)", text)
    assert urls
    for url in urls:
        parts = urlsplit(url)
        assert site.get(parts.path, headers={"Host": parts.netloc}).status_code == 200
    catalog = site.get("/agents.json").json()
    assert catalog["site"]["url"] == "https://example.com"
    assert catalog["mcp"][0]["url"] == "https://app.example.com/mcp"
    assert catalog["mcp"][0]["type"] == "streamable-http"
    assert set(catalog) == {"$schema", "version", "standard", "site", "mcp", "skills"}
    directives = site.get("/agents.txt").text
    assert f"MCP: {catalog['mcp'][0]['url']}\n" in directives
    assert f"Skills: {catalog['skills'][0]['url']}\n" in directives
    full = site.get("/llms-full.txt").text
    assert site.get("/agents.md").text in full
    assert site.get("/skills/richpost/SKILL.md").text in full


def test_setup_examples_are_parseable_and_keep_credentials_out_of_config(site):
    guide = site.get("/agents.md").text
    codex = tomllib.loads(re.search(r"```toml\n(.*?)```", guide, re.S)[1])["mcp_servers"]["richpost"]
    claude = json.loads(re.search(r"```json\n(.*?)```", guide, re.S)[1])["mcpServers"]["richpost"]
    assert codex == {"url": "https://app.example.com/mcp", "bearer_token_env_var": "RICHPOST_TOKEN"}
    assert claude == {
        "type": "http",
        "url": codex["url"],
        "headers": {"Authorization": "Bearer ${RICHPOST_TOKEN}"},
    }
    for required in ("Required human step", "Токены API и MCP", "draft", "tools/list", "studio_status", "isError"):
        assert required in guide
    assert "Do not mint a token" in guide
    skill = site.get("/skills/richpost/SKILL.md").text
    assert skill.startswith("---\nname: richpost\ndescription:")
    assert "https://example.com/agents.md" in skill
    assert "Token issuance is a human step" in skill


def test_forwarded_host_cannot_rewrite_installation_destinations(site):
    forged = {"Host": "mirror.example", "X-Forwarded-Host": "attacker.invalid", "Forwarded": "host=attacker.invalid"}
    for path in public_site.AGENT_PATHS:
        response = site.get(path, headers=forged)
        assert response.status_code == 200
        assert response.content == site.get(path).content
        assert "attacker.invalid" not in response.text and "mirror.example" not in response.text
    assert site.get("/agents.md", headers={"Host": "attacker.invalid"}).status_code == 400


def test_docs_do_not_expose_secrets_or_account_data(site, monkeypatch):
    cfg = settings.get()
    secrets = ("operator-secret-sentinel", "999:BOT_SECRET_SENTINEL", "import-secret-sentinel")
    for key, value in zip(("api_key", "bot_token", "import_api_hash"), secrets, strict=True):
        monkeypatch.setattr(cfg, key, value)
    public_docs = {path: site.get(path).content for path in public_site.AGENT_PATHS}
    login_as(site)
    created = issue(site)
    site.post("/api/drafts", json={"title": "PRIVATE_ACCOUNT_SENTINEL", "post": BLOCKS_POST})
    for path, content in public_docs.items():
        assert site.get(path).content == content
        assert site.get(path, headers=bearer(created)).content == content
        assert all(value.encode() not in content for value in (*secrets, created["secret"], "PRIVATE_ACCOUNT_SENTINEL"))


def test_public_document_access_does_not_grant_mcp_or_token_management(site, tg):
    assert site.get("/api/drafts").status_code == 401
    assert site.post("/mcp", json={"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).status_code == 401
    login_as(site)
    created = issue(site)
    site.cookies.clear()
    result, _ = call_as(site, bearer(created), "studio_status")
    assert not result.get("isError")
    assert site.post("/api/account/tokens", headers=bearer(created), json={"name": "automatic"}).status_code == 403
    assert not store.list_drafts() and not store.list_posts()
    assert not tg.calls


def test_private_installation_documents_work_behind_operator_auth(site, monkeypatch):
    monkeypatch.setattr(settings.get(), "public", False)
    monkeypatch.setattr(settings.get(), "api_key", "private-studio-sentinel")
    monkeypatch.setattr(settings.get(), "landing_url", "")
    for path in public_site.AGENT_PATHS:
        assert site.get(path).status_code == 200
    guide = site.get("/agents.md").text
    assert "Ask the operator" in guide
    assert "private-studio-sentinel" not in guide
    assert site.get("/api/drafts").status_code == 401


def test_agent_routes_are_exact_not_a_file_server(site):
    for path in (
        "/skills/other/SKILL.md",
        "/skills/richpost/AGENTS.md",
        "/bundled/agent-docs/setup.md",
        "/agents.md/extra",
    ):
        response = site.get(path, headers={"Host": "example.com"}, follow_redirects=False)
        assert response.status_code == 404
    for path in ("/skills/richpost/../../settings.py", "/skills/richpost/%2e%2e/%2e%2e/settings.py"):
        assert site.get(path, headers={"Host": "example.com"}).status_code == 404


def test_human_pages_expose_setup_links_and_markdown_discovery(site):
    for prefix in ("", "/en"):
        home = site.get(prefix + "/", headers={"Host": "example.com"})
        assert f'href="{prefix}/features/ai-agents/"' in home.text
        page = site.get(prefix + "/features/ai-agents/", headers={"Host": "example.com"})
        assert page.status_code == 200
        assert 'rel="describedby" href="/llms.txt" type="text/markdown"' in page.text
        assert 'href="https://example.com/agents.md"' in page.text
        assert 'href="/skills/richpost/SKILL.md"' in page.text
