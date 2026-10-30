"""Search-facing routes must be public, canonical and separate from account data."""

import re
from xml.etree import ElementTree

import pytest

import public_site
import settings


@pytest.fixture
def marketing(client, monkeypatch):
    cfg = settings.get()
    monkeypatch.setattr(cfg, "public", True)
    monkeypatch.setattr(cfg, "landing_url", "https://example.com")
    monkeypatch.setattr(cfg, "public_url", "https://app.example.com")
    monkeypatch.setattr(cfg, "allowed_hosts", {*cfg.allowed_hosts, "example.com", "app.example.com"})
    return client


def test_sitemap_contains_only_public_canonical_pages(marketing):
    r = marketing.get("/sitemap.xml", headers={"Host": "example.com"})
    assert r.status_code == 200
    root = ElementTree.fromstring(r.content)  # noqa: S314 - trusted test output
    urls = {loc.text for loc in root.iter("{http://www.sitemaps.org/schemas/sitemap/0.9}loc")}
    assert urls == {"https://example.com" + path for path in public_site.PAGE_PATHS}
    for url in urls:
        path = url.removeprefix("https://example.com")
        response = marketing.get(path, headers={"Host": "example.com"})
        assert response.status_code == 200
        assert response.headers.get("x-robots-tag") is None
        assert f'<link rel="canonical" href="{url}">' in response.text
        assert response.text.count("<h1") == 1
        assert 'hreflang="ru"' in response.text and 'hreflang="en"' in response.text
        assert "__APP_URL__" not in response.text
        assert "script-src 'self'" in response.headers["content-security-policy"]


def test_languages_are_explicit_and_reciprocal(marketing):
    for path in ("/", "/features/telegram-tables/"):
        ru = marketing.get(path, headers={"Host": "example.com", "Accept-Language": "en"})
        en = marketing.get("/en" + path, headers={"Host": "example.com", "Accept-Language": "ru"})
        assert '<html lang="ru">' in ru.text and '<html lang="en">' in en.text
        assert re.findall(r'<link rel="alternate"[^>]+>', ru.text) == re.findall(
            r'<link rel="alternate"[^>]+>', en.text
        )
        assert '?lang=en"' in en.text
        assert not re.search("[а-яА-ЯёЁ]", en.text.replace('aria-label="Русская версия"', ""))


def test_app_and_local_previews_are_noindex(marketing):
    for path in ("/", "/en/", "/landing", "/landing/en/"):
        response = marketing.get(path, headers={"Host": "app.example.com"})
        assert response.status_code == 200
        assert response.headers["x-robots-tag"] == "noindex, nofollow"
    assert "data-nosnippet" in marketing.get("/", headers={"Host": "example.com"}).text
    private = marketing.get("/api/drafts", headers={"Host": "app.example.com"})
    assert private.status_code == 401 and private.headers["x-robots-tag"] == "noindex, nofollow"


def test_robots_allows_reading_noindex_and_advertises_only_marketing_sitemap(marketing):
    r = marketing.get("/robots.txt", headers={"Host": "example.com"})
    assert r.status_code == 200 and r.headers["content-type"].startswith("text/plain")
    assert "Allow: /" in r.text and "Sitemap: https://example.com/sitemap.xml" in r.text
    app = marketing.get("/robots.txt", headers={"Host": "app.example.com"})
    assert "Disallow: /" not in app.text and "Sitemap:" not in app.text
    assert marketing.get("/sitemap.xml", headers={"Host": "app.example.com"}).status_code == 404


def test_missing_marketing_pages_return_404_and_slashes_redirect(marketing):
    for path in (
        "/missing-page",
        "/mcp-missing",
        "/docs-missing",
        "/features/no-such-guide/",
        "/en/features/no-such-guide/",
    ):
        r = marketing.get(path, headers={"Host": "example.com"}, follow_redirects=False)
        assert r.status_code == 404 and "location" not in r.headers
    r = marketing.get("/en", headers={"Host": "example.com"}, follow_redirects=False)
    assert r.status_code in (307, 308) and r.headers["location"].endswith("/en/")
